//! Taking back the last few things done to files.

use std::{
    ffi::OsString,
    fs, io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use crate::{clipboard::percent_decode_bytes, model, transfer};

/// How many operations can be undone.
const DEPTH: usize = 20;

/// Something done, with what it takes to take it back.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A rename in `dir`, from one name to another.
    Rename {
        dir: PathBuf,
        from: String,
        to: String,
    },
    /// (original, where it went).
    Moved(Vec<(PathBuf, PathBuf)>),
    /// The copies that were made.
    Copied(Vec<PathBuf>),
    NewFolder(PathBuf),
    /// Where the file used to be.
    Trashed(PathBuf),
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Action::Rename { .. } => "rename",
            Action::Moved(_) => "move",
            Action::Copied(_) => "copy",
            Action::NewFolder(_) => "new folder",
            Action::Trashed(_) => "trash",
        }
    }
}

/// The most recent actions, newest last.
#[derive(Default)]
pub struct Stack(Vec<Action>);

impl Stack {
    pub fn push(&mut self, action: Action) {
        self.0.push(action);
        if self.0.len() > DEPTH {
            self.0.remove(0);
        }
    }

    pub fn pop(&mut self) -> Option<Action> {
        self.0.pop()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

fn taken(path: &Path) -> io::Error {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    io::Error::other(format!("“{name}” is in the way"))
}

/// Take `action` back, using the trash at `trash_root`. Says what it did.
pub fn undo(action: &Action, trash_root: &Path) -> io::Result<String> {
    match action {
        Action::Rename { dir, from, to } => {
            let (a, b) = (dir.join(to), dir.join(from));
            if fs::symlink_metadata(&b).is_ok() {
                return Err(taken(&b));
            }
            fs::rename(a, b)?;
        }
        Action::Moved(pairs) => {
            for (orig, now) in pairs {
                if fs::symlink_metadata(orig).is_ok() {
                    return Err(taken(orig));
                }
                transfer::move_exact(now, orig)?;
            }
        }
        Action::Copied(copies) => {
            for copy in copies {
                model::trash_in(trash_root, copy)?;
            }
        }
        Action::NewFolder(path) => {
            fs::remove_dir(path).map_err(|e| {
                if e.kind() == io::ErrorKind::DirectoryNotEmpty {
                    io::Error::other("The new folder isn't empty any more")
                } else {
                    e
                }
            })?;
        }
        Action::Trashed(orig) => restore(trash_root, orig)?,
    }
    Ok(format!("Undid {}", action.label()))
}

/// Put the newest trashed item that came from `orig` back there.
fn restore(root: &Path, orig: &Path) -> io::Result<()> {
    let (files, info) = (root.join("files"), root.join("info"));
    let mut best: Option<(String, String)> = None;
    for entry in fs::read_dir(&info)?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".trashinfo") else {
            continue;
        };
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let (mut path, mut date) = (None, String::new());
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("Path=") {
                path = Some(PathBuf::from(OsString::from_vec(percent_decode_bytes(v))));
            } else if let Some(v) = line.strip_prefix("DeletionDate=") {
                date = v.to_string();
            }
        }
        if path.as_deref() == Some(orig)
            && best
                .as_ref()
                .is_none_or(|(d, n)| (&date, name.len(), name) >= (d, n.len(), n.as_str()))
        {
            best = Some((date, name.to_string()));
        }
    }
    let Some((_, name)) = best else {
        return Err(io::Error::other("It isn't in the Trash any more"));
    };
    if fs::symlink_metadata(orig).is_ok() {
        return Err(taken(orig));
    }
    transfer::move_exact(&files.join(&name), orig)?;
    let _ = fs::remove_file(info.join(format!("{name}.trashinfo")));
    Ok(())
}

impl crate::App {
    pub(crate) fn record(&mut self, action: Action) {
        self.undo.push(action);
    }

    /// Ctrl+Z: take back the last thing, on a thread.
    pub(crate) fn undo_last(&mut self) {
        let Some(action) = self.undo.pop() else {
            self.show_notice("Nothing to undo".into());
            return;
        };
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let result = undo(&action, &model::trash_dir()).map_err(|e| format!("Can't undo: {e}"));
            let _ = tx.send(crate::Event::Xfer(transfer::Msg::Undone(result)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_dir;

    #[test]
    fn undoes_rename_and_move() {
        let d = test_dir("undo-rm");
        let trash = d.join("Trash");
        fs::write(d.join("new"), b"x").unwrap();
        let rename = Action::Rename {
            dir: d.clone(),
            from: "old".into(),
            to: "new".into(),
        };
        assert_eq!(undo(&rename, &trash).unwrap(), "Undid rename");
        assert!(d.join("old").exists() && !d.join("new").exists());
        // Not if the old name has been taken.
        fs::write(d.join("new"), b"y").unwrap();
        assert!(undo(&rename, &trash).is_err());

        fs::create_dir(d.join("to")).unwrap();
        fs::write(d.join("to/f"), b"z").unwrap();
        let moved = Action::Moved(vec![(d.join("from-f"), d.join("to/f"))]);
        assert_eq!(undo(&moved, &trash).unwrap(), "Undid move");
        assert_eq!(fs::read(d.join("from-f")).unwrap(), b"z");
        assert!(!d.join("to/f").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn undoes_trash_and_copy_and_folder() {
        let d = test_dir("undo-trash");
        let trash = d.join("Trash");
        let f = d.join("a b.txt");
        fs::write(&f, b"1").unwrap();
        model::trash_in(&trash, &f).unwrap();
        // The same name trashed again; the newest comes back.
        fs::write(&f, b"2").unwrap();
        model::trash_in(&trash, &f).unwrap();
        assert!(!f.exists());
        undo(&Action::Trashed(f.clone()), &trash).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"2");
        assert!(trash.join("files/a b.txt").exists());
        // In the way: refused, nothing lost.
        assert!(undo(&Action::Trashed(f.clone()), &trash).is_err());
        assert!(trash.join("files/a b.txt").exists());

        // A paste goes to the trash.
        let copy = d.join("a b (copy).txt");
        fs::write(&copy, b"c").unwrap();
        undo(&Action::Copied(vec![copy.clone()]), &trash).unwrap();
        assert!(!copy.exists());

        let dir = d.join("New Folder");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("x"), b"").unwrap();
        assert!(undo(&Action::NewFolder(dir.clone()), &trash).is_err());
        fs::remove_file(dir.join("x")).unwrap();
        undo(&Action::NewFolder(dir.clone()), &trash).unwrap();
        assert!(!dir.exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stack_is_short() {
        let mut s = Stack::default();
        for i in 0..30 {
            s.push(Action::NewFolder(PathBuf::from(format!("/x{i}"))));
        }
        assert_eq!(s.len(), DEPTH);
        assert_eq!(s.pop(), Some(Action::NewFolder("/x29".into())));
    }
}
