//! New files: the Templates folder and finding a name that is free.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// The user's Templates folder: `XDG_TEMPLATES_DIR` from `user-dirs.dirs`
/// under `config_home`, else `~/Templates`. `None` when it is switched off
/// (set to `$HOME`).
pub fn templates_dir(config_home: &Path, home: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(config_home.join("user-dirs.dirs")).unwrap_or_default();
    let dir = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("XDG_TEMPLATES_DIR="))
        .map(|v| v.trim().trim_matches('"'))
        .map(|v| match v.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(v),
        })
        .unwrap_or_else(|| home.join("Templates"));
    (dir != home).then_some(dir)
}

/// The files in `dir` (name, path), sorted by name; hidden files and
/// folders are left out.
pub fn list_templates(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = read
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(n, _)| !n.starts_with('.'))
        .collect();
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out
}

/// `name`, or `name 2`, `name 3`… (before the extension) until `taken`
/// says no.
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    (2..)
        .map(|n| format!("{stem} {n}{ext}"))
        .find(|c| !taken(c))
        .unwrap_or_default()
}

/// Make the file in `dir`: a copy of `template`, or empty. Returns the
/// name it got. Never replaces anything.
pub fn create(dir: &Path, base: &str, template: Option<&Path>) -> io::Result<String> {
    for _ in 0..100 {
        let name = free_name(base, |n| fs::symlink_metadata(dir.join(n)).is_ok());
        let path = dir.join(&name);
        let made = match template {
            None => fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map(|_| ()),
            Some(src) => copy_new(src, &path),
        };
        match made {
            Ok(()) => return Ok(name),
            // Lost a race for the name: look again.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free name"))
}

fn copy_new(src: &Path, dst: &Path) -> io::Result<()> {
    let mut from = fs::File::open(src)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    io::copy(&mut from, &mut to)?;
    to.set_permissions(from.metadata()?.permissions())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cnde-newfile-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_count_up() {
        let taken = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        assert_eq!(free_name("Untitled", taken(&[])), "Untitled");
        assert_eq!(free_name("Untitled", taken(&["Untitled"])), "Untitled 2");
        assert_eq!(
            free_name("Untitled", taken(&["Untitled", "Untitled 2"])),
            "Untitled 3"
        );
        assert_eq!(
            free_name("Report.odt", taken(&["Report.odt"])),
            "Report 2.odt"
        );
        assert_eq!(free_name(".rc", taken(&[".rc"])), ".rc 2");
    }

    #[test]
    fn creating_never_replaces() {
        let d = temp("create");
        assert_eq!(create(&d, "Untitled", None).unwrap(), "Untitled");
        assert_eq!(create(&d, "Untitled", None).unwrap(), "Untitled 2");
        let tpl = d.join("tpl.txt");
        fs::write(&tpl, "hello").unwrap();
        assert_eq!(create(&d, "tpl.txt", Some(&tpl)).unwrap(), "tpl 2.txt");
        assert_eq!(fs::read_to_string(d.join("tpl 2.txt")).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&tpl).unwrap(), "hello");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn templates_are_listed() {
        let root = temp("tpl");
        let home = root.join("home");
        let cfg = root.join("cfg");
        fs::create_dir_all(home.join("Templates/sub")).unwrap();
        fs::create_dir_all(&cfg).unwrap();
        for n in ["b.md", "A.txt", ".hidden"] {
            fs::write(home.join("Templates").join(n), "").unwrap();
        }
        // No user-dirs.dirs: ~/Templates.
        let dir = templates_dir(&cfg, &home).unwrap();
        assert_eq!(dir, home.join("Templates"));
        let names: Vec<_> = list_templates(&dir).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["A.txt", "b.md"]);
        // user-dirs.dirs says where.
        fs::write(
            cfg.join("user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_TEMPLATES_DIR=\"$HOME/Tpl\"\n",
        )
        .unwrap();
        assert_eq!(templates_dir(&cfg, &home).unwrap(), home.join("Tpl"));
        // Pointing at $HOME turns it off.
        fs::write(cfg.join("user-dirs.dirs"), "XDG_TEMPLATES_DIR=\"$HOME/\"\n").unwrap();
        assert_eq!(templates_dir(&cfg, &home), None);
        assert!(list_templates(&root.join("nope")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
