//! The sidebar's custom shortcuts: GTK bookmarks, shared with the other file
//! managers. `~/.config/gtk-3.0/bookmarks` holds one `file:///path [Label]`
//! per line (percent-encoded); `~/.gtk-bookmarks` is the legacy place, read
//! only while the first file is missing. Lines this module doesn't
//! understand (other schemes, comments) are kept as they are.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// A line that is a folder of this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Index of the line in the file.
    pub line: usize,
    pub path: PathBuf,
    pub label: Option<String>,
}

impl Entry {
    /// What the sidebar calls it.
    pub fn name(&self) -> String {
        self.label.clone().unwrap_or_else(|| {
            self.path.file_name().map_or_else(
                || self.path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            )
        })
    }
}

/// The file's lines.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bookmarks {
    pub lines: Vec<String>,
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One line as an entry, if it is a `file://` folder.
fn parse_line(line: usize, text: &str) -> Option<Entry> {
    let text = text.trim();
    let rest = text.strip_prefix("file://")?;
    let (uri, label) = match rest.split_once(char::is_whitespace) {
        Some((u, l)) => (u, l.trim()),
        None => (rest, ""),
    };
    let path = percent_decode(uri);
    path.starts_with('/').then_some(())?;
    Some(Entry {
        line,
        path: PathBuf::from(path),
        label: (!label.is_empty()).then(|| label.to_string()),
    })
}

impl Bookmarks {
    pub fn parse(text: &str) -> Self {
        Bookmarks {
            lines: text.lines().map(str::to_string).collect(),
        }
    }

    pub fn render(&self) -> String {
        let mut out = self.lines.join("\n");
        if !self.lines.is_empty() {
            out.push('\n');
        }
        out
    }

    /// The folders, in file order.
    pub fn entries(&self) -> Vec<Entry> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| parse_line(i, l))
            .collect()
    }

    /// Add `path` at the end, or before the entry on line `before`. False
    /// if it is already there.
    pub fn add(&mut self, path: &Path, before: Option<usize>) -> bool {
        if self.entries().iter().any(|e| e.path == path) {
            return false;
        }
        let line = format!("file://{}", percent_encode(path));
        match before {
            Some(at) if at < self.lines.len() => self.lines.insert(at, line),
            _ => self.lines.push(line),
        }
        true
    }

    pub fn remove(&mut self, line: usize) {
        if line < self.lines.len() {
            self.lines.remove(line);
        }
    }

    /// Give the entry on `line` a label (none: back to its folder name).
    pub fn rename(&mut self, line: usize, label: &str) {
        let Some(entry) = self.lines.get(line).and_then(|l| parse_line(line, l)) else {
            return;
        };
        let label = label.trim();
        let uri = format!("file://{}", percent_encode(&entry.path));
        self.lines[line] = if label.is_empty() {
            uri
        } else {
            format!("{uri} {label}")
        };
    }

    /// Move the entry on `line` to just before the one on line `before`
    /// (the end if none).
    pub fn move_entry(&mut self, line: usize, before: Option<usize>) {
        if line >= self.lines.len() {
            return;
        }
        let text = self.lines.remove(line);
        let at = match before {
            Some(b) if b > line => b - 1,
            Some(b) => b,
            None => self.lines.len(),
        };
        self.lines.insert(at.min(self.lines.len()), text);
    }
}

/// `~/.config/gtk-3.0/bookmarks`.
pub fn primary(config_home: &Path) -> PathBuf {
    config_home.join("gtk-3.0").join("bookmarks")
}

fn legacy(home: &Path) -> PathBuf {
    home.join(".gtk-bookmarks")
}

/// Read the bookmarks: the GTK 3 file, else the legacy one, else none.
pub fn load(config_home: &Path, home: &Path) -> Bookmarks {
    match fs::read_to_string(primary(config_home)) {
        Ok(text) => Bookmarks::parse(&text),
        Err(_) => fs::read_to_string(legacy(home))
            .map(|t| Bookmarks::parse(&t))
            .unwrap_or_default(),
    }
}

/// Read, change and write the bookmarks back, atomically. The file is read
/// again first so edits made elsewhere meanwhile are kept.
pub fn update(
    config_home: &Path,
    home: &Path,
    change: impl FnOnce(&mut Bookmarks),
) -> io::Result<()> {
    let mut bm = load(config_home, home);
    let before = bm.clone();
    change(&mut bm);
    if bm == before {
        return Ok(());
    }
    let path = primary(config_home);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bm.render())?;
    fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "file:///home/me/Projects\n\
        file:///mnt/My%20Disk/Stuff Work stuff\n\
        sftp://server/share Server\n\
        # a comment\n\
        file:///tmp/gone\n";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cnde-bm-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_labels_and_escapes() {
        let bm = Bookmarks::parse(SAMPLE);
        let e = bm.entries();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].path, Path::new("/home/me/Projects"));
        assert_eq!(e[0].name(), "Projects");
        assert_eq!(e[1].path, Path::new("/mnt/My Disk/Stuff"));
        assert_eq!(e[1].label.as_deref(), Some("Work stuff"));
        assert_eq!(e[1].line, 1);
        assert_eq!(e[2].line, 4);
    }

    #[test]
    fn round_trip_keeps_unknown_lines() {
        let bm = Bookmarks::parse(SAMPLE);
        assert_eq!(bm.render(), SAMPLE.replace("        ", ""));
        let mut bm = bm;
        bm.remove(1);
        assert!(
            bm.render()
                .contains("sftp://server/share Server\n# a comment")
        );
        assert!(!bm.render().contains("Work stuff"));
    }

    #[test]
    fn add_rename_move() {
        let mut bm = Bookmarks::parse("file:///a\nfile:///b\n");
        assert!(bm.add(Path::new("/c d/é"), None));
        assert!(!bm.add(Path::new("/a"), None));
        assert_eq!(bm.lines[2], "file:///c%20d/%C3%A9");
        assert_eq!(bm.entries()[2].path, Path::new("/c d/é"));
        bm.rename(0, "  First  ");
        assert_eq!(bm.lines[0], "file:///a First");
        bm.rename(0, "");
        assert_eq!(bm.lines[0], "file:///a");
        bm.move_entry(0, None);
        assert_eq!(bm.lines, ["file:///b", "file:///c%20d/%C3%A9", "file:///a"]);
        bm.move_entry(2, Some(0));
        assert_eq!(bm.lines[0], "file:///a");
        bm.move_entry(0, Some(2));
        assert_eq!(bm.lines, ["file:///b", "file:///a", "file:///c%20d/%C3%A9"]);
    }

    #[test]
    fn files_primary_then_legacy_and_atomic_write() {
        let dir = scratch("files");
        let (config, home) = (dir.join("config"), dir.join("home"));
        fs::create_dir_all(&home).unwrap();
        // Only the legacy file: it is read, and a change moves to the new one.
        fs::write(home.join(".gtk-bookmarks"), "file:///old\n# keep\n").unwrap();
        assert_eq!(load(&config, &home).entries()[0].path, Path::new("/old"));
        update(&config, &home, |b| {
            b.add(Path::new("/new"), None);
        })
        .unwrap();
        let text = fs::read_to_string(primary(&config)).unwrap();
        assert_eq!(text, "file:///old\n# keep\nfile:///new\n");
        // The legacy file is ignored once the first exists.
        assert_eq!(load(&config, &home).entries().len(), 2);
        // No change: nothing written, no stray temp files.
        update(&config, &home, |_| {}).unwrap();
        let names: Vec<_> = fs::read_dir(primary(&config).parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(names.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
