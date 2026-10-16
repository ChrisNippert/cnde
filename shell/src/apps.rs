//! Installed applications: `.desktop` files (freedesktop Desktop Entry
//! spec), searching them, and launching them.
//!
//! The list is read on a background thread at startup, and again when the
//! overview opens if any applications directory changed since.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// Desktop file ID, e.g. "org.gnome.Nautilus.desktop".
    pub id: String,
    pub name: String,
    /// "Web Browser", or the comment if there's no generic name.
    pub description: String,
    pub icon: String,
    exec: String,
    terminal: bool,
    /// StartupWMClass: the app ID its windows use, when that isn't the
    /// desktop file's name.
    wm_class: String,
    keywords: Vec<String>,
    /// Lowercased for matching.
    name_lower: String,
    generic_lower: String,
}

const SETTINGS_ID: &str = "cnde-settings.desktop";
const SETTINGS_ENTRY: &str = include_str!("../../data/cnde-settings.desktop");

/// Every directory that may hold `.desktop` files, most important first:
/// the user's own entries override the system's.
pub fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let mut dirs: Vec<PathBuf> = data_home.into_iter().collect();
    // Flatpak exports, in case the login didn't add them to XDG_DATA_DIRS.
    if let Some(home) = &home {
        dirs.push(home.join(".local/share/flatpak/exports/share"));
    }
    dirs.push("/var/lib/flatpak/exports/share".into());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    let mut seen = Vec::new();
    dirs.into_iter()
        .map(|d| d.join("applications"))
        .filter(|d| {
            let new = !seen.contains(d);
            seen.push(d.clone());
            new
        })
        .collect()
}

/// When each directory last changed, to tell whether a rescan is needed.
pub fn dirs_stamp(dirs: &[PathBuf]) -> Vec<Option<SystemTime>> {
    dirs.iter()
        .map(|d| std::fs::metadata(d).and_then(|m| m.modified()).ok())
        .collect()
}

pub fn load(dirs: &[PathBuf]) -> Vec<App> {
    let started = std::time::Instant::now();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut by_id: HashMap<String, Option<App>> = HashMap::new();
    for dir in dirs {
        let mut files = Vec::new();
        collect(dir, dir, &mut files);
        for (id, path) in files {
            // The first directory to define an ID wins, even if that entry
            // hides the app (that's how users hide system apps).
            if by_id.contains_key(&id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            by_id.insert(id.clone(), parse(&id, &text, &desktop));
        }
    }
    // Our settings app, from next to our own binary, unless installed.
    if !by_id.contains_key(SETTINGS_ID)
        && let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join("cnde-settings")))
            .filter(|exe| exe.is_file())
    {
        let entry =
            SETTINGS_ENTRY.replace("Exec=cnde-settings", &format!("Exec=\"{}\"", exe.display()));
        // OnlyShowIn is for installed copies; this one is always ours.
        by_id.insert(SETTINGS_ID.into(), parse(SETTINGS_ID, &entry, "cnde"));
    }
    let mut apps: Vec<App> = by_id.into_values().flatten().collect();
    apps.sort_by(|a, b| a.name_lower.cmp(&b.name_lower));
    info!(count = apps.len(), elapsed = ?started.elapsed(), "loaded applications");
    apps
}

/// `.desktop` files under `dir`, with IDs relative to `root` ("a/b.desktop"
/// is "a-b.desktop").
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "desktop")
            && let Ok(relative) = path.strip_prefix(root)
        {
            let id = relative.to_string_lossy().replace('/', "-");
            out.push((id, path));
        }
    }
}

/// The `[Desktop Entry]` group of one file; `None` if it shouldn't be shown.
fn parse(id: &str, text: &str, current_desktop: &str) -> Option<App> {
    let mut fields: HashMap<&str, String> = HashMap::new();
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            // Untranslated keys only, for now.
            let key = key.trim();
            if !key.contains('[') {
                fields.insert(key, unescape(value.trim()));
            }
        }
    }
    let get = |key: &str| fields.get(key).map(String::as_str).unwrap_or("");
    let yes = |key: &str| get(key) == "true";
    if get("Type") != "Application" || yes("NoDisplay") || yes("Hidden") || get("Exec").is_empty() {
        return None;
    }
    let ours = |list: &str| {
        list.split(';')
            .any(|d| !d.is_empty() && current_desktop.split(':').any(|c| c == d))
    };
    if fields.contains_key("OnlyShowIn") && !ours(get("OnlyShowIn")) {
        return None;
    }
    if ours(get("NotShowIn")) {
        return None;
    }
    let try_exec = get("TryExec");
    if !try_exec.is_empty() && !in_path(try_exec) {
        return None;
    }
    let name = get("Name").to_string();
    let generic = get("GenericName");
    let description = if generic.is_empty() {
        get("Comment")
    } else {
        generic
    };
    Some(App {
        id: id.to_string(),
        name_lower: name.to_lowercase(),
        generic_lower: generic.to_lowercase(),
        name,
        description: description.to_string(),
        icon: get("Icon").to_string(),
        exec: get("Exec").to_string(),
        terminal: yes("Terminal"),
        wm_class: get("StartupWMClass").to_string(),
        keywords: get("Keywords")
            .split(';')
            .filter(|k| !k.is_empty())
            .map(str::to_lowercase)
            .collect(),
    })
}

impl App {
    /// Whether a window with this Wayland app ID belongs to this app:
    /// "firefox" for firefox.desktop, "org.gnome.Nautilus" for
    /// org.gnome.Nautilus.desktop, or the entry's StartupWMClass.
    pub fn owns(&self, app_id: &str) -> bool {
        if app_id.is_empty() {
            return false;
        }
        let stem = self.id.strip_suffix(".desktop").unwrap_or(&self.id);
        stem.eq_ignore_ascii_case(app_id)
            || self.wm_class.eq_ignore_ascii_case(app_id)
            // Flatpak and reverse-DNS IDs often end in the app ID.
            || stem
                .rsplit('.')
                .next()
                .is_some_and(|last| last.eq_ignore_ascii_case(app_id))
    }
}

/// Desktop entry string escapes: \s \n \t \r \\.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn in_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).exists();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Apps matching `query`, best first. `launches` (id -> count) breaks ties
/// in favour of what you actually use.
pub fn search<'a>(
    apps: &'a [App],
    query: &str,
    launches: &HashMap<String, u32>,
    limit: usize,
) -> Vec<&'a App> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, &App)> = apps
        .iter()
        .filter_map(|app| {
            let score = score(app, &query)?;
            let used = launches.get(&app.id).copied().unwrap_or(0).min(30);
            Some((score + used, app))
        })
        .collect();
    // Highest score first; equal scores alphabetically (the input order).
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().take(limit).map(|(_, app)| app).collect()
}

fn score(app: &App, query: &str) -> Option<u32> {
    let name = &app.name_lower;
    if name == query {
        return Some(200);
    }
    if name.starts_with(query) {
        return Some(150);
    }
    if name
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(query))
    {
        return Some(120);
    }
    if name.contains(query) {
        return Some(90);
    }
    if app.generic_lower.split(' ').any(|w| w.starts_with(query)) {
        return Some(70);
    }
    if app.keywords.iter().any(|k| k.starts_with(query)) {
        return Some(60);
    }
    let program = app
        .exec
        .split_whitespace()
        .next()
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or("")
        .to_lowercase();
    if program.starts_with(query) {
        return Some(50);
    }
    // Letters in order ("ffx" -> Firefox), for sloppy typing.
    let mut letters = name.chars();
    if query.len() >= 2 && query.chars().all(|q| letters.any(|c| c == q)) {
        return Some(20);
    }
    None
}

/// The Exec line with field codes (%f, %U, ...) removed, ready for `sh -c`.
/// The spec's quoting rules match the shell's for everything apps use.
fn command_line(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // "%%" is a literal percent; every other code (files, URLs, icon,
        // name, location) is dropped: we launch with none.
        if chars.next() == Some('%') {
            out.push('%');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Start `app`, fully detached: it outlives the shell, and its output goes
/// nowhere (apps that log a lot would otherwise fill our log).
pub fn launch(app: &App, terminal: &str) {
    let line = command_line(&app.exec);
    let line = if app.terminal {
        format!("{terminal} -e sh -c '{}'", line.replace('\'', r"'\''"))
    } else {
        line
    };
    debug!(id = %app.id, command = %line, "launching");
    // The outer shell backgrounds the app and exits at once, so the app is
    // reparented away from us and we never have to reap it.
    let result = Command::new("sh")
        .arg("-c")
        .arg(format!("{line} </dev/null >/dev/null 2>&1 &"))
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(err) = result {
        warn!(id = %app.id, %err, "can't launch");
    }
}

/// cnde-settings: the one next to our binary, else from PATH. `None` if
/// it isn't installed (it's a separate package), and then nothing offers
/// to open it.
pub fn settings_program() -> Option<std::ffi::OsString> {
    static PROGRAM: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();
    PROGRAM
        .get_or_init(|| {
            let sibling = std::env::current_exe()
                .ok()
                .and_then(|exe| Some(exe.parent()?.join("cnde-settings")))
                .filter(|exe| exe.is_file());
            let on_path = || {
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|dir| dir.join("cnde-settings"))
                        .find(|exe| exe.is_file())
                })
            };
            sibling.or_else(on_path).map(|exe| exe.into_os_string())
        })
        .clone()
}

pub fn settings_installed() -> bool {
    settings_program().is_some()
}

/// Start cnde-settings (on `page`, e.g. "dock").
pub fn open_settings(page: Option<&str>) {
    let Some(program) = settings_program() else {
        return;
    };
    let result = Command::new(&program)
        .args(page)
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        // Reaped on a thread so it doesn't linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(err) => warn!(?program, %err, "can't open settings"),
    }
}

/// How often each app was launched, kept in `$XDG_STATE_HOME/cnde/launches`
/// as "id count" lines.
pub struct LaunchCounts {
    path: Option<PathBuf>,
    pub counts: HashMap<String, u32>,
}

impl LaunchCounts {
    pub fn load() -> Self {
        let path = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .map(|dir| dir.join("cnde").join("launches"));
        let counts = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| {
                        let (id, count) = line.rsplit_once(' ')?;
                        Some((id.to_string(), count.parse().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { path, counts }
    }

    pub fn record(&mut self, id: &str) {
        *self.counts.entry(id.to_string()).or_insert(0) += 1;
        let Some(path) = &self.path else {
            return;
        };
        let mut text = String::new();
        for (id, count) in &self.counts {
            text.push_str(&format!("{id} {count}\n"));
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(err) = std::fs::write(path, text) {
            debug!(%err, "can't save launch counts");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(text: &str) -> Option<App> {
        parse("test.desktop", text, "cnde")
    }

    const FIREFOX: &str = "[Desktop Entry]\nType=Application\nName=Firefox\n\
        GenericName=Web Browser\nName[de]=Feuerfuchs\nExec=firefox %u\nIcon=firefox\n\
        Keywords=Internet;WWW;\n\n[Desktop Action new-window]\nName=New Window\n";

    #[test]
    fn parses_the_main_group_only() {
        let app = app(FIREFOX).unwrap();
        assert_eq!(app.name, "Firefox");
        assert_eq!(app.description, "Web Browser");
        assert_eq!(app.icon, "firefox");
        assert_eq!(command_line(&app.exec), "firefox");
    }

    #[test]
    fn hidden_and_foreign_entries_are_skipped() {
        assert!(
            app("[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n").is_none()
        );
        assert!(
            app("[Desktop Entry]\nType=Application\nName=X\nExec=x\nOnlyShowIn=KDE;\n").is_none()
        );
        assert!(
            app("[Desktop Entry]\nType=Application\nName=X\nExec=x\nOnlyShowIn=cnde;\n").is_some()
        );
        assert!(app("[Desktop Entry]\nType=Link\nName=X\nURL=x\n").is_none());
    }

    #[test]
    fn field_codes_are_removed() {
        assert_eq!(command_line("foo --bar %U"), "foo --bar");
        assert_eq!(command_line("printf 100%% %f"), "printf 100%");
        assert_eq!(
            command_line("flatpak run --command=x org.App @@u %U @@"),
            "flatpak run --command=x org.App @@u @@"
        );
    }

    #[test]
    fn ranking() {
        let apps: Vec<App> = [
            ("Files", "File Manager", "nautilus"),
            ("Firefox", "Web Browser", "firefox"),
            ("GNU Image Manipulation Program", "Image Editor", "gimp"),
        ]
        .iter()
        .map(|(name, generic, exec)| {
            app(&format!(
                "[Desktop Entry]\nType=Application\nName={name}\nGenericName={generic}\nExec={exec}\n"
            ))
            .unwrap()
        })
        .collect();
        let none = HashMap::new();
        let names = |q: &str| -> Vec<String> {
            search(&apps, q, &none, 10)
                .iter()
                .map(|a| a.name.clone())
                .collect()
        };
        assert_eq!(names("fi"), ["Files", "Firefox"]);
        assert_eq!(names("browser"), ["Firefox"]);
        assert_eq!(names("image"), ["GNU Image Manipulation Program"]);
        assert_eq!(names("gimp"), ["GNU Image Manipulation Program"]);
        assert!(names("zzz").is_empty());
    }
}
