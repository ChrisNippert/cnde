//! Launching clients. Resolution can hit the disk or run helper scripts
//! (`xdg-settings`), so it all happens on a throwaway thread: the compositor
//! thread must never block (ARCHITECTURE.md §4.2).

use std::{
    ffi::OsString,
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

/// The displays launched apps connect to.
#[derive(Debug, Clone)]
pub struct ClientEnv {
    pub wayland_display: OsString,
    /// `:N` once XWayland is set up; X11 apps start it on first connect.
    pub x11_display: Option<String>,
}

pub fn spawn_terminal(env: &ClientEnv) {
    let env = env.clone();
    thread::spawn(move || {
        let mut candidates: Vec<Vec<String>> = Vec::new();
        if let Ok(term) = std::env::var("TERMINAL") {
            candidates.push(split_command_line(&term));
        }
        candidates.extend(TERMINAL_FALLBACKS.iter().map(|t| vec![t.to_string()]));
        run_first_available(candidates, &env);
    });
}

pub fn spawn_browser(env: &ClientEnv) {
    let env = env.clone();
    thread::spawn(move || {
        let mut candidates: Vec<Vec<String>> = Vec::new();
        if let Some(exec) = default_browser_exec() {
            candidates.push(exec);
        }
        candidates.extend(BROWSER_FALLBACKS.iter().map(|b| vec![b.to_string()]));
        run_first_available(candidates, &env);
    });
}

/// Run cnde-shell for the whole session, restarting it if it crashes
/// (ARCHITECTURE.md §2). Restarts back off so a crash loop can't spin, and
/// the backoff resets once it has stayed up a while.
/// `shell` is `autostart.shell`: a command line, or empty for none.
pub fn supervise_shell(env: &ClientEnv, shell: &str) {
    let env = env.clone();
    let mut argv = split_command_line(shell);
    if argv.is_empty() {
        info!("no shell configured (autostart.shell)");
        return;
    }
    prefer_sibling(&mut argv);
    let (program, args) = (argv[0].clone(), argv[1..].to_vec());
    thread::spawn(move || {
        let mut backoff = std::time::Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            let mut command = Command::new(&program);
            command
                .args(&args)
                .env("WAYLAND_DISPLAY", &env.wayland_display)
                .env_remove("WAYLAND_SOCKET")
                .stdin(Stdio::null());
            match &env.x11_display {
                Some(display) => command.env("DISPLAY", display),
                None => command.env_remove("DISPLAY"),
            };
            let status = match command.spawn() {
                Ok(mut child) => child.wait(),
                Err(err) => {
                    warn!(
                        ?program,
                        ?err,
                        "can't start the shell; no panel this session"
                    );
                    return;
                }
            };
            if started.elapsed() > std::time::Duration::from_secs(30) {
                backoff = std::time::Duration::from_secs(1);
            }
            warn!(?status, ?backoff, ?program, "the shell exited; restarting");
            thread::sleep(backoff);
            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
        }
    });
}

/// One of ours (cnde-shell, cnde-lock, ...): next to our own binary first
/// (a dev build run from target/), then $PATH.
fn prefer_sibling(argv: &mut [String]) {
    let Some(name) = argv.first().filter(|name| name.starts_with("cnde-")) else {
        return;
    };
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(name)))
        .filter(|path| path.is_file())
    {
        argv[0] = sibling.to_string_lossy().into_owned();
    }
}

pub fn spawn_command_line(command: &str, env: &ClientEnv) {
    let env = env.clone();
    let mut argv = split_command_line(command);
    prefer_sibling(&mut argv);
    thread::spawn(move || run_first_available(vec![argv], &env));
}

fn run_first_available(candidates: Vec<Vec<String>>, env: &ClientEnv) {
    for argv in candidates {
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .env("WAYLAND_DISPLAY", &env.wayland_display)
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null());
        // Our X server, or none: never the host's (nested), or X11-capable
        // apps would open on the host desktop instead.
        match &env.x11_display {
            Some(display) => command.env("DISPLAY", display),
            None => command.env_remove("DISPLAY"),
        };
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

/// Whitespace split that respects quotes: double quotes (with backslash
/// escapes, as in .desktop Exec lines) and single quotes (literal, as in a
/// shell). Enough for Exec lines, `$TERMINAL` and config commands; not a shell.
fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => current.extend(chars.next()),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            (_, c) => current.push(c),
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
        assert_eq!(
            split_command_line(r#"sh -c 'sleep 2; echo "hi"'"#),
            vec!["sh", "-c", r#"sleep 2; echo "hi""#]
        );
    }
}
