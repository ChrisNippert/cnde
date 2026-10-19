//! The icon size of each view: its limits, the row height that follows, and
//! the step of a key press or a wheel notch.

use crate::view::ViewMode;

/// (smallest, largest, default) icon size of a view, in logical px.
pub fn limits(mode: ViewMode) -> (f32, f32, f32) {
    match mode {
        ViewMode::Grid => (48.0, 192.0, 48.0),
        ViewMode::List => (16.0, 48.0, 18.0),
    }
}

pub fn clamp(mode: ViewMode, size: f32) -> f32 {
    let (min, max, _) = limits(mode);
    size.clamp(min, max)
}

/// How much one key press changes the size.
pub fn step(mode: ViewMode) -> f32 {
    match mode {
        ViewMode::Grid => 16.0,
        ViewMode::List => 4.0,
    }
}

/// Where `size` lies between the limits, 0-1 (for the slider).
pub fn fraction(mode: ViewMode, size: f32) -> f32 {
    let (min, max, _) = limits(mode);
    ((size - min) / (max - min)).clamp(0.0, 1.0)
}

/// The size at `fraction` (0-1) of the way along the slider.
pub fn from_fraction(mode: ViewMode, fraction: f32) -> f32 {
    let (min, max, _) = limits(mode);
    clamp(mode, (min + (max - min) * fraction).round())
}

/// A list row's height: the icon with some air, never under 30.
pub fn row_h(icon: f32) -> f32 {
    (icon + 12.0).max(30.0)
}

/// How far the name sits from the row's left edge, past the icon.
pub fn name_offset(icon: f32) -> f32 {
    (icon + 10.0).max(28.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_old_sizes() {
        assert_eq!(row_h(limits(ViewMode::List).2), 30.0);
        assert_eq!(name_offset(limits(ViewMode::List).2), 28.0);
    }

    #[test]
    fn rows_grow_with_the_icon() {
        assert_eq!(row_h(16.0), 30.0);
        assert_eq!(row_h(48.0), 60.0);
        assert!(name_offset(48.0) > name_offset(18.0));
    }

    #[test]
    fn slider_round_trips() {
        for mode in [ViewMode::Grid, ViewMode::List] {
            let (min, max, _) = limits(mode);
            assert_eq!(from_fraction(mode, 0.0), min);
            assert_eq!(from_fraction(mode, 1.0), max);
            let mid = from_fraction(mode, 0.5);
            assert!((fraction(mode, mid) - 0.5).abs() < 0.02);
            assert_eq!(clamp(mode, 10_000.0), max);
        }
    }
}
