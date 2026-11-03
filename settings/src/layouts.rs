//! The Keyboard page's layouts: the common ones by name, and the config's
//! `input.keyboard.layout` and `variant` lists (comma-separated, in step)
//! read and rewritten one at a time. Each is named by an id, "us" or
//! "us(dvorak)".

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

/// The layouts offered by name: xkb layout, variant, name.
pub const COMMON: &[(&str, &str, &str)] = &[
    ("us", "", "English (US)"),
    ("gb", "", "English (UK)"),
    ("us", "intl", "English (US, international)"),
    ("us", "dvorak", "English (Dvorak)"),
    ("us", "dvp", "English (programmer Dvorak)"),
    ("us", "colemak", "English (Colemak)"),
    ("us", "colemak_dh", "English (Colemak-DH)"),
    ("us", "workman", "English (Workman)"),
    ("ara", "", "Arabic"),
    ("cn", "", "Chinese"),
    ("cz", "", "Czech"),
    ("dk", "", "Danish"),
    ("nl", "", "Dutch"),
    ("fi", "", "Finnish"),
    ("fr", "", "French"),
    ("ca", "", "French (Canada)"),
    ("de", "", "German"),
    ("de", "nodeadkeys", "German (no dead keys)"),
    ("ch", "", "German (Switzerland)"),
    ("gr", "", "Greek"),
    ("il", "", "Hebrew"),
    ("hu", "", "Hungarian"),
    ("it", "", "Italian"),
    ("jp", "", "Japanese"),
    ("kr", "", "Korean"),
    ("no", "", "Norwegian"),
    ("pl", "", "Polish"),
    ("pt", "", "Portuguese"),
    ("br", "", "Portuguese (Brazil)"),
    ("ru", "", "Russian"),
    ("es", "", "Spanish"),
    ("latam", "", "Spanish (Latin American)"),
    ("se", "", "Swedish"),
    ("tr", "", "Turkish"),
    ("ua", "", "Ukrainian"),
];

/// The id of a layout and variant.
pub fn id(layout: &str, variant: &str) -> String {
    if variant.is_empty() {
        layout.to_string()
    } else {
        format!("{layout}({variant})")
    }
}

/// The configured layouts' ids, in order; with none set, the "us" xkb
/// falls back to.
pub fn configured(layout: &str, variant: &str) -> Vec<String> {
    if layout.trim().is_empty() {
        return vec!["us".into()];
    }
    let mut variants = variant.split(',').map(str::trim);
    layout
        .split(',')
        .map(|l| id(l.trim(), variants.next().unwrap_or("")))
        .collect()
}

/// fcitx5's profile for the layouts (ids like "us(dvorak)"), with Mozc as
/// the input method after them.
pub fn fcitx_profile(ids: &[String]) -> String {
    let mut layouts: Vec<String> = ids
        .iter()
        .filter(|id| !id.is_empty())
        .map(|id| match id.split_once('(') {
            Some((layout, variant)) => format!("{layout}-{}", variant.trim_end_matches(')')),
            None => id.clone(),
        })
        .collect();
    if layouts.is_empty() {
        layouts.push("us".into());
    }
    let mut text = format!(
        "[Groups/0]\nName=Default\nDefault Layout={}\nDefaultIM=mozc\n\n",
        layouts[0]
    );
    for (i, layout) in layouts.iter().enumerate() {
        text += &format!("[Groups/0/Items/{i}]\nName=keyboard-{layout}\nLayout=\n\n");
    }
    text += &format!(
        "[Groups/0/Items/{}]\nName=mozc\nLayout=\n\n[GroupOrder]\n0=Default\n",
        layouts.len()
    );
    text
}

/// fcitx5's main config: Super+Space switches input methods.
pub fn fcitx_config() -> &'static str {
    "[Hotkey/TriggerKeys]\n0=Super+space\n"
}

/// Write fcitx5's profile and config under `config_home` for the layouts,
/// each only if its file doesn't exist yet, so the user's settings stay.
pub fn write_fcitx_setup(config_home: &Path, layout: &str, variant: &str) -> io::Result<()> {
    let dir = config_home.join("fcitx5");
    fs::create_dir_all(&dir)?;
    let ids = configured(layout, variant);
    write_if_missing(&dir.join("profile"), &fcitx_profile(&ids))?;
    write_if_missing(&dir.join("config"), fcitx_config())
}

/// Create `path` with `text`, unless it already exists.
fn write_if_missing(path: &Path, text: &str) -> io::Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(text.as_bytes()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

/// A layout's name ("us(dvorak)" is "English (Dvorak)"), or its id.
pub fn name(id_: &str) -> String {
    COMMON
        .iter()
        .find(|(l, v, _)| id(l, v) == id_)
        .map_or_else(|| id_.to_string(), |(_, _, name)| name.to_string())
}

/// The config's layout and variant lists with layout `slot` set to `to`
/// (an id; empty removes it; a slot past the end adds it).
pub fn edited(layout: &str, variant: &str, slot: usize, to: &str) -> (String, String) {
    let mut ids = configured(layout, variant);
    match (slot < ids.len(), to.is_empty()) {
        (true, true) => {
            ids.remove(slot);
        }
        (true, false) => ids[slot] = to.to_string(),
        (false, false) if !ids.iter().any(|i| i == to) => ids.push(to.to_string()),
        _ => {}
    }
    let parts: Vec<(&str, &str)> = ids
        .iter()
        .map(|i| match i.split_once('(') {
            Some((l, v)) => (l, v.trim_end_matches(')')),
            None => (i.as_str(), ""),
        })
        .collect();
    let layouts: Vec<&str> = parts.iter().map(|(l, _)| *l).collect();
    let variants: Vec<&str> = parts.iter().map(|(_, v)| *v).collect();
    let variant = if variants.iter().all(|v| v.is_empty()) {
        String::new()
    } else {
        variants.join(",")
    };
    (layouts.join(","), variant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fcitx_profile_lists_layouts_then_mozc() {
        assert_eq!(
            fcitx_profile(&["us".into()]),
            "[Groups/0]\nName=Default\nDefault Layout=us\nDefaultIM=mozc\n\n\
             [Groups/0/Items/0]\nName=keyboard-us\nLayout=\n\n\
             [Groups/0/Items/1]\nName=mozc\nLayout=\n\n\
             [GroupOrder]\n0=Default\n"
        );
        let ids = ["us".into(), "us(dvorak)".into(), "de".into()];
        let text = fcitx_profile(&ids);
        assert!(text.contains("Default Layout=us\n"));
        assert!(text.contains("[Groups/0/Items/1]\nName=keyboard-us-dvorak\n"));
        assert!(text.contains("[Groups/0/Items/2]\nName=keyboard-de\n"));
        assert!(text.contains("[Groups/0/Items/3]\nName=mozc\n"));
        assert_eq!(fcitx_config(), "[Hotkey/TriggerKeys]\n0=Super+space\n");
    }

    #[test]
    fn fcitx_setup_is_written_once_and_keeps_the_users_files() {
        let root = std::env::temp_dir().join(format!("cnde-fcitx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write_fcitx_setup(&root, "us,de", ",").unwrap();
        let dir = root.join("fcitx5");
        let profile = fs::read_to_string(dir.join("profile")).unwrap();
        assert!(profile.contains("Name=keyboard-us\n"));
        assert!(profile.contains("Name=keyboard-de\n"));
        assert_eq!(fs::read_to_string(dir.join("config")).unwrap(), fcitx_config());

        // The user's own profile is left alone.
        fs::write(dir.join("profile"), "mine").unwrap();
        write_fcitx_setup(&root, "gb", "").unwrap();
        assert_eq!(fs::read_to_string(dir.join("profile")).unwrap(), "mine");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn layouts_are_added_changed_and_removed_in_step_with_variants() {
        assert_eq!(configured("", ""), ["us"]);
        assert_eq!(configured("us,de", ",nodeadkeys"), ["us", "de(nodeadkeys)"]);
        let added = edited("us", "", 1, "us(dvorak)");
        assert_eq!(added, ("us,us".into(), ",dvorak".into()));
        let changed = edited(&added.0, &added.1, 0, "gr");
        assert_eq!(changed, ("gr,us".into(), ",dvorak".into()));
        let removed = edited(&changed.0, &changed.1, 1, "");
        assert_eq!(removed, ("gr".into(), String::new()));
        // Already there: not added twice.
        assert_eq!(
            edited("us,de", "", 5, "de"),
            ("us,de".into(), String::new())
        );
        assert_eq!(name("us(colemak_dh)"), "English (Colemak-DH)");
        assert_eq!(name("fo"), "fo");
    }
}
