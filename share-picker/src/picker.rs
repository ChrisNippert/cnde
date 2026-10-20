//! The picker's choice, apart from any drawing: which card is selected,
//! what a key or click does, and what we tell xdg-desktop-portal-wlr.
//!
//! The portal's "simple" chooser protocol: print `Monitor: <output>`
//! on stdout and exit 0, or exit non-zero to cancel.

use std::io::{self, Write};

use crate::layout::Hit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Choose(String),
    Cancel,
}

pub struct Picker {
    names: Vec<String>,
    pub selected: usize,
    columns: usize,
}

impl Picker {
    pub fn new(names: Vec<String>) -> Self {
        Self {
            names,
            selected: 0,
            columns: 1,
        }
    }

    pub fn set_columns(&mut self, columns: usize) {
        self.columns = columns.max(1);
    }

    pub fn navigate(&mut self, nav: Nav) {
        let last = self.names.len().saturating_sub(1);
        let next = match nav {
            Nav::Left => self.selected.saturating_sub(1),
            Nav::Right => self.selected + 1,
            Nav::Up => self.selected.saturating_sub(self.columns),
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
            Hit::Card(i) if i != self.selected && i < self.names.len() => {
                self.selected = i;
                true
            }
            _ => false,
        }
    }

    pub fn confirm(&self) -> Option<Outcome> {
        self.names.get(self.selected).cloned().map(Outcome::Choose)
    }

    pub fn click(&mut self, hit: Hit) -> Option<Outcome> {
        match hit {
            Hit::Card(i) => {
                self.selected = i;
                self.confirm()
            }
            Hit::Cancel | Hit::Outside => Some(Outcome::Cancel),
            Hit::Panel => None,
        }
    }
}

/// Writes the answer portal-wlr reads; returns the exit code to leave with.
pub fn report(out: &mut impl Write, outcome: &Outcome) -> io::Result<i32> {
    match outcome {
        Outcome::Choose(name) => {
            // portal-wlr reads "Monitor: <output>"; anything else counts as
            // the user declining.
            writeln!(out, "Monitor: {name}")?;
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
        let mut p = Picker::new((1..=n).map(|i| format!("DP-{i}")).collect());
        p.set_columns(columns);
        p
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
        assert_eq!(p.confirm(), Some(Outcome::Choose("DP-2".into())));
    }

    #[test]
    fn clicks() {
        let mut p = picker(2, 2);
        assert_eq!(p.click(Hit::Card(1)), Some(Outcome::Choose("DP-2".into())));
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
        let code = report(&mut out, &Outcome::Choose("HDMI-A-1".into())).unwrap();
        assert_eq!((code, out.as_slice()), (0, b"Monitor: HDMI-A-1\n".as_slice()));
        let mut out = Vec::new();
        assert_eq!(report(&mut out, &Outcome::Cancel).unwrap(), 1);
        assert!(out.is_empty());
    }
}
