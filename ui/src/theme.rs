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

    /// Near-black or white, whichever reads on this colour.
    pub fn ink(self) -> Color {
        let luminance = 0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b;
        if luminance > 0.55 {
            Color::rgb(0x1b, 0x1d, 0x1a)
        } else {
            Color::rgb(0xff, 0xff, 0xff)
        }
    }

    /// Hue (0-360), saturation and lightness (0-1).
    pub fn hsl(self) -> (f32, f32, f32) {
        let (r, g, b) = (self.r, self.g, self.b);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let d = max - min;
        if d < 1e-6 {
            return (0.0, 0.0, l);
        }
        let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-6);
        let h = if max == r {
            60.0 * ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        (h, s.clamp(0.0, 1.0), l)
    }

    pub fn from_hsl(h: f32, s: f32, l: f32) -> Self {
        let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
        let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
        let m = l - c / 2.0;
        let (r, g, b) = match (h.rem_euclid(360.0) / 60.0) as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        Self {
            r: r + m,
            g: g + m,
            b: b + m,
            a: 1.0,
        }
    }

    /// Relative luminance (WCAG), for picking legible text on a colour.
    pub fn luminance(self) -> f32 {
        let lin = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
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
    /// A floating bar's corners.
    pub panel_float_radius: f32,
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
    /// Space between the screen's sides and the first and last buttons.
    pub panel_padding: f32,

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
    /// Window title bars' top corners.
    pub window_radius: f32,
    /// Right-click menus.
    pub menu_radius: f32,
    /// A dock that doesn't stretch across its edge.
    pub dock_radius: f32,
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
            panel_float_radius: 10.0,
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
            panel_padding: 12.0,

            status_icon_size: 16.0,
            status_spacing: 12.0,
            status_icon_text_gap: 4.0,

            window_background: Color::rgb(0x22, 0x22, 0x26),
            window_sidebar: Color::rgb(0x2e, 0x2e, 0x32),
            window_text: Color::rgb(0xff, 0xff, 0xff),
            window_dim: Color::rgba(0xff, 0xff, 0xff, 0.55),
            window_header_height: 50.0,
            window_title_size: 15.0,
            sidebar_width: 204.0,
            sidebar_row_height: 34.0,
            sidebar_selected: Color::rgba(0xff, 0xff, 0xff, 0.10),
            hover: Color::rgba(0xff, 0xff, 0xff, 0.05),
            card_background: Color::rgba(0xff, 0xff, 0xff, 0.08),
            card_radius: 14.0,
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
            window_radius: 12.0,
            menu_radius: 14.0,
            // cnde_ipc::dock::RADIUS: the compositor frosts behind it.
            dock_radius: 24.0,
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
        Self::colours_for(appearance).rounded(appearance.roundness())
    }

    /// The bar and dock in `tint` (a maximized window's colour, with
    /// `appearance.adaptive_color`), `amount` of the way, with ink that
    /// reads on it.
    pub fn matched(&self, tint: Color, amount: f32) -> Self {
        let ink = tint.ink();
        let background = tint.with_alpha(1.0);
        let fg = |c: Color| c.mix(ink.with_alpha(c.a), amount);
        let wash = |c: Color, a: f32| c.mix(ink.with_alpha(a), amount);
        Self {
            panel_background: self.panel_background.mix(background, amount),
            search_background: self.search_background.mix(background, amount),
            panel_foreground: fg(self.panel_foreground),
            workspace_active: fg(self.workspace_active),
            workspace_dot: fg(self.workspace_dot),
            popup_text: fg(self.popup_text),
            qs_dim: wash(self.qs_dim, 0.6),
            panel_button_hover: wash(self.panel_button_hover, 0.12),
            panel_button_active: wash(self.panel_button_active, 0.2),
            search_selected: wash(self.search_selected, 0.12),
            ..self.clone()
        }
    }

    /// Every corner radius times `k` (1: as designed, 0: square).
    pub fn rounded(mut self, k: f32) -> Self {
        for r in [
            &mut self.panel_radius,
            &mut self.panel_float_radius,
            &mut self.qs_radius,
            &mut self.qs_tile_radius,
            &mut self.search_results_radius,
            &mut self.search_row_radius,
            &mut self.card_radius,
            &mut self.button_radius,
            &mut self.popover_radius,
            &mut self.window_radius,
            &mut self.menu_radius,
            &mut self.dock_radius,
        ] {
            *r *= k;
        }
        self
    }

    fn colours_for(appearance: &cnde_config::Appearance) -> Self {
        let dark = appearance.color_scheme == cnde_config::ColorScheme::Dark;
        match (appearance.palette, dark) {
            (cnde_config::Palette::Adwaita, dark) => Self::for_dark(dark),
            (cnde_config::Palette::Quiet, true) => Self::quiet_dark(),
            (cnde_config::Palette::Quiet, false) => Self::quiet_light(),
            (cnde_config::Palette::Cnde, true) => Self::dusk(),
            (cnde_config::Palette::Cnde, false) => Self::blossom(),
            (cnde_config::Palette::Custom, dark) => {
                let parse = |text: &str, fallback: Color| {
                    cnde_config::parse_color(text)
                        .map_or(fallback, |(r, g, b, _)| Color::rgb(r, g, b))
                };
                let base = parse(&appearance.base_color, Color::rgb(0x1c, 0x17, 0x26));
                let accent = parse(&appearance.accent_color, Color::rgb(0xf4, 0xa7, 0xc6));
                Self::custom(dark, base, accent)
            }
        }
    }

    /// A palette worked out from two colours: every surface takes `base`'s
    /// hue (and some of its saturation) at the lightness its role needs,
    /// dark or light; `accent` marks what's on or chosen.
    pub fn custom(dark: bool, base: Color, accent: Color) -> Self {
        let (h, s, _) = base.hsl();
        // Muted, so a vivid base still reads as a background.
        let s = (s * 0.45).min(0.35);
        let shade = |l: f32| Color::from_hsl(h, s, l);
        let tint = |l: f32, a: f32| Color::from_hsl(h, s * 0.6, l).with_alpha(a);
        let on_accent = if accent.luminance() > 0.4 {
            Color::from_hsl(h, s, 0.12)
        } else {
            Color::rgb(0xff, 0xff, 0xff)
        };
        let accent = accent.with_alpha(1.0);
        if dark {
            let text = |a: f32| tint(0.93, a);
            Self {
                panel_background: shade(0.06),
                panel_foreground: text(1.0),
                panel_button_hover: text(0.12),
                panel_button_active: text(0.22),
                qs_background: shade(0.13).with_alpha(0.95),
                qs_tile: text(0.07),
                qs_toggle_off: text(0.13),
                qs_toggle_on: accent,
                qs_slider_track: text(0.13),
                qs_slider_fill: accent,
                qs_on_fill: on_accent,
                qs_dim: text(0.6),
                search_background: shade(0.13).with_alpha(0.95),
                search_selected: text(0.11),
                window_background: shade(0.11),
                window_sidebar: shade(0.15),
                window_text: text(1.0),
                window_dim: text(0.58),
                sidebar_selected: text(0.09),
                hover: text(0.05),
                card_background: text(0.06),
                card_separator: shade(0.03).with_alpha(0.5),
                switch_off: text(0.18),
                button_background: text(0.09),
                button_hover: text(0.14),
                entry_background: text(0.09),
                focus_ring: accent.with_alpha(0.6),
                popover_background: shade(0.2),
                accent,
                popup_text: text(1.0),
                on_accent,
                ..Self::dark()
            }
        } else {
            let ink = |a: f32| tint(0.17, a);
            Self {
                panel_background: shade(0.92),
                panel_foreground: ink(1.0),
                panel_button_hover: ink(0.08),
                panel_button_active: ink(0.14),
                qs_background: shade(0.97).with_alpha(0.97),
                qs_tile: ink(0.05),
                qs_toggle_off: ink(0.10),
                qs_toggle_on: accent,
                qs_slider_track: ink(0.10),
                qs_slider_fill: accent,
                qs_on_fill: on_accent,
                qs_dim: ink(0.58),
                search_background: shade(0.97).with_alpha(0.97),
                search_selected: ink(0.08),
                popup_text: ink(1.0),
                window_background: shade(0.97),
                window_sidebar: shade(0.93),
                window_text: ink(1.0),
                window_dim: ink(0.58),
                sidebar_selected: ink(0.09),
                hover: ink(0.04),
                card_background: Color::from_hsl(h, s, 0.995),
                card_separator: ink(0.08),
                switch_off: ink(0.16),
                button_background: ink(0.07),
                button_hover: ink(0.11),
                entry_background: ink(0.06),
                focus_ring: accent.with_alpha(0.6),
                popover_background: Color::from_hsl(h, s, 0.995),
                accent,
                on_accent,
                ..Self::light()
            }
        }
    }

    /// Quiet, dark: warm charcoal with a faint sage cast, soft white text
    /// and an amber accent. No cards: sections are labels and thin rules.
    pub fn quiet_dark() -> Self {
        let text = |a: f32| Color::rgba(0xee, 0xf0, 0xec, a);
        let amber = Color::rgb(0xe3, 0xa9, 0x5e);
        let on_amber = Color::rgb(0x1a, 0x14, 0x0b);
        let glass = Color::rgba(0x22, 0x25, 0x23, 0.96);
        Self {
            panel_background: Color::rgb(0x1a, 0x1d, 0x1b),
            panel_foreground: text(1.0),
            panel_button_hover: text(0.10),
            panel_button_active: text(0.18),
            qs_background: glass,
            qs_tile: text(0.0),
            qs_toggle_off: text(0.09),
            qs_toggle_on: amber,
            qs_slider_track: text(0.09),
            qs_slider_fill: text(0.92),
            qs_on_fill: Color::rgb(0x1f, 0x22, 0x20),
            qs_dim: text(0.6),
            search_background: glass,
            search_selected: text(0.09),
            window_background: Color::rgb(0x1f, 0x22, 0x20),
            window_sidebar: Color::rgb(0x26, 0x2a, 0x27),
            window_text: text(1.0),
            window_dim: text(0.6),
            sidebar_selected: amber,
            hover: text(0.06),
            card_background: text(0.0),
            card_separator: text(0.08),
            switch_off: text(0.16),
            button_background: text(0.08),
            button_hover: text(0.13),
            entry_background: text(0.07),
            focus_ring: amber.with_alpha(0.6),
            popover_background: Color::rgb(0x2a, 0x2e, 0x2b),
            error: Color::rgb(0xff, 0x8a, 0x7a),
            accent: amber,
            popup_text: text(1.0),
            on_accent: on_amber,
            ..Self::dark().quiet_shapes()
        }
    }

    /// Quiet, light: warm off-white, near-black ink, a deeper amber.
    pub fn quiet_light() -> Self {
        let ink = |a: f32| Color::rgba(0x1b, 0x1d, 0x1a, a);
        let amber = Color::rgb(0xb5, 0x6f, 0x1c);
        let paper = Color::rgba(0xf7, 0xf7, 0xf4, 0.97);
        Self {
            panel_background: Color::rgb(0xf4, 0xf5, 0xf1),
            panel_foreground: ink(1.0),
            panel_button_hover: ink(0.07),
            panel_button_active: ink(0.12),
            qs_background: paper,
            qs_tile: ink(0.0),
            qs_toggle_off: ink(0.06),
            qs_toggle_on: amber,
            qs_slider_track: ink(0.08),
            qs_slider_fill: amber,
            qs_on_fill: Color::rgb(0xff, 0xff, 0xff),
            qs_dim: ink(0.58),
            search_background: paper,
            search_selected: ink(0.07),
            popup_text: ink(1.0),
            window_background: Color::rgb(0xfb, 0xfb, 0xf9),
            window_sidebar: Color::rgb(0xef, 0xf1, 0xec),
            window_text: ink(1.0),
            window_dim: ink(0.58),
            sidebar_selected: amber,
            hover: ink(0.045),
            card_background: ink(0.0),
            card_separator: ink(0.08),
            switch_off: ink(0.14),
            button_background: ink(0.06),
            button_hover: ink(0.10),
            entry_background: ink(0.05),
            focus_ring: amber.with_alpha(0.6),
            popover_background: Color::rgb(0xff, 0xff, 0xfd),
            error: Color::rgb(0xb4, 0x23, 0x18),
            accent: amber,
            on_accent: Color::rgb(0xff, 0xff, 0xff),
            ..Self::light().quiet_shapes()
        }
    }

    /// Quiet's sizes and corners (at the default roundness): tighter than
    /// Adwaita's, so the larger corner setting still reads as calm.
    fn quiet_shapes(self) -> Self {
        Self {
            panel_radius: 0.0,
            panel_shadow: 0,
            qs_radius: 18.0,
            qs_tile_radius: 14.0,
            qs_padding: 14.0,
            search_results_radius: 18.0,
            search_row_radius: 10.0,
            card_radius: 10.0,
            button_radius: 8.0,
            popover_radius: 12.0,
            menu_radius: 12.0,
            dock_radius: 16.0,
            ..self
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
