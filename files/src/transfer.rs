//! Copying and moving files on a worker thread, with progress and cancel,
//! and the strip at the bottom of the window that shows a long one.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{OpenOptionsExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use cnde_ui::{Color, Painter, Rect, Weight};
use rustix::io::Errno;
use smithay_client_toolkit::reexports::calloop::channel::Sender;

use crate::{
    App, Event,
    folder::Location,
    model,
    ops::Act,
    undo,
    view::{Colors, Painted, Target, Ui},
};

/// A transfer shorter than this never shows its strip.
const STRIP_DELAY: Duration = Duration::from_millis(300);
/// Progress goes to the window at most this often.
const REPORT_EVERY: Duration = Duration::from_millis(50);
const CHUNK: usize = 256 * 1024;
const STRIP_H: f32 = 44.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

/// What a transfer reports back, and other file work that ends in the window.
pub enum Msg {
    Progress {
        name: String,
        done: u64,
        total: u64,
    },
    Done(Outcome),
    /// Clipboard contents read from an offer (mime type, bytes).
    Pasted(String, Vec<u8>),
    /// Something dropped on the window, onto this folder.
    Dropped {
        dest: PathBuf,
        data: Vec<u8>,
        mode: Mode,
    },
    /// What a drag that just entered carries.
    DragPaths(Vec<PathBuf>),
    /// Something worth undoing happened.
    Record(undo::Action),
    /// An undo finished (a message, or why it failed).
    Undone(Result<String, String>),
}

/// How a transfer ended.
pub struct Outcome {
    pub mode: Mode,
    /// (source, new path) for everything that went through.
    pub made: Vec<(PathBuf, PathBuf)>,
    pub error: Option<String>,
    pub cancelled: bool,
}

/// The transfer in progress, for the strip.
pub struct Strip {
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub since: Instant,
    pub cancel: Arc<AtomicBool>,
}

/// The window state transfers keep.
#[derive(Default)]
pub struct State {
    pub strip: Option<Strip>,
    /// Names to select once the listing has them.
    pub select: Vec<String>,
}

enum Fail {
    Cancel,
    Io(io::Error),
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Io(e)
    }
}

/// `name` with " (copy)" before the extension, " (copy 2)" and so on until
/// it's free in `dir`. A name that is free stays as it is.
pub fn unique_path(dir: &Path, name: &OsStr, is_dir: bool) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let bytes = name.as_bytes();
    let dot = if is_dir {
        None
    } else {
        bytes
            .iter()
            .rposition(|&b| b == b'.')
            .filter(|&i| i > 0 && i + 1 < bytes.len())
    };
    let (stem, ext) = match dot {
        Some(i) => (&bytes[..i], &bytes[i..]),
        None => (bytes, &bytes[bytes.len()..]),
    };
    let stem = strip_copy_tag(stem);
    for n in 1u32.. {
        let tag = if n == 1 {
            " (copy)".to_string()
        } else {
            format!(" (copy {n})")
        };
        let mut out = stem.to_vec();
        out.extend_from_slice(tag.as_bytes());
        out.extend_from_slice(ext);
        let path = dir.join(OsString::from_vec(out));
        if fs::symlink_metadata(&path).is_err() {
            return path;
        }
    }
    unreachable!()
}

/// `a (copy)` and `a (copy 3)` are copies of `a`.
fn strip_copy_tag(stem: &[u8]) -> &[u8] {
    let Some(open) = stem
        .windows(7)
        .rposition(|w| w == b" (copy " || w == b" (copy)")
    else {
        return stem;
    };
    let tail = &stem[open..];
    let ok = tail == b" (copy)"
        || (tail.starts_with(b" (copy ")
            && tail.ends_with(b")")
            && tail[7..tail.len() - 1].iter().all(u8::is_ascii_digit)
            && tail.len() > 8);
    if ok { &stem[..open] } else { stem }
}

/// Bytes in every file under `path`.
pub fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        let Ok(rd) = fs::read_dir(path) else {
            return 0;
        };
        rd.flatten().map(|e| tree_size(&e.path())).sum()
    } else if meta.is_file() {
        meta.len()
    } else {
        0
    }
}

struct Run<'a> {
    cancel: &'a AtomicBool,
    done: u64,
    total: u64,
    last: Instant,
    name: String,
    report: &'a mut dyn FnMut(&str, u64, u64),
}

impl Run<'_> {
    fn tick(&mut self, force: bool) {
        if force || self.last.elapsed() >= REPORT_EVERY {
            self.last = Instant::now();
            (self.report)(&self.name, self.done, self.total);
        }
    }

    fn check(&self) -> Result<(), Fail> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Fail::Cancel)
        } else {
            Ok(())
        }
    }
}

/// `made` turns true once `dst` exists because of us: only then is it
/// ours to delete after a failure (something else may have taken the name
/// since it was picked).
fn copy_item(run: &mut Run<'_>, src: &Path, dst: &Path, made: &mut bool) -> Result<(), Fail> {
    run.check()?;
    let meta = fs::symlink_metadata(src)?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        symlink(fs::read_link(src)?, dst)?;
        *made = true;
    } else if kind.is_dir() {
        fs::create_dir(dst)?;
        *made = true;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_item(run, &entry.path(), &dst.join(entry.file_name()), &mut false)?;
        }
        // Last, so a read-only folder could still be filled.
        fs::set_permissions(dst, meta.permissions())?;
        if let (Ok(file), Ok(time)) = (fs::File::open(dst), meta.modified()) {
            let _ = file.set_modified(time);
        }
    } else if kind.is_file() {
        let mut from = fs::File::open(src)?;
        let mut to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(meta.permissions().mode() & 0o7777)
            .open(dst)?;
        *made = true;
        let mut buf = vec![0u8; CHUNK];
        loop {
            run.check()?;
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            to.write_all(&buf[..n])?;
            run.done += n as u64;
            run.tick(false);
        }
        if let Ok(time) = meta.modified() {
            let _ = to.set_modified(time);
        }
        fs::set_permissions(dst, meta.permissions())?;
    }
    Ok(())
}

fn remove_any(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Move `from` to exactly `to` (which must be free): a rename, or a copy and
/// delete across drives.
pub fn move_exact(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
            let cancel = AtomicBool::new(false);
            let mut report = |_: &str, _: u64, _: u64| {};
            let mut run = Run {
                cancel: &cancel,
                done: 0,
                total: 0,
                last: Instant::now(),
                name: String::new(),
                report: &mut report,
            };
            let mut made = false;
            match copy_item(&mut run, from, to, &mut made) {
                Ok(()) => remove_any(from),
                Err(Fail::Io(e)) => {
                    if made {
                        let _ = remove_any(to);
                    }
                    Err(e)
                }
                Err(Fail::Cancel) => unreachable!(),
            }
        }
        other => other,
    }
}

/// Copy or move `sources` into `dest`, never overwriting. `report` gets the
/// current name and bytes done of total.
pub fn run(
    sources: &[PathBuf],
    dest: &Path,
    mode: Mode,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&str, u64, u64),
) -> Outcome {
    let mut out = Outcome {
        mode,
        made: Vec::new(),
        error: None,
        cancelled: false,
    };
    let mut errors: Vec<String> = Vec::new();
    // What a rename couldn't do; copied (and for a move, deleted) below.
    let mut slow: Vec<&PathBuf> = Vec::new();
    let real_dest = fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    for src in sources {
        let Some(name) = src.file_name() else {
            continue;
        };
        let shown = name.to_string_lossy();
        let Ok(meta) = fs::symlink_metadata(src) else {
            errors.push(format!("“{shown}” isn't there any more"));
            continue;
        };
        if mode == Mode::Move && src.parent() == Some(dest) {
            continue;
        }
        if meta.is_dir() && fs::canonicalize(src).is_ok_and(|s| real_dest.starts_with(s)) {
            errors.push(format!("Can't put “{shown}” inside itself"));
            continue;
        }
        if mode == Mode::Move {
            let dst = unique_path(dest, name, meta.is_dir());
            match fs::rename(src, &dst) {
                Ok(()) => {
                    out.made.push((src.clone(), dst));
                    continue;
                }
                Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {}
                Err(e) => {
                    errors.push(format!("Can't move “{shown}”: {e}"));
                    continue;
                }
            }
        }
        slow.push(src);
    }

    let total = slow.iter().map(|s| tree_size(s)).sum();
    let mut state = Run {
        cancel,
        done: 0,
        total,
        last: Instant::now(),
        name: String::new(),
        report,
    };
    for src in slow {
        let Some(name) = src.file_name() else {
            continue;
        };
        let shown = name.to_string_lossy().into_owned();
        let is_dir = fs::symlink_metadata(src).is_ok_and(|m| m.is_dir());
        let dst = unique_path(dest, name, is_dir);
        state.name = shown.clone();
        state.tick(true);
        let mut made = false;
        match copy_item(&mut state, src, &dst, &mut made) {
            Ok(()) => {
                if mode == Mode::Move
                    && let Err(e) = remove_any(src)
                {
                    errors.push(format!(
                        "Copied “{shown}” but can't remove the original: {e}"
                    ));
                }
                out.made.push((src.clone(), dst));
            }
            Err(Fail::Cancel) => {
                if made {
                    let _ = remove_any(&dst);
                }
                out.cancelled = true;
                break;
            }
            Err(Fail::Io(e)) => {
                if made {
                    let _ = remove_any(&dst);
                }
                let verb = if mode == Mode::Move { "move" } else { "copy" };
                errors.push(format!("Can't {verb} “{shown}”: {e}"));
            }
        }
    }
    out.error = match errors.len() {
        0 => None,
        1 => errors.pop(),
        n => Some(format!("{} (and {} more problems)", errors[0], n - 1)),
    };
    out
}

/// Run a transfer on a thread; progress and the end arrive as events.
pub fn start(
    sources: Vec<PathBuf>,
    dest: PathBuf,
    mode: Mode,
    tx: Sender<Event>,
    cancel: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut report = |name: &str, done: u64, total: u64| {
            let _ = tx.send(Event::Xfer(Msg::Progress {
                name: name.to_string(),
                done,
                total,
            }));
        };
        let outcome = run(&sources, &dest, mode, &cancel, &mut report);
        let _ = tx.send(Event::Xfer(Msg::Done(outcome)));
    });
}

impl App {
    /// Begin copying or moving `sources` into `dest`.
    pub(crate) fn start_transfer(&mut self, sources: Vec<PathBuf>, dest: PathBuf, mode: Mode) {
        if sources.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let name = sources[0]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A second transfer replaces the strip; both still run.
        self.ui.x.xfer.strip = Some(Strip {
            name,
            done: 0,
            total: 0,
            since: Instant::now(),
            cancel: cancel.clone(),
        });
        start(sources, dest, mode, self.events.clone(), cancel);
    }

    /// Paste into the open folder, if it is one.
    fn open_dir_path(&mut self) -> Option<PathBuf> {
        match &self.ui.location {
            Location::Dir(p) => Some(p.clone()),
            _ => {
                self.show_error("Open a folder first".into());
                None
            }
        }
    }

    pub(crate) fn paste_into_open(&mut self, data: &[u8], mime: &str) {
        let Some(dest) = self.open_dir_path() else {
            return;
        };
        let (cut, paths) = crate::clipboard::parse_offer(mime, data);
        if paths.is_empty() {
            self.show_error("Nothing to paste".into());
            return;
        }
        let mode = if cut { Mode::Move } else { Mode::Copy };
        self.start_transfer(paths, dest, mode);
    }

    pub(crate) fn xfer_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Progress { name, done, total } => {
                if let Some(strip) = &mut self.ui.x.xfer.strip {
                    strip.name = name;
                    strip.done = done;
                    strip.total = total;
                    if strip.since.elapsed() > STRIP_DELAY {
                        self.redraw();
                    }
                }
            }
            Msg::Done(outcome) => {
                self.ui.x.xfer.strip = None;
                if let Some(err) = &outcome.error {
                    self.show_error(err.clone());
                }
                let into_open = |p: &PathBuf| match &self.ui.location {
                    Location::Dir(d) => p.parent() == Some(d.as_path()),
                    _ => false,
                };
                self.ui.x.xfer.select = outcome
                    .made
                    .iter()
                    .filter(|(_, to)| into_open(to))
                    .filter_map(|(_, to)| Some(to.file_name()?.to_string_lossy().into_owned()))
                    .collect();
                if !outcome.made.is_empty() {
                    self.record(match outcome.mode {
                        Mode::Copy => undo::Action::Copied(
                            outcome.made.iter().map(|(_, to)| to.clone()).collect(),
                        ),
                        Mode::Move => undo::Action::Moved(outcome.made.clone()),
                    });
                }
                self.refresh_dir();
                self.redraw();
            }
            Msg::Pasted(mime, data) => self.paste_into_open(&data, &mime),
            Msg::Dropped { dest, data, mode } => {
                let (_, paths) = crate::clipboard::parse_offer("text/uri-list", &data);
                self.start_transfer(paths, dest, mode);
            }
            Msg::DragPaths(paths) => self.drag_paths(paths),
            Msg::Record(action) => self.record(action),
            Msg::Undone(result) => {
                match result {
                    Ok(message) => self.show_notice(message),
                    Err(err) => self.show_error(err),
                }
                self.refresh_dir();
            }
        }
    }

    /// Select what a finished transfer made, once the listing has it.
    pub(crate) fn apply_pending_select(&mut self) {
        let names = std::mem::take(&mut self.ui.x.xfer.select);
        if names.is_empty() {
            return;
        }
        let f = &mut self.ui.folder;
        f.selected = names.into_iter().collect();
        let first = f
            .visible
            .iter()
            .position(|&i| f.selected.contains(&f.entries[i].name));
        f.cursor = first;
        f.anchor = first;
        if let Some(pos) = first {
            self.ensure_visible(pos);
        }
    }

    pub(crate) fn cancel_transfer(&mut self) {
        if let Some(strip) = &self.ui.x.xfer.strip {
            strip.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// The progress strip, along the bottom of the main area; its cancel button
/// is a hit. Only a transfer that has run a moment shows one.
pub fn paint_strip(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    w: f32,
    h: f32,
) {
    let Some(s) =
        ui.x.xfer
            .strip
            .as_ref()
            .filter(|s| s.since.elapsed() > STRIP_DELAY)
    else {
        return;
    };
    let t = p.theme;
    let x = crate::view::SIDE_W + 12.0;
    let width = w - crate::view::panel_w(ui) - x - 12.0;
    let y = h - STRIP_H - 12.0;
    p.canvas.rounded_rect(
        x,
        y,
        width,
        STRIP_H,
        12.0,
        Color::rgba(0x10, 0x10, 0x12, 0.96),
    );
    let ink = Color::rgb(0xf2, 0xf2, 0xf4);
    let size = t.font_size - 0.5;
    let amount = if s.total > 0 {
        format!(
            "{} of {}",
            model::human_size(s.done),
            model::human_size(s.total)
        )
    } else {
        String::new()
    };
    let amount_w = p.measure(&amount, size - 1.0, Weight::Normal);
    let name_w = (width * 0.30).min(260.0);
    p.label_v(
        &s.name,
        x + 16.0,
        y + STRIP_H / 2.0,
        name_w,
        size,
        Weight::Normal,
        ink,
    );
    let bx = x + 16.0 + name_w + 14.0;
    let bw = (width - (bx - x) - amount_w - 14.0 - 44.0).max(40.0);
    let frac = if s.total > 0 {
        (s.done as f32 / s.total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.canvas.rounded_rect(
        bx,
        y + STRIP_H / 2.0 - 3.0,
        bw,
        6.0,
        3.0,
        ink.with_alpha(0.2),
    );
    if frac > 0.0 {
        p.canvas.rounded_rect(
            bx,
            y + STRIP_H / 2.0 - 3.0,
            (bw * frac).max(6.0),
            6.0,
            3.0,
            t.accent,
        );
    }
    p.label_v(
        &amount,
        bx + bw + 14.0,
        y + STRIP_H / 2.0,
        amount_w + 2.0,
        size - 1.0,
        Weight::Normal,
        ink.with_alpha(0.7),
    );
    let cancel = Rect::new(x + width - 40.0, y + 4.0, 36.0, STRIP_H - 8.0);
    if ui.hover == Some(Target::Act(Act::CancelTransfer)) {
        p.canvas.rounded_rect(
            cancel.x,
            cancel.y,
            cancel.w,
            cancel.h,
            10.0,
            ink.with_alpha(0.14),
        );
    }
    p.icon(
        "window-close-symbolic",
        cancel.x + cancel.w / 2.0,
        cancel.y + cancel.h / 2.0,
        14.0,
        ink,
    );
    out.hits.push((cancel, Target::Act(Act::CancelTransfer)));
    let _ = c;
}

/// How tall the strip is while it shows, so a toast can sit above it.
pub fn strip_room(ui: &Ui) -> f32 {
    match &ui.x.xfer.strip {
        Some(s) if s.since.elapsed() > STRIP_DELAY => STRIP_H + 12.0,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_dir;
    use std::os::unix::fs::MetadataExt;

    fn go(sources: &[PathBuf], dest: &Path, mode: Mode) -> Outcome {
        run(
            sources,
            dest,
            mode,
            &AtomicBool::new(false),
            &mut |_, _, _| {},
        )
    }

    #[test]
    fn names_never_clash() {
        let d = test_dir("xfer-names");
        let free = unique_path(&d, OsStr::new("a.txt"), false);
        assert_eq!(free, d.join("a.txt"));
        fs::write(d.join("a.txt"), b"1").unwrap();
        let second = unique_path(&d, OsStr::new("a.txt"), false);
        assert_eq!(second, d.join("a (copy).txt"));
        fs::write(&second, b"2").unwrap();
        assert_eq!(
            unique_path(&d, OsStr::new("a.txt"), false),
            d.join("a (copy 2).txt")
        );
        // A copy of a copy is another copy of the original.
        assert_eq!(
            unique_path(&d, OsStr::new("a (copy).txt"), false),
            d.join("a (copy 2).txt")
        );
        fs::create_dir(d.join("dir.d")).unwrap();
        assert_eq!(
            unique_path(&d, OsStr::new("dir.d"), true),
            d.join("dir.d (copy)")
        );
        fs::write(d.join(".hidden"), b"").unwrap();
        assert_eq!(
            unique_path(&d, OsStr::new(".hidden"), false),
            d.join(".hidden (copy)")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    fn make_tree(root: &Path) {
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::write(root.join("top.txt"), vec![7u8; 1000]).unwrap();
        fs::write(root.join("sub/a.bin"), vec![1u8; 300_000]).unwrap();
        fs::write(root.join("sub/deep/b"), b"bee").unwrap();
        symlink("top.txt", root.join("link")).unwrap();
        fs::set_permissions(root.join("top.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        let old = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::open(root.join("top.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    #[test]
    fn copies_a_tree() {
        let d = test_dir("xfer-copy");
        let (src, dest) = (d.join("src"), d.join("dest"));
        make_tree(&src);
        fs::create_dir(&dest).unwrap();
        let mut seen = 0;
        let out = run(
            std::slice::from_ref(&src),
            &dest,
            Mode::Copy,
            &AtomicBool::new(false),
            &mut |_, done, total| {
                assert!(done <= total);
                seen += 1;
            },
        );
        assert!(out.error.is_none() && !out.cancelled);
        assert!(seen > 0);
        let got = dest.join("src");
        assert_eq!(out.made, vec![(src.clone(), got.clone())]);
        assert_eq!(tree_size(&got), tree_size(&src));
        assert_eq!(tree_size(&got), 1000 + 300_000 + 3);
        assert_eq!(
            fs::read_link(got.join("link")).unwrap(),
            Path::new("top.txt")
        );
        let (a, b) = (
            fs::metadata(src.join("top.txt")).unwrap(),
            fs::metadata(got.join("top.txt")).unwrap(),
        );
        assert_eq!(a.mode() & 0o7777, b.mode() & 0o7777);
        assert_eq!(a.modified().unwrap(), b.modified().unwrap());
        assert!(src.join("sub/a.bin").exists());

        // Again: a second copy, the first untouched.
        let out = go(std::slice::from_ref(&src), &dest, Mode::Copy);
        assert_eq!(out.made[0].1, dest.join("src (copy)"));
        // Into its own folder: a copy next to it.
        let out = go(&[dest.join("src")], &dest, Mode::Copy);
        assert_eq!(out.made[0].1, dest.join("src (copy 2)"));
        // A folder inside itself is refused.
        let out = go(std::slice::from_ref(&src), &src.join("sub"), Mode::Copy);
        assert!(out.error.unwrap().contains("inside itself"));
        assert!(out.made.is_empty());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn moves_and_never_overwrites() {
        let d = test_dir("xfer-move");
        let (src, dest) = (d.join("src"), d.join("dest"));
        make_tree(&src);
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("top.txt"), b"mine").unwrap();
        let out = go(&[src.join("top.txt"), src.join("sub")], &dest, Mode::Move);
        assert!(out.error.is_none());
        assert_eq!(fs::read(dest.join("top.txt")).unwrap(), b"mine");
        assert_eq!(fs::read(dest.join("top (copy).txt")).unwrap().len(), 1000);
        assert!(dest.join("sub/deep/b").exists());
        assert!(!src.join("sub").exists());
        // Moving into the folder it is already in does nothing.
        let out = go(&[dest.join("top.txt")], &dest, Mode::Move);
        assert!(out.made.is_empty() && out.error.is_none());
        // The cross-drive path: copy then delete.
        let from = d.join("cross");
        make_tree(&from);
        let to = d.join("cross2");
        move_exact(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(tree_size(&to), 1000 + 300_000 + 3);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn cancel_cleans_up() {
        let d = test_dir("xfer-cancel");
        let (src, dest) = (d.join("src"), d.join("dest"));
        make_tree(&src);
        fs::create_dir(&dest).unwrap();
        let cancel = AtomicBool::new(true);
        let out = run(
            std::slice::from_ref(&src),
            &dest,
            Mode::Copy,
            &cancel,
            &mut |_, _, _| {},
        );
        assert!(out.cancelled);
        assert!(out.made.is_empty());
        assert!(!dest.join("src").exists());
        assert!(src.exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
