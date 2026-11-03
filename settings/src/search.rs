//! Searching every page's rows at once.

use crate::pages::{self, Control, Group, Key, Page, Row};
use crate::view::Ui;

/// Whether every word of `query` is somewhere in `hay` (any order, any
/// case).
pub fn matches(query: &str, hay: &[&str]) -> bool {
    let hay = hay.join(" ").to_lowercase();
    let mut words = query.split_whitespace().peekable();
    words.peek().is_some() && words.all(|w| hay.contains(&w.to_lowercase()))
}

/// Rows worth listing out of their page: plain config rows, with controls
/// that work without their page around them.
fn searchable(row: &Row) -> bool {
    matches!(row.key, Key::Path(_) | Key::Shortcut(_))
        && !matches!(
            row.control,
            Control::Wallpapers | Control::Arrangement | Control::Mixer | Control::Info
        )
}

/// The rows matching `query`, a group per page section. Advanced rows count
/// only while the Advanced toggle is on.
pub fn results(ui: &Ui, query: &str) -> Vec<Group> {
    let mut out = Vec::new();
    for page in Page::visible(&ui.config) {
        for group in pages::build_for(ui, page) {
            let rows: Vec<Row> = group
                .rows
                .into_iter()
                .filter(|r| searchable(r) && (ui.advanced || !r.advanced))
                .filter(|r| matches(query, &[page.title(), &group.title, &r.title, &r.subtitle]))
                .collect();
            if rows.is_empty() {
                continue;
            }
            let title = if group.title.is_empty() || group.title == page.title() {
                page.title().to_string()
            } else {
                format!("{} \u{b7} {}", page.title(), group.title)
            };
            out.push(Group {
                title,
                description: String::new(),
                rows,
            });
        }
    }
    if out.is_empty() {
        out.push(Group {
            title: String::new(),
            description: format!("No settings match \u{201c}{}\u{201d}", query.trim()),
            rows: Vec::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_match_in_any_order() {
        let hay = ["Night Light", "Colour temperature", "Warmth at night"];
        assert!(matches("night temp", &hay));
        assert!(matches("TEMP NIGHT", &hay));
        assert!(matches("  colour ", &hay));
        assert!(!matches("night speed", &hay));
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(!matches("", &["anything"]));
        assert!(!matches("   ", &["anything"]));
    }
}
