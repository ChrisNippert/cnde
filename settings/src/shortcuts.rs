//! Editing the keyboard shortcuts: combos compared and written the way the
//! config reads them.

use cnde_config::{SHORTCUT_DEFAULTS, Shortcuts};

/// A combo reduced to what the compositor compares: the modifiers (bits:
/// Super, Alt, Ctrl, Shift) and a lower-case key name.
fn normal(combo: &str) -> Option<(u8, String)> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).collect();
    let (key, mods) = parts.split_last()?;
    let mut bits = 0;
    for m in mods {
        bits |= match m.to_ascii_lowercase().as_str() {
            "super" | "mod" | "mod4" | "logo" | "win" => 1,
            "alt" | "mod1" => 2,
            "ctrl" | "control" | "ctl" => 4,
            "shift" => 8,
            _ => return None,
        };
    }
    let key = match key.to_ascii_lowercase().as_str() {
        "" => return None,
        "enter" => "return".to_string(),
        "esc" => "escape".to_string(),
        other => other.to_string(),
    };
    Some((bits, key))
}

/// Whether `a` and `b` are the same combo.
pub fn same(a: &str, b: &str) -> bool {
    normal(a).is_some_and(|a| normal(b) == Some(a))
}

/// What's wrong with binding `combo`, if anything: a bare letter would eat
/// typing, and a few combos are the compositor's own.
pub fn refuse(combo: &str) -> Option<&'static str> {
    let (mods, key) = normal(combo)?;
    let function = key
        .strip_prefix('f')
        .is_some_and(|n| n.parse::<u8>().is_ok());
    if mods & 4 != 0 && mods & 2 != 0 && (key == "backspace" || function) {
        return Some("That combo is reserved by the compositor");
    }
    let special = function || matches!(key.as_str(), "print" | "pause") || key.starts_with("xf86");
    if mods & 7 == 0 && !special {
        return Some("Hold Super, Ctrl or Alt as well");
    }
    None
}

/// The label of another action already using `combo` (`name` itself may
/// keep it).
pub fn conflict(shortcuts: &Shortcuts, name: &str, combo: &str) -> Option<String> {
    let uses = |keys: &[String]| keys.iter().any(|k| same(k, combo));
    for (other, _, label) in SHORTCUT_DEFAULTS {
        if *other != name && shortcuts.bindings.get(*other).is_some_and(|k| uses(k)) {
            return Some(label.to_string());
        }
    }
    shortcuts
        .custom
        .iter()
        .find(|c| uses(&c.keys))
        .map(|c| c.command.clone())
}

/// `keys` with `combo` as the first: an action's other combos stay.
pub fn with_first(keys: &[String], combo: &str) -> Vec<String> {
    let mut out = vec![combo.to_string()];
    out.extend(keys.iter().skip(1).filter(|k| !same(k, combo)).cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_compare_like_the_compositor() {
        assert!(same("Super+Shift+S", "mod+shift+s"));
        assert!(same("Alt+Shift+Tab", "Shift+Alt+Tab"));
        assert!(same("Super+Enter", "Super+Return"));
        assert!(!same("Super+S", "Super+Shift+S"));
        assert!(!same("Super+S", "Ctrl+S"));
    }

    #[test]
    fn conflicts_name_the_other_action() {
        let s = Shortcuts::default();
        assert_eq!(
            conflict(&s, "browser", "Super+Q").as_deref(),
            Some("Open a terminal")
        );
        // Its own combo is fine, and so is a free one.
        assert_eq!(conflict(&s, "terminal", "Super+Q"), None);
        assert_eq!(conflict(&s, "terminal", "Super+Z"), None);
        // The second combo of "close" counts too.
        assert!(conflict(&s, "terminal", "alt+f4").is_some());
    }

    #[test]
    fn some_combos_are_refused() {
        assert!(refuse("a").is_some());
        assert!(refuse("Shift+a").is_some());
        assert!(refuse("Ctrl+Alt+BackSpace").is_some());
        assert!(refuse("Ctrl+Alt+F2").is_some());
        assert_eq!(refuse("Super+a"), None);
        assert_eq!(refuse("Print"), None);
        assert_eq!(refuse("F5"), None);
    }

    #[test]
    fn the_new_combo_replaces_the_first() {
        let keys = vec!["Super+C".to_string(), "Alt+F4".to_string()];
        assert_eq!(with_first(&keys, "Super+X"), ["Super+X", "Alt+F4"]);
        assert_eq!(with_first(&keys, "alt+f4"), ["alt+f4"]);
        assert_eq!(with_first(&[], "Super+X"), ["Super+X"]);
    }
}
