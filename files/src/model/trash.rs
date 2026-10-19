//! Moving files to the freedesktop home trash.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::SystemTime,
};

use super::{format::local_tm, listing::dir_size};

/// `$XDG_DATA_HOME/Trash`, by default `~/.local/share/Trash`.
pub fn trash_dir() -> PathBuf {
    let base = match env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/share"),
    };
    base.join("Trash")
}

/// Bytes held in the trash.
pub fn trash_size() -> u64 {
    dir_size(&trash_dir().join("files"), &AtomicBool::new(false))
}

/// Move `path` to the trash, recording where it came from.
pub fn trash(path: &Path) -> io::Result<()> {
    trash_in(&trash_dir(), path)
}

pub(crate) fn trash_in(root: &Path, path: &Path) -> io::Result<()> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let base = abs
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "nothing to trash"))?
        .to_string_lossy()
        .into_owned();
    // Check it exists (without following a symlink) before making dirs.
    fs::symlink_metadata(&abs)?;
    let (files, info) = (root.join("files"), root.join("info"));
    fs::create_dir_all(&files)?;
    fs::create_dir_all(&info)?;

    let mut name = base.clone();
    let mut n = 2;
    // create_new on the info file reserves the name atomically.
    let mut info_file = loop {
        let free = fs::symlink_metadata(files.join(&name)).is_err();
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(info.join(format!("{name}.trashinfo")));
        match f {
            Ok(f) if free => break (f, name.clone()),
            Ok(_) => {
                let _ = fs::remove_file(info.join(format!("{name}.trashinfo")));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        name = format!("{base} ({n})");
        n += 1;
    };
    let t = local_tm(SystemTime::now());
    let text = format!(
        "[Trash Info]\nPath={}\nDeletionDate={:04}-{:02}-{:02}T{:02}:{:02}:{:02}\n",
        percent_encode(&abs.to_string_lossy()),
        t.year,
        t.month,
        t.day,
        t.hour,
        t.min,
        t.sec
    );
    io::Write::write_all(&mut info_file.0, text.as_bytes())?;
    let info_path = info.join(format!("{}.trashinfo", info_file.1));
    if let Err(e) = fs::rename(&abs, files.join(&info_file.1)) {
        let _ = fs::remove_file(&info_path);
        if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) {
            return Err(io::Error::other(
                "Moving to the trash across drives isn't supported yet",
            ));
        }
        return Err(e);
    }
    Ok(())
}

/// Percent-encode a path, leaving '/' and unreserved characters alone.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::listing::test_dir;

    #[test]
    fn trashes_with_info_and_unique_names() {
        let d = test_dir("trash");
        let root = d.join("Trash");
        let src = d.join("my file.txt");
        fs::write(&src, b"abc").unwrap();
        trash_in(&root, &src).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(root.join("files/my file.txt")).unwrap(), b"abc");
        let info = fs::read_to_string(root.join("info/my file.txt.trashinfo")).unwrap();
        assert!(info.starts_with("[Trash Info]\nPath="));
        assert!(info.contains(&format!("Path={}/my%20file.txt\n", d.display())));
        assert!(info.contains("DeletionDate="));

        // Same name again, and a folder.
        fs::write(&src, b"def").unwrap();
        trash_in(&root, &src).unwrap();
        assert_eq!(
            fs::read(root.join("files/my file.txt (2)")).unwrap(),
            b"def"
        );
        assert!(root.join("info/my file.txt (2).trashinfo").exists());
        let dir = d.join("folder");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("x"), b"12345").unwrap();
        trash_in(&root, &dir).unwrap();
        assert!(root.join("files/folder/x").exists());
        assert_eq!(dir_size(&root.join("files"), &AtomicBool::new(false)), 11);

        assert!(trash_in(&root, &d.join("missing")).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn encodes() {
        assert_eq!(percent_encode("/a b/é#"), "/a%20b/%C3%A9%23");
    }
}
