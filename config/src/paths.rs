//! Where cnde finds files it doesn't own the layout of: the freedesktop
//! data directories, and its own programs and data wherever they were
//! installed.

use std::path::{Path, PathBuf};

/// Where this build was told it would be installed (`CNDE_PREFIX=/opt/cnde
/// cargo build`). `/usr` if not told, which is the distro packages' layout.
pub const PREFIX: &str = match option_env!("CNDE_PREFIX") {
    Some(prefix) => prefix,
    None => "/usr",
};

/// `$XDG_DATA_HOME`, by default `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// `$XDG_DATA_DIRS`, by default `/usr/local/share:/usr/share`.
pub fn data_dirs() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// cnde's own `share` directory: next to the running binary
/// (`<prefix>/bin/..`) if it has one, else the prefix it was built for.
/// A `~/.local` or `/opt` install isn't in `$XDG_DATA_DIRS`, so lookups of
/// cnde's own data (its icons) add this.
pub fn own_share() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("share")))
        .filter(|share| share.join("icons/cnde").is_dir())
        .unwrap_or_else(|| Path::new(PREFIX).join("share"))
}

/// Every place to look for shared data: the user's, then the system's,
/// then cnde's own (without repeats).
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = data_home().into_iter().chain(data_dirs()).collect();
    let own = own_share();
    if !dirs.contains(&own) {
        dirs.push(own);
    }
    dirs
}

/// One of cnde's programs: next to our own binary first (a dev build run
/// from target/, or any install that keeps them together), else wherever
/// `$PATH` has it.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(name)))
        .filter(|path| path.is_file());
    sibling.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    })
}
