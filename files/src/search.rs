//! Search: the sidebar field's text state, key handling, and the fuzzy
//! filter that narrows the current folder while a query is set.

use std::cmp::Reverse;

use cnde_ui::{Color, Painter, Rect, Weight};
use smithay_client_toolkit::seat::keyboard::Keysym;

use crate::model::{self, Entry};

/// The search field: its text, caret and whether it has the keyboard.
#[derive(Default)]
pub struct Search {
    pub query: String,
    /// In chars.
    pub cursor: usize,
    pub focused: bool,
}

/// What a key did to the field.
#[derive(Debug, PartialEq, Eq)]
pub enum Keyed {
    /// Not for the search; the window handles it.
    Ignored,
    /// Handled, the query is the same.
    Handled,
    /// The query changed.
    Changed,
    /// Open the selected match.
    Open,
}

impl Search {
    pub fn active(&self) -> bool {
        !self.query.is_empty()
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.focused = false;
    }

    fn byte(&self, chars: usize) -> usize {
        self.query
            .char_indices()
            .nth(chars)
            .map_or(self.query.len(), |(i, _)| i)
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte(self.cursor);
        self.query.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// Handle a key press. `text` is what it types, if anything.
    /// `type_to_filter` lets a printable character start the search while
    /// the field has no focus.
    pub fn key(
        &mut self,
        keysym: Keysym,
        text: Option<&str>,
        ctrl: bool,
        alt: bool,
        type_to_filter: bool,
    ) -> Keyed {
        if ctrl && matches!(keysym, Keysym::f | Keysym::F) {
            self.focused = true;
            self.cursor = self.query.chars().count();
            return Keyed::Handled;
        }
        if keysym == Keysym::Escape {
            if self.focused || self.active() {
                let had = self.active();
                self.clear();
                return if had { Keyed::Changed } else { Keyed::Handled };
            }
            return Keyed::Ignored;
        }
        let printable =
            text.filter(|t| !ctrl && !alt && !t.is_empty() && !t.chars().any(char::is_control));
        if !self.focused {
            // Typing a name starts the search, as in other file managers.
            let starts = printable.filter(|t| type_to_filter && !t.starts_with(' '));
            if let Some(t) = starts {
                self.focused = true;
                self.cursor = self.query.chars().count();
                self.insert(t);
                return Keyed::Changed;
            }
            return Keyed::Ignored;
        }
        if alt {
            return Keyed::Ignored;
        }
        let len = self.query.chars().count();
        match keysym {
            Keysym::Return | Keysym::KP_Enter => return Keyed::Open,
            Keysym::BackSpace if self.cursor > 0 => {
                let at = self.byte(self.cursor - 1);
                self.query.remove(at);
                self.cursor -= 1;
            }
            Keysym::Delete if self.cursor < len => {
                let at = self.byte(self.cursor);
                self.query.remove(at);
            }
            Keysym::BackSpace | Keysym::Delete => return Keyed::Handled,
            Keysym::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                return Keyed::Handled;
            }
            Keysym::Right => {
                self.cursor = (self.cursor + 1).min(len);
                return Keyed::Handled;
            }
            Keysym::Home => {
                self.cursor = 0;
                return Keyed::Handled;
            }
            Keysym::End => {
                self.cursor = len;
                return Keyed::Handled;
            }
            _ => match printable {
                Some(t) => self.insert(t),
                // Arrows, Page keys and the rest move through the results.
                None => return Keyed::Ignored,
            },
        }
        Keyed::Changed
    }
}

/// Narrow `visible` (indices into `entries`) to the fuzzy matches of `query`,
/// best first, ties by name. An empty query leaves it alone.
pub fn filter(entries: &[Entry], visible: &mut Vec<usize>, query: &str) {
    if query.is_empty() {
        return;
    }
    let mut scored: Vec<(i32, usize)> = visible
        .iter()
        .filter_map(|&i| Some((model::fuzzy_score(query, &entries[i].name)?, i)))
        .collect();
    scored.sort_by_cached_key(|&(score, i)| (Reverse(score), entries[i].name.to_lowercase()));
    *visible = scored.into_iter().map(|(_, i)| i).collect();
}

/// Char indices of `name` a query's letters land on (in order, first
/// occurrences, case-insensitive), for highlighting.
pub fn match_positions(query: &str, name: &str) -> Vec<usize> {
    let mut q = query.chars().flat_map(char::to_lowercase).peekable();
    let mut out = Vec::new();
    for (i, c) in name.chars().enumerate() {
        let Some(&want) = q.peek() else {
            break;
        };
        if c.to_lowercase().eq(std::iter::once(want)) {
            out.push(i);
            q.next();
        }
    }
    out
}

/// A name with the matched letters in `hot` (bold) and the rest in `ink`.
#[allow(clippy::too_many_arguments)]
pub fn paint_name(
    p: &mut Painter<'_, '_>,
    query: &str,
    name: &str,
    x: f32,
    cy: f32,
    max_width: f32,
    size: f32,
    ink: Color,
    hot: Color,
) {
    let positions = match_positions(query, name);
    let chars: Vec<char> = name.chars().collect();
    let is_hot = |i: usize| positions.binary_search(&i).is_ok();
    let end = x + max_width;
    let mut x = x;
    let mut i = 0;
    while i < chars.len() && x < end {
        let on = is_hot(i);
        let mut j = i;
        while j < chars.len() && is_hot(j) == on {
            j += 1;
        }
        let run: String = chars[i..j].iter().collect();
        let (weight, color) = if on {
            (Weight::Bold, hot)
        } else {
            (Weight::Normal, ink)
        };
        x += p.label_v(&run, x, cy, end - x, size, weight, color);
        i = j;
    }
}

/// The field in the sidebar: a quiet pill until it has focus or text.
pub fn paint_field(p: &mut Painter<'_, '_>, search: &Search, rect: Rect, pill: Color, dim: Color) {
    if search.focused || search.active() {
        let cursor = search.focused.then_some(search.cursor);
        p.entry(rect, &search.query, "Search", cursor, false);
        return;
    }
    let size = p.theme.font_size;
    p.canvas
        .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, pill);
    p.icon(
        "edit-find-symbolic",
        rect.x + 18.0,
        rect.y + 16.0,
        16.0,
        dim,
    );
    p.label_v(
        "Search",
        rect.x + 36.0,
        rect.y + 16.0,
        rect.w - 44.0,
        size,
        Weight::Normal,
        dim,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from("/x").join(name),
            is_dir: false,
            is_symlink: false,
            hidden: false,
            size: 0,
            items: None,
            modified: None,
            kind: String::new(),
        }
    }

    fn names(entries: &[Entry], visible: &[usize]) -> Vec<String> {
        visible.iter().map(|&i| entries[i].name.clone()).collect()
    }

    #[test]
    fn filters_and_orders() {
        let entries: Vec<Entry> = ["domain.rs", "main.rs", "readme.md", "Main.toml", "a_main.c"]
            .iter()
            .map(|n| entry(n))
            .collect();
        let mut visible: Vec<usize> = (0..entries.len()).collect();
        filter(&entries, &mut visible, "main");
        let got = names(&entries, &visible);
        assert!(!got.contains(&"readme.md".to_string()));
        assert_eq!(got.len(), 4);
        assert!(got[0].to_lowercase().starts_with("main"));
        assert_eq!(got.last().unwrap(), "domain.rs");
        let mut all: Vec<usize> = (0..entries.len()).collect();
        filter(&entries, &mut all, "");
        assert_eq!(all.len(), entries.len());
    }

    #[test]
    fn ties_fall_by_name() {
        let entries = vec![entry("b1"), entry("a1"), entry("c1")];
        let mut visible = vec![0, 1, 2];
        filter(&entries, &mut visible, "1");
        assert_eq!(names(&entries, &visible), ["a1", "b1", "c1"]);
    }

    #[test]
    fn positions() {
        assert_eq!(match_positions("mr", "main.rs"), vec![0, 5]);
        assert_eq!(match_positions("MA", "main"), vec![0, 1]);
    }

    #[test]
    fn typing_and_editing() {
        let mut s = Search::default();
        let key = |s: &mut Search, k, t| s.key(k, t, false, false, true);
        assert_eq!(
            s.key(Keysym::a, Some("a"), false, false, false),
            Keyed::Ignored
        );
        assert_eq!(key(&mut s, Keysym::a, Some("a")), Keyed::Changed);
        assert!(s.focused);
        key(&mut s, Keysym::b, Some("b"));
        assert_eq!(s.query, "ab");
        key(&mut s, Keysym::Left, None);
        key(&mut s, Keysym::x, Some("x"));
        assert_eq!(s.query, "axb");
        assert_eq!(key(&mut s, Keysym::BackSpace, None), Keyed::Changed);
        assert_eq!(s.query, "ab");
        assert_eq!(key(&mut s, Keysym::Down, None), Keyed::Ignored);
        assert_eq!(key(&mut s, Keysym::Return, None), Keyed::Open);
        assert_eq!(key(&mut s, Keysym::Escape, None), Keyed::Changed);
        assert!(!s.active() && !s.focused);
        assert_eq!(
            s.key(Keysym::f, Some("f"), true, false, true),
            Keyed::Handled
        );
        assert!(s.focused);
    }
}
