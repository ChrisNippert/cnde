//! Every animation duration and curve in the compositor, in one place, so the
//! "feel" is tuned here and nowhere else (ARCHITECTURE.md §4.3).

use std::time::Duration;

use super::{Curve, Params};

/// Global speed multiplier; 0.0 disables motion ("reduce motion").
/// Becomes a config option once the config crate exists.
pub const SPEED_MULTIPLIER: f64 = 1.0;

pub const WINDOW_OPEN: Params = Params {
    duration: Duration::from_millis(250),
    curve: Curve::EaseOutExpo,
};
/// Windows grow from this scale to 1.0 while fading in.
pub const WINDOW_OPEN_START_SCALE: f64 = 0.9;
