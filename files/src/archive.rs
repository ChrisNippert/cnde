//! Compress and Extract, by running `bsdtar` (libarchive) as a child
//! process on a worker thread. Nothing here goes through a shell, and an
//! archive is never opened inside another: only the one asked for is
//! unpacked, and what it holds stays as it is.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use rustix::fs::{CWD, RenameFlags, renameat_with};

use smithay_client_toolkit::reexports::calloop::channel::Sender;

use crate::{
    App, Event,
    folder::Location,
    transfer::{self, Msg, Outcome, Strip},
};

/// More entries than this and the archive is refused.
const MAX_ENTRIES: u64 = 100_000;
/// Listing an archive may take this long.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Free space that must be left over after extracting.
const MARGIN: u64 = 64 << 20;
/// Writing this much more than the listing declared aborts the extract.
const SLACK: u64 = 16 << 20;
const POLL: Duration = Duration::from_millis(100);
/// Looking at how much has been written is a walk; do it this often.
const SIZE_EVERY: Duration = Duration::from_millis(400);

/// Whether `bsdtar` is installed. Looked up once.
pub fn available() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| crate::ops::on_path("bsdtar"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    TarGz,
    TarZst,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Zip, Format::TarGz, Format::TarZst];

    pub fn ext(self) -> &'static str {
        match self {
            Format::Zip => ".zip",
            Format::TarGz => ".tar.gz",
            Format::TarZst => ".tar.zst",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Zip => "Zip (.zip)",
            Format::TarGz => "Tar, gzip (.tar.gz)",
            Format::TarZst => "Tar, zstd (.tar.zst)",
        }
    }
}

const EXTS: [&str; 14] = [
    ".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tar.lz4", ".tar", ".tgz", ".txz", ".tbz2",
    ".tzst", ".zip", ".7z", ".rar", ".jar",
];

/// The archive extension `name` ends in, if it is one we unpack.
fn archive_ext(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    EXTS.iter()
        .copied()
        .find(|e| lower.ends_with(e) && lower.len() > e.len())
}

pub fn is_archive(name: &str) -> bool {
    archive_ext(name).is_some()
}

/// `name` without its archive extension.
pub fn stem(name: &str) -> &str {
    match archive_ext(name) {
        Some(e) => &name[..name.len() - e.len()],
        None => name,
    }
}

/// `stem` plus `ext`, or `stem (2)ext`, `stem (3)ext`… until `taken` says no.
pub fn numbered(stem: &str, ext: &str, taken: impl Fn(&str) -> bool) -> String {
    let first = format!("{stem}{ext}");
    if !taken(&first) {
        return first;
    }
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !taken(c))
        .unwrap_or_default()
}

/// What the archive is called when `names` are compressed.
pub fn default_stem(names: &[OsString]) -> String {
    match names {
        [one] => one.to_string_lossy().into_owned(),
        _ => "Archive".into(),
    }
}

// ---- listing and the checks on it ----

/// What a listing says is inside.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: u64,
    /// Uncompressed bytes of everything, as declared.
    pub bytes: u64,
}

/// A name from the archive that could write outside the folder it is
/// extracted into: absolute, or with a `..` in it.
pub fn unsafe_path(name: &str) -> bool {
    let p = Path::new(name);
    p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Read a `bsdtar -tvf` listing: how many entries and how many bytes. The
/// size is the fifth field of the `ls -l` style line.
pub fn parse_verbose<'a>(lines: impl Iterator<Item = &'a str>) -> Listing {
    let mut out = Listing::default();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        out.entries += 1;
        // Links, folders and devices carry no data of their own.
        if !line.starts_with('-') {
            continue;
        }
        let size = line
            .split_whitespace()
            .nth(4)
            .and_then(|s| s.parse::<u64>().ok());
        out.bytes = out.bytes.saturating_add(size.unwrap_or(0));
    }
    out
}

/// Why the listing can't be unpacked here, if it can't.
pub fn check(names: &[String], list: &Listing, free: u64) -> Result<(), String> {
    if list.entries > MAX_ENTRIES || names.len() as u64 > MAX_ENTRIES {
        return Err(format!(
            "Too many files in the archive ({MAX_ENTRIES} at most)"
        ));
    }
    if let Some(bad) = names.iter().find(|n| unsafe_path(n)) {
        return Err(format!("The archive has an unsafe path (“{bad}”)"));
    }
    if list.bytes > free.saturating_sub(MARGIN) {
        return Err(format!(
            "Not enough room: it unpacks to {} and {} is free",
            human(list.bytes),
            human(free)
        ));
    }
    Ok(())
}

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut u) = (n as f64, 0);
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn free_space(dir: &Path) -> u64 {
    rustix::fs::statvfs(dir)
        .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
        .unwrap_or(0)
}

/// The most written to an extract before it is stopped.
fn write_limit(declared: u64) -> u64 {
    declared.saturating_add(declared / 10).saturating_add(SLACK)
}

// ---- running bsdtar ----

enum Fail {
    Cancel,
    Msg(String),
}

fn bsdtar() -> Command {
    let mut c = Command::new("bsdtar");
    c.stdin(Stdio::null());
    c
}

fn first_line(text: &str) -> String {
    match text.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(l) => l.trim_start_matches("bsdtar: ").to_string(),
        None => "bsdtar failed".into(),
    }
}

/// Collect a child's stderr (a few KB of it) on a thread.
fn drain_stderr(child: &mut Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.take(8192).read_to_end(&mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
    }
    rx
}

fn kill(child: &mut Child) {
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    tracing::debug!(pid, "killed bsdtar");
}

/// Lines of `bsdtar -t…` output on the archive, within the time limit and
/// the entry limit. The child is killed when it overruns.
fn list(mode: &str, archive: &Path, cancel: &AtomicBool) -> Result<Vec<String>, Fail> {
    let mut child = bsdtar()
        .arg(mode)
        .arg("-f")
        .arg(archive)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Fail::Msg(format!("Can't run bsdtar: {e}")))?;
    let stdout = child.stdout.take();
    let errs = drain_stderr(&mut child);
    let (tx, rx) = mpsc::channel::<Result<Vec<String>, ()>>();
    std::thread::spawn(move || {
        let mut lines = Vec::new();
        if let Some(out) = stdout {
            // Names may not be UTF-8; show them lossily.
            for raw in BufReader::new(out).split(b'\n') {
                let Ok(raw) = raw else { break };
                lines.push(String::from_utf8_lossy(&raw).into_owned());
                if lines.len() as u64 > MAX_ENTRIES + 1 {
                    let _ = tx.send(Err(()));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(lines));
    });
    let start = Instant::now();
    let got = loop {
        match rx.recv_timeout(POLL) {
            Ok(r) => break r,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(Vec::new()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancel.load(Ordering::Relaxed) {
            kill(&mut child);
            return Err(Fail::Cancel);
        }
        if start.elapsed() > LIST_TIMEOUT {
            kill(&mut child);
            return Err(Fail::Msg("Reading the archive took too long".into()));
        }
    };
    let Ok(lines) = got else {
        kill(&mut child);
        return Err(Fail::Msg(format!(
            "Too many files in the archive ({MAX_ENTRIES} at most)"
        )));
    };
    let status = child.wait().map_err(|e| Fail::Msg(e.to_string()))?;
    if !status.success() {
        let text = errs
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default();
        return Err(Fail::Msg(format!(
            "Can't read the archive: {}",
            first_line(&text)
        )));
    }
    Ok(lines)
}

/// Run `cmd` to its end, calling `watch` while it works; an `Err` from
/// `watch`, or the cancel flag, kills it.
fn run_child(
    mut cmd: Command,
    cancel: &AtomicBool,
    mut watch: impl FnMut() -> Result<(), String>,
) -> Result<(), Fail> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Fail::Msg(format!("Can't run bsdtar: {e}")))?;
    let errs = drain_stderr(&mut child);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                let text = errs
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap_or_default();
                return Err(Fail::Msg(first_line(&text)));
            }
            Ok(None) => {}
            Err(e) => return Err(Fail::Msg(e.to_string())),
        }
        if cancel.load(Ordering::Relaxed) {
            kill(&mut child);
            return Err(Fail::Cancel);
        }
        if let Err(m) = watch() {
            kill(&mut child);
            return Err(Fail::Msg(m));
        }
        std::thread::sleep(POLL);
    }
}

// ---- moving results into place ----

static SEQ: AtomicU32 = AtomicU32::new(0);

fn scratch_name(tag: &str) -> String {
    format!(
        ".{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Rename `from` to a free `stem`+`ext` in `dir`, never replacing anything.
fn place(from: &Path, dir: &Path, stem: &str, ext: &str) -> Result<PathBuf, String> {
    for _ in 0..100 {
        let name = numbered(stem, ext, |n| fs::symlink_metadata(dir.join(n)).is_ok());
        let to = dir.join(&name);
        match renameat_with(CWD, from, CWD, &to, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(to),
            Err(e) if e == rustix::io::Errno::EXIST => {}
            Err(e) => return Err(format!("Can't put “{name}” in place: {e}")),
        }
    }
    Err("No free name".into())
}

fn outcome(src: PathBuf, made: Result<PathBuf, Fail>) -> Outcome {
    let mut out = Outcome {
        mode: transfer::Mode::Copy,
        made: Vec::new(),
        error: None,
        cancelled: false,
    };
    match made {
        Ok(to) => out.made.push((src, to)),
        Err(Fail::Cancel) => out.cancelled = true,
        Err(Fail::Msg(m)) => out.error = Some(m),
    }
    out
}

// ---- compress ----

/// Compress `names` (in `dir`) into a new archive next to them.
pub fn compress(dir: &Path, names: &[OsString], format: Format, cancel: &AtomicBool) -> Outcome {
    let src = dir.join(names.first().map(OsString::as_os_str).unwrap_or_default());
    let tmp = dir.join(format!("{}{}", scratch_name("compressing"), format.ext()));
    let mut cmd = bsdtar();
    // -a picks the format from the suffix of the output name.
    cmd.args(["-a", "-cf"])
        .arg(&tmp)
        .arg("-C")
        .arg(dir)
        .arg("--");
    for n in names {
        if n.as_bytes().first() == Some(&b'-') {
            let mut dotted = OsString::from("./");
            dotted.push(n);
            cmd.arg(dotted);
        } else {
            cmd.arg(n);
        }
    }
    let done = run_child(cmd, cancel, || Ok(()))
        .and_then(|()| place(&tmp, dir, &default_stem(names), format.ext()).map_err(Fail::Msg));
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    outcome(src, done)
}

// ---- extract ----

/// Unpack `archive` into a new folder in its own directory. A single
/// top-level folder inside is used as it is instead of being nested.
pub fn extract(
    archive: &Path,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&str, u64, u64),
) -> Outcome {
    let result = extract_inner(archive, cancel, report);
    outcome(archive.to_path_buf(), result)
}

fn extract_inner(
    archive: &Path,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&str, u64, u64),
) -> Result<PathBuf, Fail> {
    let dir = archive.parent().unwrap_or(Path::new("."));
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Err(Fail::Cancel);
    }
    let names = list("-t", archive, cancel)?;
    let verbose = list("-tv", archive, cancel)?;
    let listing = parse_verbose(verbose.iter().map(String::as_str));
    check(&names, &listing, free_space(dir)).map_err(Fail::Msg)?;

    let tmp = dir.join(scratch_name("extracting"));
    fs::create_dir(&tmp).map_err(|e| Fail::Msg(format!("Can't make a folder here: {e}")))?;
    let limit = write_limit(listing.bytes);
    let mut cmd = bsdtar();
    // No -P: bsdtar refuses `..` and symlinks leading out of the folder.
    cmd.args(["-x", "--no-same-owner", "--no-same-permissions", "-f"])
        .arg(archive)
        .arg("-C")
        .arg(&tmp);
    let mut last = Instant::now() - SIZE_EVERY;
    let ran = run_child(cmd, cancel, || {
        if last.elapsed() >= SIZE_EVERY {
            last = Instant::now();
            let written = transfer::tree_size(&tmp);
            report(&name, written.min(listing.bytes), listing.bytes);
            if written > limit {
                return Err("The archive unpacks to more than it says; stopped".into());
            }
        }
        Ok(())
    });
    let placed = ran.and_then(|()| settle(&tmp, dir, stem(&name)).map_err(Fail::Msg));
    // Whatever is left: all of it after a failure, else the emptied folder.
    let _ = fs::remove_dir_all(&tmp);
    placed
}

/// Move the result of an extract out of `tmp`: its one top-level folder, or
/// `tmp` itself as a folder called `stem`.
fn settle(tmp: &Path, dir: &Path, stem: &str) -> Result<PathBuf, String> {
    let mut kids = fs::read_dir(tmp)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path());
    let (first, second) = (kids.next(), kids.next());
    if let (Some(only), None) = (&first, second)
        && fs::symlink_metadata(only).is_ok_and(|m| m.is_dir())
        && let Some(n) = only.file_name().and_then(OsStr::to_str)
    {
        return place(only, dir, n, "");
    }
    place(tmp, dir, stem, "")
}

// ---- the window side ----

/// What a worker does.
enum Job {
    Compress(PathBuf, Vec<OsString>, Format),
    Extract(PathBuf),
}

impl App {
    /// Compress the selection into the open folder.
    pub(crate) fn compress_selection(&mut self, format: Format) {
        let Location::Dir(dir) = self.ui.location.clone() else {
            return;
        };
        let names: Vec<OsString> = self
            .selected_paths()
            .iter()
            .filter_map(|p| p.file_name().map(OsStr::to_os_string))
            .collect();
        if names.is_empty() {
            return;
        }
        let label = format!("Compressing {}", default_stem(&names));
        self.start_archive_job(Job::Compress(dir, names, format), label);
    }

    /// Unpack the selected archive beside itself.
    pub(crate) fn extract_selection(&mut self) {
        let paths = self.selected_paths();
        let [path] = paths.as_slice() else {
            return;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !is_archive(&name) {
            return;
        }
        let label = format!("Extracting {name}");
        self.start_archive_job(Job::Extract(path.clone()), label);
    }

    /// Run `job` on a thread, with the transfer strip and its cancel.
    fn start_archive_job(&mut self, job: Job, label: String) {
        if !available() {
            self.show_error("bsdtar isn't installed".into());
            return;
        }
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        self.ui.x.xfer.strip = Some(Strip {
            name: label.clone(),
            done: 0,
            total: 0,
            since: Instant::now(),
            cancel: cancel.clone(),
        });
        let tx: Sender<Event> = self.events.clone();
        std::thread::spawn(move || {
            let mut report = |_: &str, done: u64, total: u64| {
                let _ = tx.send(Event::Xfer(Msg::Progress {
                    name: label.clone(),
                    done,
                    total,
                }));
            };
            let outcome = match job {
                Job::Compress(dir, names, format) => compress(&dir, &names, format, &cancel),
                Job::Extract(path) => extract(&path, &cancel, &mut report),
            };
            let _ = tx.send(Event::Xfer(Msg::Done(outcome)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names() {
        assert!(is_archive("a.zip"));
        assert!(is_archive("A.TAR.GZ"));
        assert!(is_archive("x.7z"));
        assert!(!is_archive("notes.txt"));
        assert!(!is_archive(".zip"));
        assert_eq!(stem("photos.tar.zst"), "photos");
        assert_eq!(stem("a.b.zip"), "a.b");
        assert_eq!(stem("plain"), "plain");
    }

    #[test]
    fn names_get_numbers() {
        let taken = |t: &'static [&'static str]| move |n: &str| t.contains(&n);
        assert_eq!(numbered("Archive", ".zip", taken(&[])), "Archive.zip");
        assert_eq!(
            numbered("Archive", ".zip", taken(&["Archive.zip"])),
            "Archive (2).zip"
        );
        assert_eq!(
            numbered("a", ".tar.gz", taken(&["a.tar.gz", "a (2).tar.gz"])),
            "a (3).tar.gz"
        );
        assert_eq!(numbered("dir", "", taken(&["dir"])), "dir (2)");
        assert_eq!(default_stem(&["x.txt".into()]), "x.txt");
        assert_eq!(default_stem(&["a".into(), "b".into()]), "Archive");
    }

    #[test]
    fn traversal_is_caught() {
        assert!(unsafe_path("/etc/passwd"));
        assert!(unsafe_path("../x"));
        assert!(unsafe_path("a/../../x"));
        assert!(!unsafe_path("a/b/c.txt"));
        assert!(!unsafe_path("./a/b"));
        assert!(!unsafe_path("a..b/c"));
    }

    #[test]
    fn listing_is_summed() {
        let text = "\
drwxr-xr-x  0 me     me          0 Jan  1 12:00 top/
-rw-r--r--  0 me     me       1000 Jan  1 12:00 top/a file.txt
-rw-r--r--  0 me     me        234 Jan  1 12:00 top/b
lrwxrwxrwx  0 me     me          0 Jan  1 12:00 top/l -> b
";
        let l = parse_verbose(text.lines());
        assert_eq!(
            l,
            Listing {
                entries: 4,
                bytes: 1234
            }
        );
        assert_eq!(parse_verbose([].into_iter()), Listing::default());
    }

    #[test]
    fn refusals() {
        let ok = vec!["a/b".to_string()];
        let small = Listing {
            entries: 1,
            bytes: 10,
        };
        assert!(check(&ok, &small, 1 << 30).is_ok());
        // Not enough room, counting the margin.
        let big = Listing {
            entries: 1,
            bytes: 1 << 30,
        };
        assert!(check(&ok, &big, 1 << 30).unwrap_err().contains("room"));
        let edge = Listing {
            entries: 1,
            bytes: MARGIN + 1,
        };
        assert!(check(&ok, &edge, MARGIN).is_err());
        assert!(check(&ok, &small, MARGIN + 10).is_ok());
        // Too many entries.
        let many = Listing {
            entries: MAX_ENTRIES + 1,
            bytes: 0,
        };
        assert!(
            check(&ok, &many, u64::MAX)
                .unwrap_err()
                .contains("Too many")
        );
        // Unsafe names.
        let bad = vec!["fine".to_string(), "../evil".to_string()];
        assert!(check(&bad, &small, 1 << 30).unwrap_err().contains("unsafe"));
        assert!(write_limit(0) >= SLACK);
        assert!(write_limit(1000) > 1000);
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cnde-archive-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip() {
        if !available() {
            return;
        }
        let d = temp("rt");
        let cancel = AtomicBool::new(false);
        fs::create_dir_all(d.join("proj/sub")).unwrap();
        fs::write(d.join("proj/sub/f.txt"), "hello").unwrap();
        fs::write(d.join("lone.txt"), "x").unwrap();
        // One folder: proj.zip, never replacing.
        for (fmt, want) in [(Format::Zip, "proj.zip"), (Format::TarGz, "proj.tar.gz")] {
            let out = compress(&d, &["proj".into()], fmt, &cancel);
            assert_eq!(out.error, None);
            assert_eq!(out.made[0].1, d.join(want));
        }
        let again = compress(&d, &["proj".into()], Format::Zip, &cancel);
        assert_eq!(again.made[0].1, d.join("proj (2).zip"));
        // Several: Archive.zip.
        let many = compress(
            &d,
            &["proj".into(), "lone.txt".into()],
            Format::Zip,
            &cancel,
        );
        assert_eq!(many.made[0].1, d.join("Archive.zip"));
        // A single top-level folder is not nested.
        let out = extract(&d.join("proj.zip"), &cancel, &mut |_, _, _| {});
        assert_eq!(out.error, None);
        assert_eq!(out.made[0].1, d.join("proj (2)"));
        assert_eq!(
            fs::read_to_string(d.join("proj (2)/sub/f.txt")).unwrap(),
            "hello"
        );
        // Two top-level entries go in a folder named after the archive.
        let out = extract(&d.join("Archive.zip"), &cancel, &mut |_, _, _| {});
        assert_eq!(out.made[0].1, d.join("Archive"));
        assert!(d.join("Archive/lone.txt").is_file());
        assert!(d.join("Archive/proj/sub/f.txt").is_file());
        // No scratch folders left behind.
        let leftovers = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(leftovers, 0);
        // A cancelled extract leaves nothing.
        let stop = AtomicBool::new(true);
        let out = extract(&d.join("Archive.zip"), &stop, &mut |_, _, _| {});
        assert!(out.cancelled && out.made.is_empty());
        // Not an archive: an error, not a crash.
        let out = extract(&d.join("lone.txt"), &cancel, &mut |_, _, _| {});
        assert!(out.error.is_some());
        let _ = fs::remove_dir_all(&d);
    }
}
