//! The Keyboard page's layouts: the common ones by name, and the config's
//! `input.keyboard.layout` and `variant` lists (comma-separated, in step)
//! read and rewritten one at a time. Each is named by an id, "us" or
//! "us(dvorak)".

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
