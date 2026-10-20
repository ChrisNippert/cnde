//! A window's app ID to its icon name: the matching desktop entry's
//! `Icon=`, else the app ID itself (which is often the icon's name too).
//! A small version of the shell's lookup.

use std::path::PathBuf;

/// Where desktop entries live, the user's own first.
fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/share")));
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
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

/// `Icon=` and `StartupWMClass=` of a desktop entry's main group.
fn fields(text: &str) -> (String, String) {
    let (mut icon, mut class) = (String::new(), String::new());
    let mut in_main = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
        } else if in_main && let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Icon" => icon = value.trim().to_string(),
                "StartupWMClass" => class = value.trim().to_string(),
                _ => {}
            }
        }
    }
    (icon, class)
}

pub fn icon_name(app_id: &str) -> String {
    icon_name_in(app_id, &application_dirs())
}

fn icon_name_in(app_id: &str, dirs: &[PathBuf]) -> String {
    if app_id.is_empty() {
        return String::new();
    }
    // The entry named after the app ID, then any whose StartupWMClass is it.
    for dir in dirs {
        for name in [app_id.to_string(), app_id.to_lowercase()] {
            if let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.desktop")))
                && let (icon, _) = fields(&text)
                && !icon.is_empty()
            {
                return icon;
            }
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                let (icon, class) = fields(&text);
                if !icon.is_empty() && class.eq_ignore_ascii_case(app_id) {
                    return icon;
                }
            }
        }
    }
    app_id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_icon_from_the_matching_entry() {
        let dir = std::env::temp_dir().join(format!("cnde-appicon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("org.example.Foo.desktop"),
            "[Desktop Entry]\nName=Foo\nIcon=foo-icon\n[Desktop Action x]\nIcon=other\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("bar.desktop"),
            "[Desktop Entry]\nIcon=bar-icon\nStartupWMClass=BarWindow\n",
        )
        .unwrap();
        let dirs = [dir.clone()];
        assert_eq!(icon_name_in("org.example.Foo", &dirs), "foo-icon");
        assert_eq!(icon_name_in("barwindow", &dirs), "bar-icon");
        assert_eq!(icon_name_in("unknown", &dirs), "unknown");
        assert_eq!(icon_name_in("", &dirs), "");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
