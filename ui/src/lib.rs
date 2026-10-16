//! cnde's small UI toolkit (ARCHITECTURE.md §5.1), shared by the shell, the
//! lock screen and the settings app so they look like one product.
//!
//! [`widgets`] has the controls app windows use: switches, sliders,
//! dropdowns, text entries.
//!
//! Deliberately small: CPU drawing with tiny-skia straight into the caller's
//! pixel buffer (premultiplied RGBA, i.e. wl_shm ABGR8888), text with
//! cosmic-text, and every visual value a theme token. Motion is the
//! compositor's job, so clients here mostly draw still frames.

pub mod anim;
pub mod icons;
pub mod paint;
pub mod text;
pub mod theme;
pub mod widgets;

pub use anim::Animator;
pub use icons::Icons;
pub use paint::Canvas;
pub use text::{Fonts, TextLayout, Weight};
pub use theme::{Color, Theme};
pub use widgets::{Painter, Rect};
// For callers that keep their own images (the settings app's thumbnails).
pub use tiny_skia;
