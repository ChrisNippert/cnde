//! Event sounds from the freedesktop sound theme, played with whichever of
//! pw-play and paplay is installed. Nothing plays if neither is, or the
//! theme isn't installed (sound-theme-freedesktop).

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    thread,
};

use tracing::debug;

/// Play the theme sound `name` (e.g. "audio-volume-change") without waiting.
pub fn play(name: &str) {
    let Some(path) = find(name) else {
        debug!(name, "no such sound");
        return;
    };
    let _ = thread::Builder::new()
        .name("sound".into())
        .spawn(move || {
            for player in ["pw-play", "paplay"] {
                let played = Command::new(player)
                    .arg(&path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if played.is_ok() {
                    return;
                }
            }
        });
}

/// The sound's file in the data directories, user's first.
fn find(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(dirs.split(':').map(PathBuf::from))
        .flat_map(|dir| {
            ["oga", "ogg", "wav"]
                .map(|ext| dir.join(format!("sounds/freedesktop/stereo/{name}.{ext}")))
        })
        .find(|path| path.is_file())
}
