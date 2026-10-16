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

    /// Between `self` (t = 0) and `other` (t = 1), alpha included.
    pub fn mix(self, other: Color, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        Self {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
            a: lerp(self.a, other.a),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    /// The dark palette (vs the light one).
    pub dark: bool,

    // Panel
    pub panel_background: Color,
    pub panel_foreground: Color,
    /// Logical pixels.
    pub panel_height: u32,
    /// The bar's bottom corners (0: a flat strip, as in GNOME).
    pub panel_radius: f32,
    /// A soft shadow under the bar, logical pixels tall (0: none).
    pub panel_shadow: u32,

    // Workspace indicator (GNOME-style dots; the active one is a pill)
    pub workspace_dot: Color,
    pub workspace_active: Color,
    pub workspace_dot_size: f32,
    pub workspace_active_width: f32,
    pub workspace_spacing: f32,

    // Panel buttons (Activities, status): a pill behind them on hover, and
    // a brighter one while what they open is open.
    pub panel_button_hover: Color,
    pub panel_button_active: Color,
    /// Gap between the pill and the panel's top and bottom edges.
    pub panel_button_inset: f32,
    /// Extra width either side of the button's content.
    pub panel_button_padding: f32,

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

    // Overview search (a pill on top, results card below)
    pub search_width: f32,
    pub search_height: f32,
    pub search_background: Color,
    pub search_text_size: f32,
    pub search_gap: f32,
    pub search_results_radius: f32,
    pub search_row_height: f32,
    pub search_row_radius: f32,
    pub search_icon_size: f32,
    pub search_selected: Color,
    pub search_max_results: usize,

    // App grid (every app, in the overview)
    pub grid_max_width: f32,
    pub grid_padding: f32,
    pub grid_cell_width: f32,
    pub grid_cell_height: f32,
    pub grid_icon_size: f32,

    // App windows (the settings app), libadwaita style
    pub window_background: Color,
    pub window_sidebar: Color,
    pub window_text: Color,
    pub window_dim: Color,
    pub window_header_height: f32,
    pub window_title_size: f32,
    pub sidebar_width: f32,
    pub sidebar_row_height: f32,
    pub sidebar_selected: Color,
    pub hover: Color,
    /// Boxed lists: rows grouped on a rounded card.
    pub card_background: Color,
    pub card_radius: f32,
    pub card_separator: Color,
    pub row_height: f32,
    pub row_subtitle_height: f32,
    pub row_padding: f32,
    pub group_title_size: f32,
    pub subtitle_size: f32,
    pub switch_width: f32,
    pub switch_height: f32,
    pub switch_off: Color,
    pub switch_knob: Color,
    pub button_background: Color,
    pub button_hover: Color,
    pub button_height: f32,
    pub button_radius: f32,
    pub entry_background: Color,
    pub focus_ring: Color,
    pub popover_background: Color,
    pub popover_radius: f32,
    pub error: Color,

    // Text
    pub font_size: f32,
    pub font_families: &'static [&'static str],

    // General
    pub accent: Color,
    /// Text and icons on shell popups: quick settings, search, the dock.
    pub popup_text: Color,
    /// Text and icons on an accent-coloured fill.
    pub on_accent: Color,
    pub padding: f32,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            dark: true,
            panel_background: Color::rgb(0x00, 0x00, 0x00),
            panel_foreground: Color::rgb(0xff, 0xff, 0xff),
            panel_height: 32,
            panel_radius: 14.0,
            panel_shadow: 8,

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

            search_width: 560.0,
            search_height: 48.0,
            search_background: Color::rgba(0x1c, 0x1c, 0x1e, 0.94),
            search_text_size: 17.0,
            search_gap: 10.0,
            search_results_radius: 24.0,
            search_row_height: 56.0,
            search_row_radius: 14.0,
            search_icon_size: 36.0,
            search_selected: Color::rgba(0xff, 0xff, 0xff, 0.12),
            search_max_results: 7,

            grid_max_width: 1240.0,
            grid_padding: 28.0,
            grid_cell_width: 136.0,
            grid_cell_height: 124.0,
            grid_icon_size: 64.0,

            panel_button_hover: Color::rgba(0xff, 0xff, 0xff, 0.12),
            panel_button_active: Color::rgba(0xff, 0xff, 0xff, 0.22),
            panel_button_inset: 4.0,
            panel_button_padding: 10.0,

            status_icon_size: 16.0,
            status_spacing: 12.0,
            status_icon_text_gap: 4.0,

            window_background: Color::rgb(0x22, 0x22, 0x26),
            window_sidebar: Color::rgb(0x2e, 0x2e, 0x32),
            window_text: Color::rgb(0xff, 0xff, 0xff),
            window_dim: Color::rgba(0xff, 0xff, 0xff, 0.55),
            window_header_height: 42.0,
            window_title_size: 14.0,
            sidebar_width: 204.0,
            sidebar_row_height: 34.0,
            sidebar_selected: Color::rgba(0xff, 0xff, 0xff, 0.10),
            hover: Color::rgba(0xff, 0xff, 0xff, 0.05),
            card_background: Color::rgba(0xff, 0xff, 0xff, 0.08),
            card_radius: 12.0,
            card_separator: Color::rgba(0x00, 0x00, 0x06, 0.36),
            row_height: 42.0,
            row_subtitle_height: 54.0,
            row_padding: 12.0,
            group_title_size: 14.0,
            subtitle_size: 13.0,
            switch_width: 48.0,
            switch_height: 26.0,
            switch_off: Color::rgba(0xff, 0xff, 0xff, 0.20),
            switch_knob: Color::rgb(0xff, 0xff, 0xff),
            button_background: Color::rgba(0xff, 0xff, 0xff, 0.10),
            button_hover: Color::rgba(0xff, 0xff, 0xff, 0.15),
            button_height: 34.0,
            button_radius: 8.0,
            entry_background: Color::rgba(0xff, 0xff, 0xff, 0.10),
            focus_ring: Color::rgba(0x35, 0x84, 0xe4, 0.6),
            popover_background: Color::rgb(0x38, 0x38, 0x3c),
            popover_radius: 12.0,
            error: Color::rgb(0xff, 0x7b, 0x63),

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
            popup_text: Color::rgb(0xff, 0xff, 0xff),
            on_accent: Color::rgb(0xff, 0xff, 0xff),
            padding: 12.0,
        }
    }
}

impl Theme {
    /// libadwaita's light palette, the top bar included.
    pub fn light() -> Self {
        let ink = |a: f32| Color::rgba(0x00, 0x00, 0x06, a);
        let paper = Color::rgba(0xfa, 0xfa, 0xfb, 0.96);
        Self {
            dark: false,
            qs_background: paper,
            qs_tile: ink(0.05),
            qs_toggle_off: ink(0.10),
            qs_slider_track: ink(0.10),
            qs_slider_fill: Color::rgb(0x35, 0x84, 0xe4),
            qs_on_fill: Color::rgb(0xff, 0xff, 0xff),
            qs_dim: ink(0.55),
            search_background: paper,
            search_selected: ink(0.08),
            panel_background: Color::rgb(0xfa, 0xfa, 0xfb),
            panel_foreground: Color::rgb(0x24, 0x24, 0x28),
            panel_button_hover: ink(0.08),
            panel_button_active: ink(0.14),
            popup_text: Color::rgb(0x24, 0x24, 0x28),

            window_background: Color::rgb(0xfa, 0xfa, 0xfb),
            window_sidebar: Color::rgb(0xeb, 0xeb, 0xed),
            window_text: Color::rgb(0x24, 0x24, 0x28),
            window_dim: ink(0.55),
            sidebar_selected: ink(0.10),
            hover: ink(0.04),
            card_background: Color::rgb(0xff, 0xff, 0xff),
            card_separator: ink(0.09),
            switch_off: ink(0.15),
            button_background: ink(0.08),
            button_hover: ink(0.12),
            entry_background: ink(0.06),
            popover_background: Color::rgb(0xff, 0xff, 0xff),
            error: Color::rgb(0xc3, 0x00, 0x00),
            ..Self::dark()
        }
    }

    pub fn for_dark(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// The theme the config's appearance asks for.
    pub fn for_appearance(appearance: &cnde_config::Appearance) -> Self {
        let dark = appearance.color_scheme == cnde_config::ColorScheme::Dark;
        match (appearance.palette, dark) {
            (cnde_config::Palette::Adwaita, dark) => Self::for_dark(dark),
            (cnde_config::Palette::Cnde, true) => Self::dusk(),
            (cnde_config::Palette::Cnde, false) => Self::blossom(),
        }
    }

    /// cnde's dark palette: deep plum, lavender-white text, a pastel pink
    /// accent and lavender for focus.
    pub fn dusk() -> Self {
        let text = |a: f32| Color::rgba(0xec, 0xe6, 0xf5, a);
        let pink = Color::rgb(0xf4, 0xa7, 0xc6);
        let glass = Color::rgba(0x22, 0x1c, 0x2e, 0.95);
        Self {
            panel_background: Color::rgb(0x15, 0x11, 0x1d),
            panel_foreground: text(1.0),
            panel_button_hover: text(0.12),
            panel_button_active: text(0.22),
            qs_background: glass,
            qs_tile: text(0.07),
            qs_toggle_off: text(0.13),
            qs_toggle_on: pink,
            qs_slider_track: text(0.13),
            qs_slider_fill: pink,
            qs_on_fill: Color::rgb(0x2a, 0x1a, 0x26),
            qs_dim: text(0.6),
            search_background: glass,
            search_selected: text(0.11),
            window_background: Color::rgb(0x1c, 0x17, 0x26),
            window_sidebar: Color::rgb(0x24, 0x1e, 0x31),
            window_text: text(1.0),
            window_dim: text(0.58),
            sidebar_selected: text(0.09),
            hover: text(0.05),
            card_background: text(0.06),
            card_separator: Color::rgba(0x08, 0x04, 0x10, 0.4),
            switch_off: text(0.18),
            button_background: text(0.09),
            button_hover: text(0.14),
            entry_background: text(0.09),
            focus_ring: Color::rgba(0xb8, 0xa4, 0xe8, 0.7),
            popover_background: Color::rgb(0x2f, 0x28, 0x40),
            error: Color::rgb(0xff, 0x8f, 0xa3),
            accent: pink,
            popup_text: text(1.0),
            on_accent: Color::rgb(0x2a, 0x1a, 0x26),
            ..Self::dark()
        }
    }

    /// cnde's light palette: blush white, plum ink and a rose accent.
    pub fn blossom() -> Self {
        let ink = |a: f32| Color::rgba(0x2d, 0x22, 0x38, a);
        let rose = Color::rgb(0xd9, 0x66, 0x9a);
        let paper = Color::rgba(0xfd, 0xf3, 0xf7, 0.97);
        Self {
            panel_background: Color::rgb(0xf8, 0xe6, 0xee),
            panel_foreground: ink(1.0),
            panel_button_hover: ink(0.08),
            panel_button_active: ink(0.14),
            qs_background: paper,
            qs_tile: ink(0.05),
            qs_toggle_off: ink(0.10),
            qs_toggle_on: rose,
            qs_slider_track: ink(0.10),
            qs_slider_fill: rose,
            qs_on_fill: Color::rgb(0xff, 0xff, 0xff),
            qs_dim: ink(0.58),
            search_background: paper,
            search_selected: ink(0.08),
            popup_text: ink(1.0),
            window_background: Color::rgb(0xfd, 0xf3, 0xf7),
            window_sidebar: Color::rgb(0xf8, 0xe6, 0xee),
            window_text: ink(1.0),
            window_dim: ink(0.58),
            sidebar_selected: ink(0.09),
            hover: ink(0.04),
            card_background: Color::rgb(0xff, 0xff, 0xff),
            card_separator: ink(0.08),
            switch_off: ink(0.16),
            button_background: ink(0.07),
            button_hover: ink(0.11),
            entry_background: ink(0.06),
            focus_ring: Color::rgba(0x9b, 0x84, 0xd6, 0.7),
            popover_background: Color::rgb(0xff, 0xff, 0xff),
            error: Color::rgb(0xc0, 0x26, 0x4d),
            accent: rose,
            on_accent: Color::rgb(0xff, 0xff, 0xff),
            ..Self::light()
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
