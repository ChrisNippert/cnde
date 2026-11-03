//! Settings in the overview's search: pages and the main options, so
//! typing "night light" or "touchpad" offers them next to the apps. The
//! list is kept here (the shell doesn't link the settings app), and
//! `cnde-settings` opens a page by name or finds an option by its text.

/// A Settings page or option the search can open.
#[derive(Debug, PartialEq)]
pub struct Entry {
    /// What the result is called.
    pub title: &'static str,
    /// Other words that find it.
    pub keywords: &'static [&'static str],
    /// The page it's on, as `cnde-settings` takes the name.
    pub page: &'static str,
    /// The page's title, shown under the result.
    pub page_title: &'static str,
    /// For an option: what `cnde-settings --search` looks for. `None` for
    /// a page.
    pub search: Option<&'static str>,
}

impl Entry {
    /// The result's second line.
    pub fn subtitle(&self) -> String {
        match self.search {
            Some(_) => format!("{} \u{2014} Settings", self.page_title),
            None => "Settings".into(),
        }
    }

    /// Stable across runs; results carry it like an app's desktop ID.
    pub fn id(&self) -> String {
        match self.search {
            Some(_) => format!("{ID_PREFIX}option:{}", self.title),
            None => format!("{ID_PREFIX}page:{}", self.page),
        }
    }
}

pub const ID_PREFIX: &str = "cnde-settings:";

/// The icon on every Settings result.
pub const ICON: &str = "preferences-system";

const fn page(title: &'static str, page: &'static str, keywords: &'static [&'static str]) -> Entry {
    Entry {
        title,
        keywords,
        page,
        page_title: title,
        search: None,
    }
}

const fn option(
    title: &'static str,
    page: &'static str,
    page_title: &'static str,
    search: &'static str,
    keywords: &'static [&'static str],
) -> Entry {
    Entry {
        title,
        keywords,
        page,
        page_title,
        search: Some(search),
    }
}

/// Pages first, then the options that people look for.
pub static ENTRIES: &[Entry] = &[
    page("Appearance", "appearance", &["theme", "colours", "colors", "wallpaper", "background"]),
    page("Top Bar", "top bar", &["panel", "clock", "tray"]),
    page("Dock", "dock", &["launcher", "favorites"]),
    page("Windows", "windows", &["snap", "title bars", "workspaces"]),
    page("Tiling", "tiling", &["tile", "gaps", "master", "split"]),
    page("Wi-Fi", "wifi", &["network", "internet", "wireless"]),
    page("Bluetooth", "bluetooth", &["devices", "pair"]),
    page("Displays", "displays", &["monitor", "screen", "resolution", "scale"]),
    page("Sound", "sound", &["audio", "volume", "speakers", "microphone"]),
    page("Mouse & Touchpad", "mouse & touchpad", &["pointer", "trackpad", "cursor"]),
    page("Keyboard", "keyboard", &["shortcuts", "typing"]),
    page("Language & Input", "language", &["layout", "input method", "japanese", "fcitx", "keyboard layout"]),
    page("Power", "power", &["battery", "suspend", "sleep"]),
    page("Default Apps", "default apps", &["browser", "terminal", "file manager"]),
    page("Accessibility", "accessibility", &["zoom", "contrast", "motion"]),
    option("Dark style", "appearance", "Appearance", "Style", &["dark mode", "light", "theme"]),
    option("Dark mode schedule", "appearance", "Appearance", "Switch automatically", &["sunset", "automatic"]),
    option("Accent colour", "appearance", "Appearance", "Accent colour", &["accent color", "highlight"]),
    option("Wallpaper", "appearance", "Appearance", "Picture", &["background", "picture", "image"]),
    option("Corner radius", "appearance", "Appearance", "Corner radius", &["rounded", "round"]),
    option("Material", "appearance", "Appearance", "Material", &["frosted", "blur", "solid", "transparency"]),
    option("Frost", "appearance", "Appearance", "Frost", &["blur", "blurred", "glass"]),
    option("Live blur", "appearance", "Appearance", "Live blur", &["frosted", "blur", "transparency", "gpu"]),
    option("Night light", "displays", "Displays", "Night light", &["blue light", "warm", "colour temperature", "color temperature"]),
    option("Resolution", "displays", "Displays", "Resolution", &["refresh rate", "scale", "fractional"]),
    option("Display rotation", "displays", "Displays", "Rotation", &["orientation", "portrait"]),
    option("Touchpad", "mouse & touchpad", "Mouse & Touchpad", "Tap to click", &["tap", "trackpad", "click"]),
    option("Natural scrolling", "mouse & touchpad", "Mouse & Touchpad", "Natural scrolling", &["scroll", "reverse"]),
    option("Pointer speed", "mouse & touchpad", "Mouse & Touchpad", "Pointer speed", &["mouse speed", "sensitivity"]),
    option("Keyboard layout", "language", "Language & Input", "Layout", &["language", "input source"]),
    option("Key repeat", "keyboard", "Keyboard", "Repeat speed", &["repeat delay"]),
    option("Tiling", "tiling", "Tiling", "Tiling", &["tile", "master"]),
    option("Window gaps", "tiling", "Tiling", "Gaps", &["tiling gaps", "spacing"]),
    option("New windows open at the pointer", "tiling", "Tiling", "New windows open at the pointer", &["tiling", "split", "where new windows open"]),
    option("Rounded corners", "tiling", "Tiling", "Rounded corners", &["tiled", "shadows", "tiles"]),
    option("Snap zones", "windows", "Windows", "Snap zone", &["edge", "half"]),
    option("Focus follows mouse", "windows", "Windows", "Focus follows mouse", &["hover", "focus on hover", "sloppy focus"]),
    option("Title bar buttons", "windows", "Windows", "Title bar buttons", &["minimize", "maximize", "close"]),
    option("Workspaces", "windows", "Windows", "Workspaces", &["virtual desktops", "monitors"]),
    option("Clock format", "top bar", "Top Bar", "Clock format", &["time", "date", "24 hour"]),
    option("Top bar style", "top bar", "Top Bar", "Style", &["islands", "clear", "transparent"]),
    option("Tray icons", "top bar", "Top Bar", "Tray icons", &["status icons", "systray"]),
    option("On-screen keyboard", "accessibility", "Accessibility", "On-screen keyboard", &["touch keyboard", "virtual keyboard"]),
    option("Dock position", "dock", "Dock", "Position", &["bottom", "left", "right"]),
    option("Dock icon size", "dock", "Dock", "Icon size", &["size"]),
    option("Event sounds", "sound", "Sound", "Event sounds", &["alert", "notification sound"]),
    option("Default terminal", "default apps", "Default Apps", "Terminal", &["terminal emulator"]),
];

// Scores sit between the apps' strong name matches (120 and up) and their
// weak ones (70 and below), so "files" finds the Files app first and
// "night" finds Night light before an app known only by a keyword.
const TITLE_START: u32 = 100;
const TITLE_WORD: u32 = 95;
const TITLE_HAS: u32 = 85;
const KEYWORD: u32 = 75;
/// Pages edge out options of the same score.
const PAGE_BONUS: u32 = 2;

fn score(entry: &Entry, query: &str) -> Option<u32> {
    let title = entry.title.to_lowercase();
    let base = if title.starts_with(query) {
        TITLE_START
    } else if title
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with(query))
    {
        TITLE_WORD
    } else if title.contains(query) {
        TITLE_HAS
    } else if entry
        .keywords
        .iter()
        .any(|k| k.split(' ').any(|w| w.starts_with(query)) || k.starts_with(query))
    {
        KEYWORD
    } else {
        return None;
    };
    Some(base + if entry.search.is_none() { PAGE_BONUS } else { 0 })
}

/// Entries matching `query` (lowercase, trimmed), best first.
pub fn search(query: &str) -> Vec<(u32, &'static Entry)> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u32, &Entry)> = ENTRIES
        .iter()
        .filter_map(|e| Some((score(e, query)?, e)))
        .collect();
    hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    hits
}

/// The entry a result ID names.
pub fn by_id(id: &str) -> Option<&'static Entry> {
    id.strip_prefix(ID_PREFIX)?;
    ENTRIES.iter().find(|e| e.id() == id)
}

/// The arguments for `cnde-settings` that open an entry.
pub fn args(entry: &Entry) -> Vec<&'static str> {
    match entry.search {
        Some(text) => vec!["--search", text],
        None => vec![entry.page],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(query: &str) -> Vec<&'static str> {
        search(query).into_iter().map(|(_, e)| e.title).collect()
    }

    #[test]
    fn finds_options_by_title_and_keyword() {
        assert_eq!(titles("night light")[0], "Night light");
        assert_eq!(titles("dark")[0], "Dark style");
        assert!(titles("touchpad").contains(&"Touchpad"));
        assert_eq!(titles("wallpaper")[0], "Wallpaper");
        assert!(titles("resolution").contains(&"Resolution"));
        assert_eq!(titles("blue")[0], "Bluetooth");
        assert!(titles("trackpad").contains(&"Touchpad"));
        assert!(titles("zzz").is_empty());
    }

    #[test]
    fn pages_rank_above_options_on_a_tie() {
        let hits = search("dock");
        assert_eq!(hits[0].1.title, "Dock");
        assert!(hits[0].0 > hits[1].0);
    }

    #[test]
    fn scores_stay_between_strong_and_weak_apps() {
        for e in ENTRIES {
            for q in [e.title.to_lowercase().as_str(), e.keywords.first().copied().unwrap_or("")] {
                if let Some(s) = score(e, q) {
                    assert!((71..120).contains(&s), "{} {q} {s}", e.title);
                }
            }
        }
    }

    #[test]
    fn ids_round_trip_and_are_unique() {
        for e in ENTRIES {
            assert_eq!(by_id(&e.id()), Some(e));
        }
        let mut ids: Vec<String> = ENTRIES.iter().map(Entry::id).collect();
        ids.sort();
        ids.dedup();
        // Two options may share a search text; they must still differ in id.
        assert_eq!(ids.len(), ENTRIES.len());
    }

    #[test]
    fn commands_for_pages_and_options() {
        let page = ENTRIES.iter().find(|e| e.title == "Top Bar").unwrap();
        assert_eq!(args(page), ["top bar"]);
        let option = ENTRIES.iter().find(|e| e.title == "Night light").unwrap();
        assert_eq!(args(option), ["--search", "Night light"]);
    }
}
