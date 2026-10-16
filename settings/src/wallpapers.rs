//! Finding wallpapers to offer and making thumbnails of them, off the main
//! thread. Looks where GNOME and KDE install theirs, plus
//! ~/Pictures/Wallpapers for your own.

use std::path::{Path, PathBuf};

use cnde_ui::tiny_skia::Pixmap;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::debug;

/// Thumbnail size in pixels (16:10, enough for 2x screens at 120x75).
pub const THUMB: (u32, u32) = (240, 150);
/// More than this many pictures aren't shown.
const LIMIT: usize = 36;

pub struct Thumb {
    pub path: PathBuf,
    pub image: Pixmap,
}

/// Start finding and decoding; each thumbnail arrives on `tx` when ready.
/// `current` (the configured wallpaper) is always included, first.
pub fn load(current: Option<PathBuf>, tx: Sender<Thumb>) {
    std::thread::Builder::new()
        .name("thumbnails".into())
        .spawn(move || {
            let mut paths = find();
            if let Some(current) = current
                && current.is_file()
            {
                paths.retain(|p| *p != current);
                paths.insert(0, current);
            }
            paths.truncate(LIMIT);
            for path in paths {
                if let Some(image) = thumbnail(&path)
                    && tx.send(Thumb { path, image }).is_err()
                {
                    // The window closed.
                    return;
                }
            }
        })
        .ok();
}

fn find() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = &home {
        roots.push(home.join("Pictures/Wallpapers"));
    }
    for base in data_home.into_iter().chain(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    ) {
        roots.push(base.join("backgrounds"));
        roots.push(base.join("wallpapers"));
    }
    let mut found = Vec::new();
    for root in roots {
        collect(&root, 4, &mut found);
    }
    found.dedup();
    found
}

fn is_image(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp"
        )
    })
}

fn collect(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    // A KDE wallpaper package ships one picture in many sizes under
    // contents/images; offer only the biggest.
    if dir.ends_with("contents/images") {
        let biggest = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_image(p))
            .max_by_key(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.split_once('x'))
                    .and_then(|(w, h)| Some(w.parse::<u64>().ok()? * h.parse::<u64>().ok()?))
                    .unwrap_or(0)
            });
        out.extend(biggest);
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if depth > 0 {
                collect(&path, depth - 1, out);
            }
        } else if is_image(&path) {
            out.push(path);
        }
    }
}

/// Decode `path` and shrink it to cover [`THUMB`], premultiplied for
/// drawing.
fn thumbnail(path: &Path) -> Option<Pixmap> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .map_err(|err| debug!(?path, %err, "not a picture"))
        .ok()?;
    let (w, h) = (image.width() as f32, image.height() as f32);
    let zoom = (THUMB.0 as f32 / w).max(THUMB.1 as f32 / h).min(1.0);
    let small = image
        .thumbnail((w * zoom).ceil() as u32, (h * zoom).ceil() as u32)
        .into_rgba8();
    drop(image);
    let (tw, th) = small.dimensions();
    let mut data = small.into_raw();
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        for c in &mut px[..3] {
            *c = (*c as u32 * a / 255) as u8;
        }
    }
    Pixmap::from_vec(data, cnde_ui::tiny_skia::IntSize::from_wh(tw, th)?)
}
