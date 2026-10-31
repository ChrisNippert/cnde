//! Reading directories: entries, sorting, kinds, and recursive sizes.

use std::{
    cmp::Ordering,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering::Relaxed},
    time::SystemTime,
};

/// One row in a folder view.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Name starts with '.'.
    pub hidden: bool,
    /// File bytes; 0 for folders.
    pub size: u64,
    /// Folders: number of children. Otherwise `None`.
    pub items: Option<u32>,
    pub modified: Option<SystemTime>,
    pub kind: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Kind,
}

/// Compare strings case-insensitively with digit runs as numbers, so
/// "file2" sorts before "file10".
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(&c) = it.peek() {
                        if !c.is_ascii_digit() {
                            break;
                        }
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (ra, rb) = (run(&mut a), run(&mut b));
                let (ta, tb) = (ra.trim_start_matches('0'), rb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// Sort in place; ties fall back to name. With `folders_first` folders stay
/// on top whichever way the rest is ordered, except by date, where files and
/// folders mix.
pub fn sort(entries: &mut [Entry], key: SortKey, descending: bool, folders_first: bool) {
    let split = folders_first && key != SortKey::Modified;
    entries.sort_by(|a, b| {
        let dirs = b.is_dir.cmp(&a.is_dir);
        if split && dirs != Ordering::Equal {
            return dirs;
        }
        let by_name = natural_cmp(&a.name, &b.name);
        let ord = match key {
            SortKey::Name => by_name,
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Kind => natural_cmp(&a.kind, &b.kind),
        };
        let ord = if descending { ord.reverse() } else { ord };
        ord.then(by_name)
    });
}

/// Read `path`, handing entries to `batch` in chunks of about 256.
/// Stops quietly (still `Ok`) once `cancel` is set.
pub fn list_dir(
    path: &Path,
    cancel: &AtomicBool,
    mut batch: impl FnMut(Vec<Entry>),
) -> io::Result<()> {
    const CHUNK: usize = 256;
    let mut chunk = Vec::with_capacity(CHUNK);
    for de in fs::read_dir(path)? {
        if cancel.load(Relaxed) {
            return Ok(());
        }
        let Ok(de) = de else { continue };
        if let Some(e) = entry_of(&de) {
            chunk.push(e);
        }
        if chunk.len() >= CHUNK {
            batch(std::mem::replace(&mut chunk, Vec::with_capacity(CHUNK)));
        }
    }
    if !chunk.is_empty() {
        batch(chunk);
    }
    Ok(())
}

fn entry_of(de: &fs::DirEntry) -> Option<Entry> {
    let name = de.file_name().to_string_lossy().into_owned();
    let path = de.path();
    let lmeta = de.metadata().ok()?;
    let is_symlink = lmeta.file_type().is_symlink();
    // A link shows what it points at; a dangling one stays a plain file.
    let meta = if is_symlink {
        fs::metadata(&path).unwrap_or(lmeta)
    } else {
        lmeta
    };
    let is_dir = meta.is_dir();
    let items = is_dir.then(|| {
        fs::read_dir(&path)
            .map(|d| d.count().min(u32::MAX as usize) as u32)
            .unwrap_or(0)
    });
    Some(Entry {
        hidden: name.starts_with('.'),
        kind: kind_of(&name, is_dir),
        size: if is_dir { 0 } else { meta.len() },
        modified: meta.modified().ok(),
        name,
        path,
        is_dir,
        is_symlink,
        items,
    })
}

/// A friendly description from the file name's extension.
pub fn kind_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".into();
    }
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return "File".into();
    };
    if stem.is_empty() || ext.is_empty() {
        return "File".into();
    }
    let known = match ext.to_ascii_lowercase().as_str() {
        "png" => "PNG image",
        "jpg" | "jpeg" => "JPEG image",
        "gif" => "GIF image",
        "webp" => "WebP image",
        "svg" => "SVG image",
        "bmp" => "BMP image",
        "avif" => "AVIF image",
        "rs" => "Rust source",
        "c" => "C source",
        "h" => "C header",
        "cpp" | "cc" | "cxx" => "C++ source",
        "py" => "Python source",
        "js" => "JavaScript source",
        "ts" => "TypeScript source",
        "go" => "Go source",
        "java" => "Java source",
        "sh" | "bash" | "zsh" => "Shell script",
        "html" | "htm" => "HTML document",
        "css" => "Stylesheet",
        "txt" | "text" | "log" => "Text",
        "md" | "markdown" => "Markdown",
        "toml" => "TOML",
        "json" => "JSON",
        "yaml" | "yml" => "YAML",
        "xml" => "XML",
        "pdf" => "PDF document",
        "odt" | "doc" | "docx" => "Document",
        "ods" | "xls" | "xlsx" => "Spreadsheet",
        "odp" | "ppt" | "pptx" => "Presentation",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "7z" | "rar" => "Archive",
        "mp4" | "mkv" | "webm" | "mov" | "avi" => "Video",
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" => "Audio",
        _ => "",
    };
    if known.is_empty() {
        format!("{} file", ext.to_ascii_uppercase())
    } else {
        known.into()
    }
}

/// Total apparent size of everything under `path`. Doesn't follow symlinks,
/// stays on one filesystem, and skips whatever it can't read.
pub fn dir_size(path: &Path, cancel: &AtomicBool) -> u64 {
    let Ok(root) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !root.is_dir() {
        return root.len();
    }
    let dev = root.dev();
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for de in rd.flatten() {
            if cancel.load(Relaxed) {
                return total;
            }
            let Ok(m) = de.metadata() else { continue };
            if m.is_dir() {
                if m.dev() == dev {
                    stack.push(de.path());
                }
            } else {
                total += m.len();
            }
        }
    }
    total
}

#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "cnde-files-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Relaxed)
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn e(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            path: name.into(),
            is_dir,
            is_symlink: false,
            hidden: name.starts_with('.'),
            size,
            items: None,
            modified: None,
            kind: kind_of(name, is_dir),
        }
    }

    fn names(v: &[Entry]) -> Vec<&str> {
        v.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn natural_name_sort_with_folders_first() {
        let mut v = vec![
            e("file10", false, 1),
            e("File2", false, 1),
            e("zdir", true, 0),
            e("file1", false, 1),
            e("adir", true, 0),
        ];
        sort(&mut v, SortKey::Name, false, true);
        assert_eq!(names(&v), ["adir", "zdir", "file1", "File2", "file10"]);
        sort(&mut v, SortKey::Name, true, true);
        assert_eq!(names(&v), ["zdir", "adir", "file10", "File2", "file1"]);
    }

    #[test]
    fn folders_first_is_optional_and_dates_mix() {
        let mut v = vec![e("b", false, 1), e("adir", true, 0), e("a", false, 1)];
        sort(&mut v, SortKey::Name, false, false);
        assert_eq!(names(&v), ["a", "adir", "b"]);
        sort(&mut v, SortKey::Name, false, true);
        assert_eq!(names(&v), ["adir", "a", "b"]);

        let at = |s| Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
        let mut v = vec![e("old", false, 1), e("dir", true, 0), e("new", false, 1)];
        v[0].modified = at(10);
        v[1].modified = at(20);
        v[2].modified = at(30);
        // Newest first, whether file or folder, even with folders first.
        sort(&mut v, SortKey::Modified, true, true);
        assert_eq!(names(&v), ["new", "dir", "old"]);
        sort(&mut v, SortKey::Modified, false, true);
        assert_eq!(names(&v), ["old", "dir", "new"]);
    }

    #[test]
    fn size_sort_ties_by_name() {
        let mut v = vec![e("b", false, 5), e("a", false, 5), e("c", false, 1)];
        sort(&mut v, SortKey::Size, false, true);
        assert_eq!(names(&v), ["c", "a", "b"]);
        sort(&mut v, SortKey::Size, true, true);
        assert_eq!(names(&v), ["a", "b", "c"]);
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("x", true), "Folder");
        assert_eq!(kind_of("a.PNG", false), "PNG image");
        assert_eq!(kind_of("main.rs", false), "Rust source");
        assert_eq!(kind_of("Cargo.toml", false), "TOML");
        assert_eq!(kind_of("a.xyz", false), "XYZ file");
        assert_eq!(kind_of("Makefile", false), "File");
        assert_eq!(kind_of(".bashrc", false), "File");
    }

    #[test]
    fn lists_in_batches_and_follows_dir_links() {
        let d = test_dir("list");
        for i in 0..300 {
            fs::write(d.join(format!("f{i}")), b"hello").unwrap();
        }
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub/a"), b"x").unwrap();
        fs::write(d.join("sub/b"), b"x").unwrap();
        symlink(d.join("sub"), d.join("link")).unwrap();
        symlink(d.join("nowhere"), d.join("dangling")).unwrap();
        fs::write(d.join(".hid"), b"").unwrap();

        let cancel = AtomicBool::new(false);
        let mut sizes = vec![];
        let mut all = vec![];
        list_dir(&d, &cancel, |b| {
            sizes.push(b.len());
            all.extend(b);
        })
        .unwrap();
        assert_eq!(sizes, [256, 48]);
        let get = |n: &str| all.iter().find(|e| e.name == n).unwrap();
        assert!(get("sub").is_dir && get("sub").items == Some(2));
        assert!(get("link").is_dir && get("link").is_symlink);
        assert!(!get("dangling").is_dir && get("dangling").is_symlink);
        assert!(get(".hid").hidden);
        assert_eq!(get("f1").size, 5);

        let stop = AtomicBool::new(true);
        let mut n = 0;
        list_dir(&d, &stop, |b| n += b.len()).unwrap();
        assert_eq!(n, 0);
        assert!(list_dir(&d.join("missing"), &cancel, |_| {}).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sums_sizes_without_following_links() {
        let d = test_dir("size");
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/one"), vec![0; 100]).unwrap();
        fs::write(d.join("a/b/two"), vec![0; 50]).unwrap();
        symlink(&d, d.join("a/loop")).unwrap();
        let cancel = AtomicBool::new(false);
        let link_len = fs::symlink_metadata(d.join("a/loop")).unwrap().len();
        assert_eq!(dir_size(&d, &cancel), 150 + link_len);
        assert_eq!(dir_size(&d.join("a/one"), &cancel), 100);
        assert_eq!(dir_size(&d.join("none"), &cancel), 0);
        assert_eq!(dir_size(&d, &AtomicBool::new(true)), 0);
        fs::remove_dir_all(&d).unwrap();
    }
}
