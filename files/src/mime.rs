//! File types: the MIME type of a path from the shared MIME database
//! (`globs2`), its parents (`subclasses`), and the XDG base dirs both
//! thumbnails and the app index look in.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Fallback when the shared MIME database isn't there.
const MIME_TABLE: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("mp4", "video/mp4"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("avi", "video/x-msvideo"),
    ("mov", "video/quicktime"),
    ("svg", "image/svg+xml"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("heic", "image/heic"),
    ("avif", "image/avif"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("epub", "application/epub+zip"),
    ("xcf", "image/x-xcf"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("html", "text/html"),
    ("zip", "application/zip"),
];

pub const DIRECTORY: &str = "inode/directory";

/// `$XDG_DATA_HOME` then each of `$XDG_DATA_DIRS`.
pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => dirs.push(PathBuf::from(d)),
        _ => {
            if let Some(h) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(h).join(".local/share"));
            }
        }
    }
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// What the globs say: extensions (maybe with dots, `tar.gz`) and whole
/// names (`Makefile`), each to its heaviest type.
#[derive(Default)]
struct Globs {
    ext: HashMap<String, String>,
    name: HashMap<String, String>,
}

struct Db {
    globs: Globs,
    /// Type to the types it is a kind of.
    parents: HashMap<String, Vec<String>>,
}

static DB: OnceLock<Db> = OnceLock::new();

fn db() -> &'static Db {
    DB.get_or_init(|| {
        let mut globs = Globs::default();
        for (e, m) in MIME_TABLE {
            globs.ext.insert(e.to_string(), m.to_string());
        }
        let mut parents = HashMap::new();
        for dir in data_dirs() {
            // The first dir with a database wins.
            if let Ok(text) = std::fs::read_to_string(dir.join("mime/globs2")) {
                parse_globs(&text, &mut globs);
                if let Ok(sub) = std::fs::read_to_string(dir.join("mime/subclasses")) {
                    parents = parse_subclasses(&sub);
                }
                break;
            }
        }
        Db { globs, parents }
    })
}

/// Fill `into` from a `globs2` file, heaviest weight first.
fn parse_globs(text: &str, into: &mut Globs) {
    let mut weights: HashMap<(bool, String), u32> = HashMap::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let mut parts = line.split(':');
        let (Some(weight), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if glob.contains(['?', '[']) {
            continue;
        }
        let (is_ext, key) = match glob.strip_prefix("*.") {
            Some(ext) if !ext.contains('*') => (true, ext.to_lowercase()),
            _ if glob.contains('*') => continue,
            _ => (false, glob.to_string()),
        };
        let weight: u32 = weight.parse().unwrap_or(50);
        let slot = (is_ext, key.clone());
        if weights.get(&slot).is_none_or(|&w| weight > w) {
            weights.insert(slot, weight);
            let map = if is_ext {
                &mut into.ext
            } else {
                &mut into.name
            };
            map.insert(key, mime.to_string());
        }
    }
}

fn parse_subclasses(text: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let mut parts = line.split_whitespace();
        if let (Some(child), Some(parent)) = (parts.next(), parts.next()) {
            map.entry(child.to_string())
                .or_default()
                .push(parent.to_string());
        }
    }
    map
}

fn lookup(globs: &Globs, name: &str) -> Option<String> {
    if let Some(m) = globs.name.get(name) {
        return Some(m.clone());
    }
    let lower = name.to_lowercase();
    // The longest extension first: `a.tar.gz` is `tar.gz` before `gz`.
    lower
        .match_indices('.')
        .filter(|(i, _)| *i > 0)
        .find_map(|(i, _)| globs.ext.get(&lower[i + 1..]))
        .cloned()
}

/// The type of the extension alone (`png`), if the database knows it.
pub fn of_ext(ext: &str) -> Option<&'static str> {
    db().globs.ext.get(ext).map(String::as_str)
}

/// The type of a path: a folder, a name the database knows, else text if
/// the start of the file looks like it.
pub fn of_path(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return DIRECTORY.into();
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(m) = lookup(&db().globs, &name) {
        return m;
    }
    let mut head = [0u8; 512];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let head = &head[..n];
    // A multi-byte character cut at the end is fine.
    let valid = match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    if n > 0 && valid && !head.contains(&0) {
        "text/plain".into()
    } else {
        "application/octet-stream".into()
    }
}

/// `mime` and everything it is a kind of, nearest first.
pub fn chain(mime: &str) -> Vec<String> {
    chain_in(&db().parents, mime)
}

fn chain_in(parents: &HashMap<String, Vec<String>>, mime: &str) -> Vec<String> {
    let mut out = vec![mime.to_string()];
    let mut i = 0;
    while i < out.len() {
        if let Some(ps) = parents.get(&out[i]) {
            for p in ps {
                if !out.contains(p) {
                    out.push(p.clone());
                }
            }
        }
        i += 1;
    }
    // Everything textual is plain text; every file is a blob.
    if mime.starts_with("text/") && !out.iter().any(|m| m == "text/plain") {
        out.push("text/plain".into());
    }
    if mime != DIRECTORY && !out.iter().any(|m| m == "application/octet-stream") {
        out.push("application/octet-stream".into());
    }
    out
}

/// What to call the kind of file in "Always use for …".
pub fn describe(path: &Path, mime: &str) -> String {
    if mime == DIRECTORY {
        return "folders".into();
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!(".{} files", ext.to_lowercase()),
        _ => mime.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let mut g = Globs::default();
        parse_globs(
            "# comment\n50:video/mp4:*.mp4\n50:application/pdf:*.pdf\n50:application/gzip:*.gz\n50:application/x-compressed-tar:*.tar.gz\n10:image/a:*.zz\n80:image/b:*.zz\n50:text/x-makefile:Makefile\n50:x/y:*.a*b\n",
            &mut g,
        );
        assert_eq!(g.ext["mp4"], "video/mp4");
        assert_eq!(g.ext["zz"], "image/b");
        assert_eq!(g.name["Makefile"], "text/x-makefile");
        assert!(!g.ext.contains_key("a*b"));
        assert_eq!(
            lookup(&g, "backup.TAR.gz").as_deref(),
            Some("application/x-compressed-tar")
        );
        assert_eq!(lookup(&g, "x.gz").as_deref(), Some("application/gzip"));
        assert_eq!(lookup(&g, "Makefile").as_deref(), Some("text/x-makefile"));
        assert_eq!(lookup(&g, "noext"), None);
    }

    #[test]
    fn chains_climb_to_plain_text() {
        let parents = parse_subclasses("text/markdown text/plain\ntext/x-rust text/x-csrc\n");
        assert_eq!(
            chain_in(&parents, "text/markdown"),
            ["text/markdown", "text/plain", "application/octet-stream"]
        );
        assert_eq!(
            chain_in(&parents, "text/x-foo"),
            ["text/x-foo", "text/plain", "application/octet-stream"]
        );
        assert_eq!(chain_in(&parents, DIRECTORY), [DIRECTORY]);
    }

    #[test]
    fn sniffs_text_and_folders() {
        let dir = std::env::temp_dir().join(format!("cnde-mime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (t, b) = (dir.join("zzz-text"), dir.join("zzz-bin"));
        std::fs::write(&t, "hello\n").unwrap();
        std::fs::write(&b, [0u8, 1, 2, 255]).unwrap();
        assert_eq!(of_path(&t, false), "text/plain");
        assert_eq!(of_path(&b, false), "application/octet-stream");
        assert_eq!(of_path(&dir, true), DIRECTORY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe(Path::new("/a/B.PNG"), "image/png"), ".png files");
        assert_eq!(describe(Path::new("/a/b"), "text/plain"), "text/plain");
        assert_eq!(describe(Path::new("/a"), DIRECTORY), "folders");
    }
}
