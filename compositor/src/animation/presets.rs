//! Every animation duration and curve in the compositor, in one place, so the
//! "feel" is tuned here and nowhere else (ARCHITECTURE.md §4.3).

use std::time::Duration;

use super::{Curve, Params};

pub const WINDOW_OPEN: Params = Params {
    duration: Duration::from_millis(250),
    curve: Curve::EaseOutExpo,
};
/// Windows grow from this scale to 1.0 while fading in.
pub const WINDOW_OPEN_START_SCALE: f64 = 0.9;

/// Maximize, snap, fullscreen and restore.
pub const WINDOW_RESIZE: Params = Params {
    duration: Duration::from_millis(250),
    curve: Curve::EaseOutExpo,
};
/// How long a resize waits for the client to redraw at its new size before
/// animating anyway (with the old buffer stretched).
pub const RESIZE_COMMIT_TIMEOUT: Duration = Duration::from_millis(150);
/// How long a window dragged off fullscreen/maximized is shown squeezed to
/// its new size while the client catches up. Longer than the animation
/// timeout: nothing waits on it, the window already follows the pointer.
pub const PENDING_SIZE_TIMEOUT: Duration = Duration::from_millis(500);

pub const WINDOW_CLOSE: Params = Params {
    duration: Duration::from_millis(180),
    curve: Curve::EaseOutCubic,
};
/// Closing windows shrink to this scale while fading out.
pub const WINDOW_CLOSE_END_SCALE: f64 = 0.9;

/// Sliding the workspace strip by one (keys, or settling after a swipe).
pub const WORKSPACE_SWITCH: Params = Params {
    duration: Duration::from_millis(300),
    curve: Curve::EaseOutCubic,
};
/// How far ahead release velocity is projected when picking where a swipe
/// settles: a quick flick goes to the next workspace even if short.
pub const SWIPE_FLICK_SECONDS: f64 = 0.15;

/// Opening and closing the overview.
pub const OVERVIEW: Params = Params {
    duration: Duration::from_millis(300),
    curve: Curve::EaseOutCubic,
};

/// Drag-to-edge snap preview growing out of the dragged window.
pub const SNAP_PREVIEW: Params = Params {
    duration: Duration::from_millis(150),
    curve: Curve::EaseOutCubic,
};
