//! Thumbnails, by the freedesktop thumbnail spec so the cache is shared
//! with Nautilus and friends: `$XDG_CACHE_HOME/thumbnails/{normal,large}/
//! <md5 of the URI>.png`, with `Thumb::URI` and `Thumb::MTime` text chunks.
//!
//! The painters only ask [`get`], which answers from an in-memory LRU of
//! pixmaps and otherwise queues the file for two worker threads. Images
//! are decoded in-process; other types go through the `.thumbnailer` files
//! in the data dirs, run with a timeout. Requests that stop being asked
//! for (scrolled away) are dropped before a worker picks them up.

use std::{
    collections::{HashMap, HashSet},
    io::Write,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cnde_ui::{
    Canvas,
    tiny_skia::{IntSize, Pixmap},
};
use image::{DynamicImage, RgbaImage, imageops::FilterType};

/// The spec's "normal" size.
pub const NORMAL: u32 = 128;
/// The spec's "large" size.
pub const LARGE: u32 = 256;
/// Images bigger than this aren't decoded here.
const MAX_DECODE: u64 = 50 << 20;
/// What the decoded pixmaps may take.
const CAP_BYTES: usize = 64 << 20;
/// A request not asked for within this of the newest one is stale.
const STALE: Duration = Duration::from_millis(250);
const TIMEOUT: Duration = Duration::from_secs(10);
const WORKERS: usize = 2;

// ---------------------------------------------------------------- MD5

/// MD5 of `data` (the cache names need it; nothing else does).
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let mut state = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for block in msg.chunks_exact(64) {
        let m: Vec<u32> = block
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, v) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn md5_hex(data: &[u8]) -> String {
    md5(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// The `file://` URI of a path, escaped the way GLib does (what Nautilus
/// hashes): letters, digits and `-_.~!$&'()*+,;=:@/` stay as they are.
pub fn uri_for(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:@/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ------------------------------------------------------------ PNG text

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// `png` with tEXt chunks added after the header.
fn add_text(png: &[u8], texts: &[(&str, &str)]) -> Vec<u8> {
    // Signature (8) + IHDR (4 length, 4 type, 13 data, 4 crc).
    let at = 33.min(png.len());
    let mut out = png[..at].to_vec();
    for (key, value) in texts {
        let mut body = Vec::new();
        body.extend_from_slice(b"tEXt");
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        out.extend_from_slice(&((body.len() - 4) as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    out.extend_from_slice(&png[at..]);
    out
}

/// The tEXt chunks before the image data.
fn read_text(png: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut i = 8;
    while i + 8 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        let kind = &png[i + 4..i + 8];
        if kind == b"IDAT" || i + 8 + len > png.len() {
            break;
        }
        if kind == b"tEXt" {
            let data = &png[i + 8..i + 8 + len];
            if let Some(nul) = data.iter().position(|&b| b == 0) {
                out.insert(
                    String::from_utf8_lossy(&data[..nul]).into_owned(),
                    String::from_utf8_lossy(&data[nul + 1..]).into_owned(),
                );
            }
        }
        i += 12 + len;
    }
    out
}

fn encode_png(img: &RgbaImage) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

// --------------------------------------------------------------- cache

/// `$XDG_CACHE_HOME/thumbnails` (or `~/.cache/thumbnails`).
pub fn cache_root() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("thumbnails")
}

fn class_name(size: u32) -> &'static str {
    if size > NORMAL { "large" } else { "normal" }
}

fn cache_file(root: &Path, size: u32, uri: &str) -> PathBuf {
    root.join(class_name(size))
        .join(format!("{}.png", md5_hex(uri.as_bytes())))
}

fn fail_file(root: &Path, uri: &str) -> PathBuf {
    root.join("fail")
        .join("cnde-files")
        .join(format!("{}.png", md5_hex(uri.as_bytes())))
}

/// Whether a cache entry's text says it is for this file as it is now.
fn valid(text: &HashMap<String, String>, uri: &str, mtime: i64) -> bool {
    text.get("Thumb::MTime").and_then(|m| m.parse::<i64>().ok()) == Some(mtime)
        && text.get("Thumb::URI").is_none_or(|u| u == uri)
}

/// The cached thumbnail of `uri`, if there is one that's still good.
fn cached(root: &Path, size: u32, uri: &str, mtime: i64) -> Option<RgbaImage> {
    let bytes = std::fs::read(cache_file(root, size, uri)).ok()?;
    if !valid(&read_text(&bytes), uri, mtime) {
        return None;
    }
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
    Some(img.to_rgba8())
}

fn failed_before(root: &Path, uri: &str, mtime: i64) -> bool {
    std::fs::read(fail_file(root, uri)).is_ok_and(|b| valid(&read_text(&b), uri, mtime))
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_name(dir: &Path) -> PathBuf {
    dir.join(format!(
        ".cnde-{}-{}.png",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `png` as `target` through a temp file, mode 0600.
fn write_atomic(target: &Path, png: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let temp = temp_name(dir);
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(png)?;
        std::fs::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn texts<'a>(uri: &'a str, mtime: &'a str) -> [(&'a str, &'a str); 2] {
    [("Thumb::URI", uri), ("Thumb::MTime", mtime)]
}

fn store(root: &Path, path: &Path, size: u32, uri: &str, mtime: i64, img: &RgbaImage) {
    // Thumbnails of the cache's own files would only loop.
    if path.starts_with(root) {
        return;
    }
    if let Some(png) = encode_png(img) {
        let png = add_text(&png, &texts(uri, &mtime.to_string()));
        let _ = write_atomic(&cache_file(root, size, uri), &png);
    }
}

fn store_failure(root: &Path, uri: &str, mtime: i64) {
    if let Some(png) = encode_png(&RgbaImage::new(1, 1)) {
        let png = add_text(&png, &texts(uri, &mtime.to_string()));
        let _ = write_atomic(&fail_file(root, uri), &png);
    }
}

// ---------------------------------------------------------- thumbnailers

/// One `.thumbnailer` file.
#[derive(Clone, Debug, PartialEq)]
pub struct Thumbnailer {
    pub try_exec: Option<String>,
    pub exec: String,
    pub mimes: Vec<String>,
}

pub fn parse_thumbnailer(text: &str) -> Option<Thumbnailer> {
    let mut in_entry = false;
    let (mut try_exec, mut exec, mut mimes) = (None, None, Vec::new());
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Thumbnailer Entry]";
        } else if in_entry && let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            match key.trim() {
                "TryExec" => try_exec = Some(value.to_string()),
                "Exec" => exec = Some(value.to_string()),
                "MimeType" => {
                    mimes = value
                        .split(';')
                        .filter(|m| !m.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                _ => {}
            }
        }
    }
    Some(Thumbnailer {
        try_exec,
        exec: exec?,
        mimes,
    })
}

/// The command line of a thumbnailer: `%i` input path, `%u` input URI,
/// `%o` output path, `%s` size, `%%` a percent sign.
pub fn expand(exec: &str, input: &Path, output: &Path, size: u32, uri: &str) -> Vec<String> {
    // Split on spaces, keeping "double quoted" parts together.
    let mut words: Vec<String> = Vec::new();
    let (mut cur, mut quoted, mut any) = (String::new(), false, false);
    for ch in exec.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' | '\t' if !quoted => {
                if any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            _ => {
                cur.push(ch);
                any = true;
            }
        }
    }
    if any {
        words.push(cur);
    }
    words
        .into_iter()
        .map(|w| {
            let mut out = String::new();
            let mut chars = w.chars();
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    out.push(ch);
                    continue;
                }
                match chars.next() {
                    Some('i') => out.push_str(&input.to_string_lossy()),
                    Some('o') => out.push_str(&output.to_string_lossy()),
                    Some('u') => out.push_str(uri),
                    Some('s') => out.push_str(&size.to_string()),
                    Some('%') => out.push('%'),
                    Some(other) => {
                        out.push('%');
                        out.push(other);
                    }
                    None => out.push('%'),
                }
            }
            out
        })
        .collect()
}

fn in_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).exists();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).exists()))
}

/// The thumbnailers that can run here, and which extension is which type.
struct Know {
    thumbnailers: Vec<Thumbnailer>,
}

static KNOW: OnceLock<Know> = OnceLock::new();

fn load_know() -> Know {
    let dirs = crate::mime::data_dirs();
    let mut thumbnailers = Vec::new();
    let mut seen = HashSet::new();
    for dir in &dirs {
        let Ok(read) = std::fs::read_dir(dir.join("thumbnailers")) else {
            continue;
        };
        let mut files: Vec<_> = read.flatten().map(|e| e.path()).collect();
        files.sort();
        for file in files {
            let name = file.file_name().map(|n| n.to_owned());
            if file.extension().is_none_or(|e| e != "thumbnailer") || !seen.insert(name) {
                continue;
            }
            if let Some(t) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| parse_thumbnailer(&text))
                && t.try_exec.as_deref().is_none_or(in_path)
                && t.exec.split(' ').next().is_some_and(in_path)
            {
                thumbnailers.push(t);
            }
        }
    }
    Know {
        thumbnailers,
    }
}

impl Know {
    fn thumbnailer_for(&self, ext: &str) -> Option<&Thumbnailer> {
        let mime = crate::mime::of_ext(ext)?;
        self.thumbnailers
            .iter()
            .find(|t| t.mimes.iter().any(|m| m == mime))
    }
}

fn ext_of(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_lowercase())
}

/// Formats decoded in-process.
fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "jpe" | "webp" | "gif")
}

/// Whether the file could have a thumbnail at all (cheap; the UI thread
/// asks this). `images_only` leaves out what needs an external program.
pub fn eligible(path: &Path, images_only: bool) -> bool {
    let Some(ext) = ext_of(path) else {
        return false;
    };
    if path.starts_with("/run/user") && path.components().any(|c| c.as_os_str() == "gvfs") {
        return false;
    }
    is_image_ext(&ext)
        || (!images_only
            && KNOW
                .get()
                .is_some_and(|k| k.thumbnailer_for(&ext).is_some()))
}

// ------------------------------------------------------------ generation

fn mtime_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn fit_down(img: DynamicImage, size: u32) -> RgbaImage {
    let img = if img.width() > size || img.height() > size {
        img.resize(size, size, FilterType::Triangle)
    } else {
        img
    };
    img.to_rgba8()
}

fn run_thumbnailer(t: &Thumbnailer, path: &Path, uri: &str, size: u32, out: &Path) -> bool {
    let args = expand(&t.exec, path, out, size, uri);
    let Some((program, rest)) = args.split_first() else {
        return false;
    };
    let Ok(mut child) = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Make the thumbnail of `path` (from the cache when it's good), writing
/// it to the cache. `None` when it can't be done.
fn make(root: &Path, path: &Path, size: u32) -> Option<RgbaImage> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = mtime_secs(meta.modified().ok()?);
    let uri = uri_for(path);
    if let Some(img) = cached(root, size, &uri, mtime) {
        return Some(img);
    }
    if failed_before(root, &uri, mtime) {
        return None;
    }
    let ext = ext_of(path)?;
    let made = if is_image_ext(&ext) {
        if meta.len() > MAX_DECODE {
            None
        } else {
            image::open(path).ok().map(|img| fit_down(img, size))
        }
    } else {
        let know = KNOW.get()?;
        let t = know.thumbnailer_for(&ext)?;
        let dir = root.join(class_name(size));
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        let out = temp_name(&dir);
        let ok = run_thumbnailer(t, path, &uri, size, &out);
        let img = if ok { image::open(&out).ok() } else { None };
        let _ = std::fs::remove_file(&out);
        img.map(|img| fit_down(img, size))
    };
    match made {
        Some(img) => {
            store(root, path, size, &uri, mtime, &img);
            Some(img)
        }
        None => {
            if !path.starts_with(root) {
                store_failure(root, &uri, mtime);
            }
            None
        }
    }
}

fn to_pixmap(img: &RgbaImage) -> Option<Pixmap> {
    let mut data = img.as_raw().clone();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
    Pixmap::from_vec(data, IntSize::from_wh(img.width(), img.height())?)
}

// ----------------------------------------------------------------- store

type Key = (PathBuf, i64, u32);

enum Slot {
    Ready(Arc<Pixmap>, u64),
    Failed,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<Key, Slot>,
    bytes: usize,
    tick: u64,
    /// In the order they were first asked for.
    queue: Vec<Key>,
    stamps: HashMap<Key, Instant>,
    working: HashSet<Key>,
    newest: Option<Instant>,
}

struct Store {
    inner: Mutex<Inner>,
    wake: Condvar,
    notify: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

static STORE: OnceLock<Store> = OnceLock::new();

fn store_ref() -> &'static Store {
    STORE.get_or_init(|| Store {
        inner: Mutex::new(Inner::default()),
        wake: Condvar::new(),
        notify: OnceLock::new(),
    })
}

fn lock() -> MutexGuard<'static, Inner> {
    store_ref().inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    fn insert(&mut self, key: Key, pixmap: Arc<Pixmap>) {
        self.tick += 1;
        self.bytes += pixmap.data().len();
        if let Some(Slot::Ready(old, _)) = self.slots.insert(key, Slot::Ready(pixmap, self.tick)) {
            self.bytes -= old.data().len();
        }
        while self.bytes > CAP_BYTES {
            let oldest = self
                .slots
                .iter()
                .filter_map(|(k, s)| match s {
                    Slot::Ready(_, t) => Some((*t, k.clone())),
                    Slot::Failed => None,
                })
                .min();
            let Some((_, k)) = oldest else { break };
            if let Some(Slot::Ready(p, _)) = self.slots.remove(&k) {
                self.bytes -= p.data().len();
            }
        }
    }

    /// The next request still wanted; stale ones are dropped on the way.
    fn next(&mut self) -> Option<Key> {
        let newest = self.newest?;
        while !self.queue.is_empty() {
            let key = self.queue.remove(0);
            let Some(stamp) = self.stamps.remove(&key) else {
                continue;
            };
            if newest.duration_since(stamp) <= STALE {
                self.working.insert(key.clone());
                return Some(key);
            }
        }
        None
    }
}

/// Start the workers; `notify` is called (from a worker) whenever a
/// thumbnail arrives, to ask for a repaint.
pub fn start(notify: impl Fn() + Send + Sync + 'static) {
    let s = store_ref();
    if s.notify.set(Box::new(notify)).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("thumb-know".into())
        .spawn(|| {
            let _ = KNOW.set(load_know());
            if let Some(n) = store_ref().notify.get() {
                n();
            }
        });
    let root = cache_root();
    for i in 0..WORKERS {
        let root = root.clone();
        let _ = std::thread::Builder::new()
            .name(format!("thumbs-{i}"))
            .spawn(move || worker(&root));
    }
}

fn worker(root: &Path) {
    let s = store_ref();
    loop {
        let key = {
            let mut inner = lock();
            loop {
                if let Some(key) = inner.next() {
                    break key;
                }
                inner = s.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
            }
        };
        let pixmap = make(root, &key.0, key.2).and_then(|img| to_pixmap(&img));
        {
            let mut inner = lock();
            inner.working.remove(&key);
            match pixmap {
                Some(p) => inner.insert(key, Arc::new(p)),
                None => {
                    inner.slots.insert(key, Slot::Failed);
                }
            }
        }
        if let Some(n) = s.notify.get() {
            n();
        }
    }
}

/// The thumbnail of a file at `size` (`NORMAL` or `LARGE`) if it's ready;
/// otherwise it's queued and the repaint comes when it is.
pub fn get(
    path: &Path,
    modified: Option<SystemTime>,
    size: u32,
    images_only: bool,
) -> Option<Arc<Pixmap>> {
    if !eligible(path, images_only) {
        return None;
    }
    let key: Key = (path.to_path_buf(), mtime_secs(modified?), size);
    let mut inner = lock();
    inner.tick += 1;
    let tick = inner.tick;
    match inner.slots.get_mut(&key) {
        Some(Slot::Ready(p, t)) => {
            *t = tick;
            return Some(p.clone());
        }
        Some(Slot::Failed) => return None,
        None => {}
    }
    let now = Instant::now();
    inner.newest = Some(now);
    if inner.working.contains(&key) {
        return None;
    }
    if inner.stamps.insert(key.clone(), now).is_none() {
        inner.queue.push(key);
        drop(inner);
        store_ref().wake.notify_one();
    }
    None
}

/// Make a thumbnail now, on this thread (previews and tests).
#[cfg(test)]
pub fn prime(path: &Path, size: u32) -> bool {
    let Some(modified) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()) else {
        return false;
    };
    let _ = KNOW.set(load_know());
    let Some(pixmap) = make(&cache_root(), path, size).and_then(|i| to_pixmap(&i)) else {
        return false;
    };
    put(path, modified, size, Arc::new(pixmap));
    true
}

/// Put a ready pixmap in the store (benchmarks).
#[cfg(test)]
pub fn put(path: &Path, modified: SystemTime, size: u32, pixmap: Arc<Pixmap>) {
    lock().insert((path.to_path_buf(), mtime_secs(modified), size), pixmap);
}

/// Draw `pixmap` inside the box, whole and centered, rounded.
pub fn draw(
    canvas: &mut Canvas<'_>,
    pixmap: &Pixmap,
    (bx, by, bw, bh): (f32, f32, f32, f32),
    radius: f32,
) {
    let (iw, ih) = (pixmap.width() as f32, pixmap.height() as f32);
    let zoom = (bw / iw).min(bh / ih);
    let (w, h) = (iw * zoom, ih * zoom);
    canvas.image(
        pixmap.as_ref(),
        bx + (bw - w) / 2.0,
        by + (bh - h) / 2.0,
        w,
        h,
        radius,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cnde-thumbs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn md5_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        // The example of the thumbnail spec.
        assert_eq!(
            md5_hex(b"file:///home/jens/photos/me.png"),
            "c6ee772d9e49320e97ec29a7eb5b1697"
        );
    }

    #[test]
    fn uris_escape_like_glib() {
        assert_eq!(uri_for(Path::new("/a/b.png")), "file:///a/b.png");
        assert_eq!(
            uri_for(Path::new("/home/me/My Photos/a (1)#é.png")),
            "file:///home/me/My%20Photos/a%20(1)%23%C3%A9.png"
        );
        assert_eq!(uri_for(Path::new("/x/50%.png")), "file:///x/50%25.png");
    }

    #[test]
    fn text_chunks_round_trip() {
        let img = RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        let png = add_text(&encode_png(&img).unwrap(), &texts("file:///a", "42"));
        let text = read_text(&png);
        assert_eq!(text["Thumb::URI"], "file:///a");
        assert_eq!(text["Thumb::MTime"], "42");
        // Still a PNG that decodes.
        let back = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(back, img);
    }

    #[test]
    fn cache_validity() {
        let root = temp("valid");
        let uri = "file:///a/b.png";
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        store(&root, Path::new("/a/b.png"), NORMAL, uri, 100, &img);
        let file = cache_file(&root, NORMAL, uri);
        assert!(file.starts_with(root.join("normal")));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(cached(&root, NORMAL, uri, 100).is_some());
        // Changed file: stale.
        assert!(cached(&root, NORMAL, uri, 101).is_none());
        // Another size class has nothing.
        assert!(cached(&root, LARGE, uri, 100).is_none());
        // Files inside the cache aren't stored.
        let inside = root.join("normal/x.png");
        store(&root, &inside, NORMAL, "file:///inside", 1, &img);
        assert!(!cache_file(&root, NORMAL, "file:///inside").exists());
        // Failures are remembered by mtime too.
        store_failure(&root, uri, 100);
        assert!(failed_before(&root, uri, 100));
        assert!(!failed_before(&root, uri, 101));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn makes_and_reuses_a_thumbnail() {
        let dir = temp("make");
        let root = dir.join("cache");
        let src = dir.join("big one.png");
        RgbaImage::from_pixel(600, 300, image::Rgba([200, 0, 0, 255]))
            .save(&src)
            .unwrap();
        let img = make(&root, &src, NORMAL).unwrap();
        assert_eq!((img.width(), img.height()), (128, 64));
        assert!(cache_file(&root, NORMAL, &uri_for(&src)).exists());
        // The second time it comes from the cache.
        let again = make(&root, &src, NORMAL).unwrap();
        assert_eq!(again.dimensions(), (128, 64));
        let large = make(&root, &src, LARGE).unwrap();
        assert_eq!(large.dimensions(), (256, 128));
        let pixmap = to_pixmap(&img).unwrap();
        assert_eq!(pixmap.width(), 128);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnailer_files() {
        let t = parse_thumbnailer(
            "[Thumbnailer Entry]\nTryExec=ffmpegthumbnailer\nExec=ffmpegthumbnailer -i %i -o %o -s %s -f\nMimeType=video/mp4;video/webm;\n",
        )
        .unwrap();
        assert_eq!(t.try_exec.as_deref(), Some("ffmpegthumbnailer"));
        assert_eq!(t.mimes, ["video/mp4", "video/webm"]);
        assert!(parse_thumbnailer("[Other]\nExec=x\n").is_none());
    }

    #[test]
    fn percent_substitution() {
        let args = expand(
            "thumb -s %s \"%i\" %o --uri=%u 100%%",
            Path::new("/a b/c.pdf"),
            Path::new("/tmp/o.png"),
            128,
            "file:///a%20b/c.pdf",
        );
        assert_eq!(
            args,
            [
                "thumb",
                "-s",
                "128",
                "/a b/c.pdf",
                "/tmp/o.png",
                "--uri=file:///a%20b/c.pdf",
                "100%"
            ]
        );
    }

    #[test]
    fn lru_evicts_the_oldest() {
        let mut inner = Inner::default();
        // Each is 4 MiB; sixteen fit under the cap.
        let big = Arc::new(Pixmap::new(1024, 1024).unwrap());
        for i in 0..20 {
            inner.insert((PathBuf::from(format!("/{i}")), 0, NORMAL), big.clone());
        }
        assert!(inner.bytes <= CAP_BYTES);
        assert_eq!(inner.slots.len(), 16);
        assert!(!inner.slots.contains_key(&(PathBuf::from("/0"), 0, NORMAL)));
        assert!(inner.slots.contains_key(&(PathBuf::from("/19"), 0, NORMAL)));
    }

    #[test]
    fn stale_requests_are_dropped() {
        let mut inner = Inner::default();
        let old = Instant::now() - Duration::from_secs(1);
        let (a, b) = (
            (PathBuf::from("/a"), 0, NORMAL),
            (PathBuf::from("/b"), 0, NORMAL),
        );
        inner.queue = vec![a.clone(), b.clone()];
        inner.stamps.insert(a, old);
        inner.stamps.insert(b.clone(), Instant::now());
        inner.newest = Some(Instant::now());
        assert_eq!(inner.next(), Some(b));
        assert_eq!(inner.next(), None);
    }
}
