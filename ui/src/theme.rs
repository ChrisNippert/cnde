//! Theme tokens. Every colour, radius and size the UI uses comes from here,
//! so theming (and later the settings app) is overriding values, never
//! patching drawing code. Defaults follow libadwaita's dark palette.

/// Straight (non-premultiplied) sRGB colour, 0.0–1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 1.0)
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a,
        }
    }

    pub const fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    // Panel
    pub panel_background: Color,
    pub panel_foreground: Color,
    /// Logical pixels.
    pub panel_height: u32,

    // Workspace indicator (GNOME-style dots; the active one is a pill)
    pub workspace_dot: Color,
    pub workspace_active: Color,
    pub workspace_dot_size: f32,
    pub workspace_active_width: f32,
    pub workspace_spacing: f32,

    // Status icons (right side of the panel)
    pub status_icon_size: f32,
    pub status_spacing: f32,
    pub status_icon_text_gap: f32,

    // Quick settings (iOS Control Center look, ARCHITECTURE.md §5.2)
    pub qs_width: f32,
    pub qs_background: Color,
    pub qs_radius: f32,
    pub qs_padding: f32,
    pub qs_gap: f32,
    pub qs_tile: Color,
    pub qs_tile_radius: f32,
    pub qs_toggle_size: f32,
    pub qs_toggle_off: Color,
    pub qs_toggle_on: Color,
    pub qs_slider_height: f32,
    pub qs_slider_track: Color,
    pub qs_slider_fill: Color,
    /// Icon colour where it sits on the bright slider fill.
    pub qs_on_fill: Color,
    pub qs_label_size: f32,
    pub qs_dim: Color,

    // Text
    pub font_size: f32,
    pub font_families: &'static [&'static str],

    // General
    pub accent: Color,
    pub padding: f32,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            panel_background: Color::rgb(0x00, 0x00, 0x00),
            panel_foreground: Color::rgb(0xff, 0xff, 0xff),
            panel_height: 32,

            workspace_dot: Color::rgba(0xff, 0xff, 0xff, 0.45),
            workspace_active: Color::rgb(0xff, 0xff, 0xff),
            workspace_dot_size: 7.0,
            workspace_active_width: 28.0,
            workspace_spacing: 6.0,

            qs_width: 360.0,
            qs_background: Color::rgba(0x1c, 0x1c, 0x1e, 0.94),
            qs_radius: 26.0,
            qs_padding: 16.0,
            qs_gap: 12.0,
            qs_tile: Color::rgba(0xff, 0xff, 0xff, 0.08),
            qs_tile_radius: 20.0,
            qs_toggle_size: 48.0,
            qs_toggle_off: Color::rgba(0xff, 0xff, 0xff, 0.14),
            qs_toggle_on: Color::rgb(0x35, 0x84, 0xe4),
            qs_slider_height: 48.0,
            qs_slider_track: Color::rgba(0xff, 0xff, 0xff, 0.14),
            qs_slider_fill: Color::rgba(0xff, 0xff, 0xff, 0.95),
            qs_on_fill: Color::rgb(0x1c, 0x1c, 0x1e),
            qs_label_size: 12.0,
            qs_dim: Color::rgba(0xff, 0xff, 0xff, 0.6),

            status_icon_size: 16.0,
            status_spacing: 12.0,
            status_icon_text_gap: 4.0,

            font_size: 14.0,
            // First one installed wins. Adwaita Sans is GNOME's UI font.
            font_families: &[
                "Adwaita Sans",
                "Cantarell",
                "Inter",
                "Noto Sans",
                "DejaVu Sans",
            ],

            accent: Color::rgb(0x35, 0x84, 0xe4),
            padding: 12.0,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
