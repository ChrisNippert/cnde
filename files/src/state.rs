//! What the file manager remembers between runs: hidden files, the sort
//! and folders-first. A few `key = value` lines in
//! `$XDG_STATE_HOME/cnde/files` (default `~/.local/state`), written when
//! one of them changes.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use crate::{folder::Folder, model::SortKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub show_hidden: bool,
    pub sort: SortKey,
    pub descending: bool,
    pub folders_first: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            show_hidden: false,
            sort: SortKey::Name,
            descending: false,
            folders_first: true,
        }
    }
}

const KEYS: [(&str, SortKey); 4] = [
    ("name", SortKey::Name),
    ("size", SortKey::Size),
    ("modified", SortKey::Modified),
    ("kind", SortKey::Kind),
];

impl State {
    pub fn of(folder: &Folder) -> Self {
        Self {
            show_hidden: folder.show_hidden,
            sort: folder.sort,
            descending: folder.descending,
            folders_first: folder.folders_first,
        }
    }

    /// Unknown keys and values are skipped, leaving the defaults.
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "show_hidden" => s.show_hidden = v == "true",
                "descending" => s.descending = v == "true",
                "folders_first" => s.folders_first = v != "false",
                "sort" => {
                    if let Some((_, key)) = KEYS.iter().find(|(name, _)| *name == v) {
                        s.sort = *key;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn render(&self) -> String {
        let sort = KEYS.iter().find(|(_, k)| *k == self.sort).map_or("name", |(n, _)| n);
        format!(
            "show_hidden = {}\nsort = {sort}\ndescending = {}\nfolders_first = {}\n",
            self.show_hidden, self.descending, self.folders_first
        )
    }

    pub fn apply(&self, folder: &mut Folder) {
        folder.show_hidden = self.show_hidden;
        folder.sort = self.sort;
        folder.descending = self.descending;
        folder.folders_first = self.folders_first;
    }
}

fn path(state_home: &Path) -> PathBuf {
    state_home.join("cnde").join("files")
}

/// `$XDG_STATE_HOME`, or `~/.local/state`.
fn state_home() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
}

pub fn load_from(state_home: &Path) -> State {
    fs::read_to_string(path(state_home)).map_or_else(|_| State::default(), |t| State::parse(&t))
}

pub fn save_to(state_home: &Path, state: &State) -> io::Result<()> {
    let path = path(state_home);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, state.render())?;
    fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

pub fn load() -> State {
    state_home().map_or_else(State::default, |h| load_from(&h))
}

/// Save `folder`'s settings; a failure is only logged.
pub fn save(folder: &Folder) {
    let Some(home) = state_home() else { return };
    if let Err(e) = save_to(&home, &State::of(folder)) {
        tracing::warn!("saving the file manager state failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = State {
            show_hidden: true,
            sort: SortKey::Modified,
            descending: true,
            folders_first: false,
        };
        assert_eq!(State::parse(&s.render()), s);
    }

    #[test]
    fn garbage_keeps_defaults() {
        assert_eq!(State::parse("sort = bogus\nnonsense\nx = y"), State::default());
    }

    #[test]
    fn saved_and_loaded() {
        let dir = env::temp_dir().join(format!("cnde-files-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(load_from(&dir), State::default());
        let s = State {
            sort: SortKey::Size,
            ..State::default()
        };
        save_to(&dir, &s).unwrap();
        assert_eq!(load_from(&dir), s);
        let _ = fs::remove_dir_all(&dir);
    }
}
