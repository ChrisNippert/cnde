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

/// Ask for a picture of your own (the desktop's file chooser through the
/// portal, else zenity or kdialog), keep a copy where GNOME keeps added
/// backgrounds, and send its thumbnail. `Ok(None)`: cancelled.
pub fn add(tx: Sender<Result<Option<Thumb>, String>>) {
    std::thread::Builder::new()
        .name("add-wallpaper".into())
        .spawn(move || {
            let result = choose().and_then(|picked| match picked {
                None => Ok(None),
                Some(path) => {
                    let path = keep(&path)?;
                    let image = thumbnail(&path)
                        .ok_or_else(|| format!("{} isn't a picture", path.display()))?;
                    Ok(Some(Thumb { path, image }))
                }
            });
            let _ = tx.send(result);
        })
        .ok();
}

fn choose() -> Result<Option<PathBuf>, String> {
    match portal_choose() {
        Ok(picked) => return Ok(picked),
        Err(err) => debug!(%err, "no file chooser portal; trying zenity and kdialog"),
    }
    let tools: [(&str, &[&str]); 2] = [
        (
            "zenity",
            &[
                "--file-selection",
                "--title=Choose a Background",
                "--file-filter=Pictures | *.png *.jpg *.jpeg *.webp *.PNG *.JPG *.JPEG *.WEBP",
            ],
        ),
        (
            "kdialog",
            &[
                "--title",
                "Choose a Background",
                "--getopenfilename",
                ".",
                "image/png image/jpeg image/webp",
            ],
        ),
    ];
    for (tool, args) in tools {
        let Ok(output) = std::process::Command::new(tool)
            .args(args)
            .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
            .output()
        else {
            continue;
        };
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // Exit status 1 with nothing printed: cancelled.
        return Ok((output.status.success() && !path.is_empty()).then(|| path.into()));
    }
    Err(
        "No file chooser: install xdg-desktop-portal-gtk (or zenity), \
         or put pictures in ~/Pictures/Wallpapers"
            .into(),
    )
}

/// org.freedesktop.portal.FileChooser.OpenFile, waiting for its answer.
fn portal_choose() -> zbus::Result<Option<PathBuf>> {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    let conn = zbus::blocking::Connection::session()?;
    let token = format!("cnde_settings_{}", std::process::id());
    // Listen on the request's path before asking, so a quick answer isn't
    // missed (the path is predictable from our name and the token).
    let sender = conn
        .unique_name()
        .map(|n| n.trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default();
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        path.as_str(),
        "org.freedesktop.portal.Request",
    )?;
    let mut responses = request.receive_signal("Response")?;

    let filters = vec![(
        "Pictures",
        vec![(1u32, "image/png"), (1, "image/jpeg"), (1, "image/webp")],
    )];
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("filters", Value::from(filters));
    conn.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.FileChooser"),
        "OpenFile",
        &("", "Choose a Background", options),
    )?;

    let Some(message) = responses.next() else {
        return Err(zbus::Error::Failure("the portal went away".into()));
    };
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    if code != 0 {
        return Ok(None);
    }
    let uris: Vec<String> = results
        .get("uris")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| v.try_into().ok())
        .unwrap_or_default();
    Ok(uris.first().and_then(|uri| file_uri_path(uri)))
}

/// "file:///a%20b.png" -> "/a b.png".
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = rest
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(std::ffi::OsString::from_vec(out).into())
}

/// A copy in ~/.local/share/backgrounds, so the picture stays offered (and
/// set) even if the original moves. Pictures already there stay put.
fn keep(path: &Path) -> Result<PathBuf, String> {
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or("HOME isn't set")?
        .join("backgrounds");
    if path.starts_with(&dir) {
        return Ok(path.to_path_buf());
    }
    std::fs::create_dir_all(&dir).map_err(|err| format!("Can't make {}: {err}", dir.display()))?;
    let name = path.file_name().ok_or("That's not a file")?;
    let mut target = dir.join(name);
    // Another picture by this name: number this one.
    let mut n = 2;
    while target.exists() && !same_file(path, &target) {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        target = dir.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    if !target.exists() {
        std::fs::copy(path, &target).map_err(|err| format!("Can't copy the picture: {err}"))?;
    }
    Ok(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode() {
        assert_eq!(
            file_uri_path("file:///home/me/My%20Pics/a%2Bb.png"),
            Some(PathBuf::from("/home/me/My Pics/a+b.png"))
        );
        assert_eq!(file_uri_path("https://example.com/a.png"), None);
    }
}
