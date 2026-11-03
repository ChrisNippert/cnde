//! What the window remembers between launches, kept in
//! `$XDG_STATE_HOME/cnde/settings` (default `~/.local/state`): whether the
//! global "Advanced" toggle is on. One `key=value` per line.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    pub advanced: bool,
}

impl State {
    fn parse(text: &str) -> State {
        let mut state = State::default();
        for line in text.lines() {
            if let Some(("advanced", v)) = line.split_once('=') {
                state.advanced = v.trim() == "true";
            }
        }
        state
    }

    fn render(&self) -> String {
        format!("advanced={}\n", self.advanced)
    }
}

fn path(state_home: &Path) -> PathBuf {
    state_home.join("cnde").join("settings")
}

/// `$XDG_STATE_HOME`, or `~/.local/state`.
fn state_home() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
}

fn load_from(state_home: &Path) -> State {
    fs::read_to_string(path(state_home)).map_or_else(|_| State::default(), |t| State::parse(&t))
}

fn save_to(state_home: &Path, state: &State) -> io::Result<()> {
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

/// Save `state`; a failure is only logged.
pub fn save(state: &State) {
    let Some(home) = state_home() else { return };
    if let Err(e) = save_to(&home, state) {
        tracing::warn!("saving the settings state failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for advanced in [true, false] {
            let s = State { advanced };
            assert_eq!(State::parse(&s.render()), s);
        }
        assert_eq!(State::parse("junk"), State::default());
    }
}
