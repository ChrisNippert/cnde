//! The picker's choice, apart from any drawing: which tab and card is
//! selected, what a key or click does, and what we tell
//! xdg-desktop-portal-wlr.
//!
//! The portal's "simple" chooser protocol: print `Monitor: <output>` or
//! `Window: <toplevel identifier>` on stdout and exit 0, or exit non-zero
//! to cancel.

use std::io::{self, Write};

use crate::layout::Hit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Screens,
    Windows,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Screens, Tab::Windows];

    pub fn index(self) -> usize {
        self as usize
    }
}

/// What gets shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An output name.
    Monitor(String),
    /// An ext-foreign-toplevel-list-v1 identifier.
    Window(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Choose(Target),
    Cancel,
}

pub struct Picker {
    screens: Vec<Target>,
    windows: Vec<Target>,
    pub tab: Tab,
    /// Keyboard focus is on the tab control rather than the cards.
    pub tabs_focused: bool,
    pub selected: usize,
    columns: usize,
}

impl Picker {
    pub fn new(screens: Vec<String>, windows: Vec<String>) -> Self {
        Self {
            screens: screens.into_iter().map(Target::Monitor).collect(),
            windows: windows.into_iter().map(Target::Window).collect(),
            tab: Tab::Screens,
            tabs_focused: false,
            selected: 0,
            columns: 1,
        }
    }

    fn items(&self) -> &[Target] {
        match self.tab {
            Tab::Screens => &self.screens,
            Tab::Windows => &self.windows,
        }
    }

    /// Cards on the current tab.
    pub fn count(&self) -> usize {
        self.items().len()
    }

    pub fn set_columns(&mut self, columns: usize) {
        self.columns = columns.max(1);
    }

    /// Returns whether the tab changed.
    pub fn set_tab(&mut self, tab: Tab) -> bool {
        if tab == self.tab {
            return false;
        }
        self.tab = tab;
        self.selected = 0;
        true
    }

    /// Tab / Shift+Tab: the other tab (there are two).
    pub fn cycle_tab(&mut self) {
        self.set_tab(Tab::ALL[1 - self.tab.index()]);
    }

    pub fn navigate(&mut self, nav: Nav) {
        if self.tabs_focused {
            match nav {
                Nav::Left => {
                    self.set_tab(Tab::Screens);
                }
                Nav::Right => {
                    self.set_tab(Tab::Windows);
                }
                Nav::Down => self.tabs_focused = false,
                Nav::Up => {}
            }
            return;
        }
        let last = self.count().saturating_sub(1);
        let next = match nav {
            Nav::Left => self.selected.saturating_sub(1),
            Nav::Right => self.selected + 1,
            Nav::Up if self.selected < self.columns => {
                self.tabs_focused = true;
                return;
            }
            Nav::Up => self.selected - self.columns,
            Nav::Down => self.selected + self.columns,
        };
        // Down from the last row stays put rather than jumping sideways.
        if next <= last {
            self.selected = next;
        }
    }

    /// The card under the pointer becomes the selection. Returns whether
    /// anything changed (so the caller redraws).
    pub fn hover(&mut self, hit: Hit) -> bool {
        match hit {
            Hit::Card(i) if (i != self.selected || self.tabs_focused) && i < self.count() => {
                self.selected = i;
                self.tabs_focused = false;
                true
            }
            _ => false,
        }
    }

    /// Enter on a card chooses it; on the tab control it goes to the cards.
    pub fn confirm(&mut self) -> Option<Outcome> {
        if self.tabs_focused {
            self.tabs_focused = false;
            return None;
        }
        self.items()
            .get(self.selected)
            .cloned()
            .map(Outcome::Choose)
    }

    pub fn click(&mut self, hit: Hit) -> Option<Outcome> {
        match hit {
            Hit::Card(i) => {
                self.selected = i;
                self.tabs_focused = false;
                self.confirm()
            }
            Hit::Tab(i) => {
                if let Some(&tab) = Tab::ALL.get(i) {
                    self.set_tab(tab);
                }
                None
            }
            Hit::Cancel | Hit::Outside => Some(Outcome::Cancel),
            Hit::Panel => None,
        }
    }
}

/// Writes the answer portal-wlr reads; returns the exit code to leave with.
pub fn report(out: &mut impl Write, outcome: &Outcome) -> io::Result<i32> {
    match outcome {
        Outcome::Choose(target) => {
            // portal-wlr reads "Monitor: <output>" or "Window: <identifier>";
            // anything else counts as the user declining.
            match target {
                Target::Monitor(name) => writeln!(out, "Monitor: {name}")?,
                Target::Window(id) => writeln!(out, "Window: {id}")?,
            }
            out.flush()?;
            Ok(0)
        }
        Outcome::Cancel => Ok(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(n: usize, columns: usize) -> Picker {
        let mut p = Picker::new(
            (1..=n).map(|i| format!("DP-{i}")).collect(),
            vec!["id-a".into(), "id-b".into(), "id-c".into()],
        );
        p.set_columns(columns);
        p
    }

    fn monitor(name: &str) -> Outcome {
        Outcome::Choose(Target::Monitor(name.into()))
    }

    #[test]
    fn arrows_move_within_the_grid() {
        let mut p = picker(4, 3);
        p.navigate(Nav::Right);
        p.navigate(Nav::Right);
        assert_eq!(p.selected, 2);
        p.navigate(Nav::Right);
        assert_eq!(p.selected, 3);
        p.navigate(Nav::Right);
        assert_eq!(p.selected, 3);
        p.navigate(Nav::Up);
        assert_eq!(p.selected, 0);
        p.navigate(Nav::Down);
        assert_eq!(p.selected, 3);
        p.navigate(Nav::Down);
        assert_eq!(p.selected, 3);
        p.navigate(Nav::Left);
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn enter_chooses_the_selected_name() {
        let mut p = picker(2, 2);
        p.navigate(Nav::Right);
        assert_eq!(p.confirm(), Some(monitor("DP-2")));
    }

    #[test]
    fn clicks() {
        let mut p = picker(2, 2);
        assert_eq!(p.click(Hit::Card(1)), Some(monitor("DP-2")));
        assert_eq!(p.click(Hit::Outside), Some(Outcome::Cancel));
        assert_eq!(p.click(Hit::Cancel), Some(Outcome::Cancel));
        assert_eq!(p.click(Hit::Panel), None);
    }

    #[test]
    fn hover_selects() {
        let mut p = picker(3, 3);
        assert!(p.hover(Hit::Card(2)));
        assert!(!p.hover(Hit::Card(2)));
        assert!(!p.hover(Hit::Panel));
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn protocol_prints_name_or_nothing() {
        let mut out = Vec::new();
        let code = report(&mut out, &monitor("HDMI-A-1")).unwrap();
        assert_eq!(
            (code, out.as_slice()),
            (0, b"Monitor: HDMI-A-1\n".as_slice())
        );
        let mut out = Vec::new();
        let outcome = Outcome::Choose(Target::Window("abc123".into()));
        let code = report(&mut out, &outcome).unwrap();
        assert_eq!((code, out.as_slice()), (0, b"Window: abc123\n".as_slice()));
        let mut out = Vec::new();
        assert_eq!(report(&mut out, &Outcome::Cancel).unwrap(), 1);
        assert!(out.is_empty());
    }

    #[test]
    fn tab_switches_lists_and_resets_selection() {
        let mut p = picker(2, 3);
        p.navigate(Nav::Right);
        assert_eq!(p.selected, 1);
        p.cycle_tab();
        assert_eq!((p.tab, p.selected, p.count()), (Tab::Windows, 0, 3));
        p.navigate(Nav::Right);
        assert_eq!(
            p.confirm(),
            Some(Outcome::Choose(Target::Window("id-b".into())))
        );
        p.cycle_tab();
        assert_eq!(p.tab, Tab::Screens);
    }

    #[test]
    fn tab_control_takes_arrows_when_focused() {
        let mut p = picker(2, 2);
        // Up from the first row reaches the control; Left/Right switch.
        p.navigate(Nav::Up);
        assert!(p.tabs_focused);
        p.navigate(Nav::Right);
        assert_eq!(p.tab, Tab::Windows);
        p.navigate(Nav::Right);
        assert_eq!(p.tab, Tab::Windows);
        p.navigate(Nav::Left);
        assert_eq!(p.tab, Tab::Screens);
        // Enter hands focus back instead of choosing.
        assert_eq!(p.confirm(), None);
        assert!(!p.tabs_focused);
        p.navigate(Nav::Up);
        p.navigate(Nav::Down);
        assert!(!p.tabs_focused);
        assert_eq!(p.confirm(), Some(monitor("DP-1")));
    }

    #[test]
    fn tab_clicks_and_empty_tabs() {
        let mut p = Picker::new(vec!["DP-1".into()], Vec::new());
        assert_eq!(p.click(Hit::Tab(1)), None);
        assert_eq!(p.tab, Tab::Windows);
        assert_eq!(p.confirm(), None);
        assert!(!p.hover(Hit::Card(0)));
        assert_eq!(p.click(Hit::Tab(0)), None);
        assert_eq!(p.confirm(), Some(monitor("DP-1")));
    }
}
