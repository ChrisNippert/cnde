//! Launching clients. Resolution can hit the disk or run helper scripts
//! (`xdg-settings`), so it all happens on a throwaway thread: the compositor
//! thread must never block (ARCHITECTURE.md §4.2).

use std::{
    ffi::{OsStr, OsString},
    io::ErrorKind,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
};

use tracing::{info, warn};

/// Tried in order when `$TERMINAL` is unset.
const TERMINAL_FALLBACKS: &[&str] = &[
    "xdg-terminal-exec",
    "alacritty",
    "foot",
    "kitty",
    "gnome-terminal",
    "konsole",
    "weston-terminal",
];
const BROWSER_FALLBACKS: &[&str] = &["firefox", "chromium", "google-chrome-stable"];

pub fn spawn_terminal(socket: &OsStr) {
    let socket = socket.to_os_string();
    thread::spawn(move || {
        let mut candidates: Vec<Vec<String>> = Vec::new();
        if let Ok(term) = std::env::var("TERMINAL") {
            candidates.push(split_command_line(&term));
        }
        candidates.extend(TERMINAL_FALLBACKS.iter().map(|t| vec![t.to_string()]));
        run_first_available(candidates, &socket);
    });
}

pub fn spawn_browser(socket: &OsStr) {
    let socket = socket.to_os_string();
    thread::spawn(move || {
        let mut candidates: Vec<Vec<String>> = Vec::new();
        if let Some(exec) = default_browser_exec() {
            candidates.push(exec);
        }
        candidates.extend(BROWSER_FALLBACKS.iter().map(|b| vec![b.to_string()]));
        run_first_available(candidates, &socket);
    });
}

pub fn spawn_command_line(command: &str, socket: &OsStr) {
    let socket = socket.to_os_string();
    let argv = split_command_line(command);
    thread::spawn(move || run_first_available(vec![argv], &socket));
}

fn run_first_available(candidates: Vec<Vec<String>>, socket: &OsString) {
    for argv in candidates {
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .env("WAYLAND_DISPLAY", socket)
            // No XWayland yet: keep X11-capable apps from opening on the host instead.
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                info!(%program, "launched");
                // Reap it so it doesn't linger as a zombie.
                let _ = child.wait();
                return;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                warn!(%program, ?err, "failed to launch");
                return;
            }
        }
    }
    warn!("nothing to launch: no candidate program was found");
}

/// Exec line of the user's default browser, from its .desktop file.
fn default_browser_exec() -> Option<Vec<String>> {
    let output = Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .output()
        .ok()?;
    let desktop_id = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if desktop_id.is_empty() {
        return None;
    }
    let path = application_dirs()
        .into_iter()
        .map(|dir| dir.join(&desktop_id))
        .find(|p| p.is_file())?;
    let contents = std::fs::read_to_string(path).ok()?;
    parse_desktop_exec(&contents)
}

fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/share")));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());

    data_home
        .into_iter()
        .chain(
            data_dirs
                .split(':')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
        )
        .map(|d| d.join("applications"))
        .collect()
}

/// The `Exec=` of the `[Desktop Entry]` group, with field codes (%u, %F, …) dropped.
fn parse_desktop_exec(contents: &str) -> Option<Vec<String>> {
    let mut in_entry = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some(exec) = line.strip_prefix("Exec=") {
            let argv: Vec<String> = split_command_line(exec)
                .into_iter()
                .filter(|arg| !(arg.len() == 2 && arg.starts_with('%')))
                .collect();
            return (!argv.is_empty()).then_some(argv);
        }
    }
    None
}

/// Whitespace split that respects double quotes. Enough for Exec lines and
/// `$TERMINAL`; not a shell.
fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' if in_quotes => current.extend(chars.next()),
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_line_drops_field_codes() {
        let desktop = "[Desktop Entry]\nName=Firefox\nExec=/usr/lib/firefox/firefox %u\n\
                       [Desktop Action new-window]\nExec=firefox --new-window %u\n";
        assert_eq!(
            parse_desktop_exec(desktop),
            Some(vec!["/usr/lib/firefox/firefox".to_string()])
        );
    }

    #[test]
    fn split_respects_quotes() {
        assert_eq!(
            split_command_line(r#"foo "bar baz" qux"#),
            vec!["foo", "bar baz", "qux"]
        );
    }
}
