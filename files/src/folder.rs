//! The folder on show: its entries, order, selection and cursor. Pure
//! in-memory state; the listing itself arrives from a background thread.

use std::{collections::HashSet, path::PathBuf};

use crate::model::{self, Entry, SortKey};

/// Where the window is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Overview,
    Recent,
    Dir(PathBuf),
}

pub struct Folder {
    /// In sort order.
    pub entries: Vec<Entry>,
    /// Indices into `entries` that show (hidden files and the like left out).
    pub visible: Vec<usize>,
    /// Bumped whenever the order or contents change, so per-row caches know.
    pub version: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub sort: SortKey,
    pub descending: bool,
    pub show_hidden: bool,
    /// The search narrowing `visible`; empty shows everything.
    pub query: String,
    pub selected: HashSet<String>,
    /// The row keyboard moves act from, as a position in `visible`.
    pub cursor: Option<usize>,
    /// Where a Shift range starts.
    pub anchor: Option<usize>,
}

impl Folder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            visible: Vec::new(),
            version: 0,
            loading: true,
            error: None,
            sort: SortKey::Name,
            descending: false,
            show_hidden: false,
            query: String::new(),
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
        }
    }

    pub fn entry(&self, pos: usize) -> Option<&Entry> {
        self.entries.get(*self.visible.get(pos)?)
    }

    /// A batch of the listing arrived.
    pub fn add(&mut self, batch: Vec<Entry>) {
        self.entries.extend(batch);
        self.resort();
    }

    /// The listing was read again: swap it in, keeping selection by name.
    pub fn replace(&mut self, entries: Vec<Entry>) {
        self.entries = entries;
        self.resort();
    }

    pub fn set_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.descending = !self.descending;
        } else {
            self.sort = key;
            self.descending = false;
        }
        self.resort();
    }

    pub fn set_hidden(&mut self, show: bool) {
        self.show_hidden = show;
        self.rebuild();
    }

    /// Narrow the rows to the fuzzy matches of `query` (empty: all), with
    /// the best match selected.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.rebuild();
        if !self.query.is_empty() && !self.visible.is_empty() {
            self.select_only(0);
        }
    }

    fn resort(&mut self) {
        model::sort(&mut self.entries, self.sort, self.descending);
        self.rebuild();
    }

    /// Recompute `visible`, keeping the cursor on the same file.
    fn rebuild(&mut self) {
        let cursor = self
            .cursor
            .and_then(|c| self.entry(c))
            .map(|e| e.name.clone());
        let anchor = self
            .anchor
            .and_then(|c| self.entry(c))
            .map(|e| e.name.clone());
        let show = self.show_hidden;
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| show || !e.hidden)
            .map(|(i, _)| i)
            .collect();
        crate::search::filter(&self.entries, &mut self.visible, &self.query);
        let find = |name: Option<String>| {
            let name = name?;
            self.visible
                .iter()
                .position(|&i| self.entries[i].name == name)
        };
        let (cursor, anchor) = (find(cursor), find(anchor));
        self.cursor = cursor;
        self.anchor = anchor;
        let names: HashSet<&str> = self
            .visible
            .iter()
            .map(|&i| self.entries[i].name.as_str())
            .collect();
        self.selected.retain(|n| names.contains(n.as_str()));
        self.version += 1;
    }

    pub fn is_selected(&self, pos: usize) -> bool {
        self.entry(pos)
            .is_some_and(|e| self.selected.contains(&e.name))
    }

    /// Plain click: just this one.
    pub fn select_only(&mut self, pos: usize) {
        self.selected.clear();
        if let Some(e) = self.entry(pos) {
            self.selected.insert(e.name.clone());
        }
        self.cursor = Some(pos);
        self.anchor = Some(pos);
    }

    /// Ctrl+click.
    pub fn toggle(&mut self, pos: usize) {
        if let Some(name) = self.entry(pos).map(|e| e.name.clone())
            && !self.selected.remove(&name)
        {
            self.selected.insert(name);
        }
        self.cursor = Some(pos);
        self.anchor = Some(pos);
    }

    /// Shift+click: from the anchor to `pos`.
    pub fn select_range(&mut self, pos: usize) {
        let anchor = self.anchor.unwrap_or(pos);
        let (a, b) = (anchor.min(pos), anchor.max(pos));
        self.selected.clear();
        for p in a..=b {
            if let Some(e) = self.entry(p) {
                self.selected.insert(e.name.clone());
            }
        }
        self.cursor = Some(pos);
        self.anchor = Some(anchor);
    }

    pub fn select_all(&mut self) {
        self.selected = self
            .visible
            .iter()
            .map(|&i| self.entries[i].name.clone())
            .collect();
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Move the cursor by `delta` rows (clamped); returns the new position.
    pub fn step(&mut self, delta: isize, extend: bool) -> Option<usize> {
        if self.visible.is_empty() {
            return None;
        }
        let last = self.visible.len() - 1;
        let to = match self.cursor {
            None => {
                if delta < 0 {
                    last
                } else {
                    0
                }
            }
            Some(c) => (c as isize + delta).clamp(0, last as isize) as usize,
        };
        self.go_to(to, extend)
    }

    pub fn go_to(&mut self, pos: usize, extend: bool) -> Option<usize> {
        if pos >= self.visible.len() {
            return None;
        }
        if extend {
            self.select_range(pos);
        } else {
            self.select_only(pos);
        }
        Some(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from("/x").join(name),
            is_dir,
            is_symlink: false,
            hidden: name.starts_with('.'),
            size: 1,
            items: None,
            modified: None,
            kind: model::kind_of(name, is_dir),
        }
    }

    #[test]
    fn hidden_and_selection() {
        let mut f = Folder::new();
        f.add(vec![
            entry("b", false),
            entry(".h", false),
            entry("a", true),
        ]);
        assert_eq!(f.visible.len(), 2);
        assert_eq!(f.entry(0).unwrap().name, "a");
        f.select_only(1);
        f.select_range(0);
        assert_eq!(f.selected.len(), 2);
        f.set_hidden(true);
        assert_eq!(f.visible.len(), 3);
        f.set_hidden(false);
        assert_eq!(f.visible.len(), 2);
        assert_eq!(f.step(5, false), Some(1));
    }
}
