//! The cnde configuration: schema, defaults, loading and validation
//! (ARCHITECTURE.md §8). Shared by the compositor, and later the shell and
//! the settings app, so hand-edits and the GUI always agree.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

mod edit;
pub use edit::ConfigFile;
pub mod schedule;
pub use schedule::{DarkMode, DarkSchedule, Location};
// The value type `ConfigFile::set` takes.
pub use toml_edit;

/// Written to disk on first run so every option is discoverable.
pub const DEFAULT_CONFIG: &str = include_str!("default-config.toml");

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub input: Input,
    /// Per-monitor settings, matched by connector name (e.g. "DP-2").
    #[serde(rename = "output")]
    pub outputs: Vec<OutputConfig>,
    pub appearance: Appearance,
    pub wallpaper: Wallpaper,
    pub animations: Animations,
    pub panel: Panel,
    pub dock: Dock,
    pub launcher: Launcher,
    pub autostart: Autostart,
    pub lock: Lock,
    /// Switching to the dark style and back by the time of day.
    pub dark_mode: DarkMode,
    /// For sunset and sunrise (schedule.rs).
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub keyboard: Keyboard,
    pub mouse: Pointer,
    pub touchpad: Touchpad,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keyboard {
    /// XKB layout(s), e.g. "us" or "us,de". Empty: XKB_DEFAULT_LAYOUT or "us".
    pub layout: String,
    pub variant: String,
    /// XKB options, e.g. "caps:escape".
    pub options: String,
    /// Milliseconds before a held key starts repeating.
    pub repeat_delay: i32,
    /// Repeats per second.
    pub repeat_rate: i32,
    /// Super+Alt+Left/Right move the content that way (Left shows the next
    /// workspace), like natural scrolling. Off: Left goes to the previous.
    pub natural_workspace_keys: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            layout: String::new(),
            variant: String::new(),
            options: String::new(),
            repeat_delay: 200,
            repeat_rate: 25,
            natural_workspace_keys: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccelProfile {
    /// Faster movement goes further (libinput's default).
    #[default]
    Adaptive,
    /// Distance is proportional to movement; what gamers usually want.
    Flat,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pointer {
    /// -1.0 (slowest) to 1.0 (fastest); 0.0 is libinput's default.
    pub speed: f64,
    pub accel_profile: AccelProfile,
    pub natural_scroll: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Touchpad {
    pub speed: f64,
    pub accel_profile: AccelProfile,
    pub natural_scroll: bool,
    pub tap_to_click: bool,
    /// Tap, then touch again and move, to drag.
    pub tap_and_drag: bool,
    pub disable_while_typing: bool,
    /// Finger travel for a three-finger swipe to move one workspace, in
    /// touchpad units. Larger feels slower.
    pub workspace_swipe_distance: f64,
}

impl Default for Touchpad {
    fn default() -> Self {
        Self {
            speed: 0.0,
            accel_profile: AccelProfile::Adaptive,
            natural_scroll: true,
            tap_to_click: true,
            tap_and_drag: true,
            disable_while_typing: true,
            workspace_swipe_distance: 900.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// Connector name, as logged when the monitor connects (e.g. "DP-2").
    pub name: String,
    /// Top-left corner in the global layout, in logical pixels.
    pub position: Option<Position>,
    /// 1.0 = no scaling; 2.0 for most HiDPI laptops.
    pub scale: Option<f64>,
    /// Resolution and refresh rate. Default: the monitor's preferred
    /// resolution at the fastest refresh it offers for it.
    pub mode: Option<ModeConfig>,
    /// The monitor the workspace strip is anchored to (its workspace is
    /// "the current one"). Default: the leftmost monitor.
    #[serde(default)]
    pub main: bool,
}

/// `mode = { width = 2560, height = 1440, refresh = 179.96 }`
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeConfig {
    pub width: u32,
    pub height: u32,
    /// In Hz; the closest one the monitor offers is used. Default: the
    /// fastest at this resolution.
    pub refresh: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub color_scheme: ColorScheme,
    /// Who draws window title bars, for apps that let the desktop do it.
    pub titlebars: Titlebars,
    /// Buttons at the right end of title bars (ours, and GTK apps').
    pub window_buttons: WindowButtons,
    /// The colours: cnde's own, or GNOME's.
    pub palette: Palette,
    /// Symbolic icons for the shell, Settings and title bars: "cnde" (its
    /// own), "Adwaita", or any installed theme's name. Missing icons come
    /// from cnde's set.
    pub icon_theme: String,
    /// For `palette = "custom"`: the surfaces' colour ("#rrggbb"; its hue
    /// is what matters, lightness is chosen per style) and the accent.
    pub base_color: String,
    pub accent_color: String,
    /// Window corners, logical px. Everything else rounded (dock, bar,
    /// menus, cards) scales with it: 0 squares them all.
    pub corner_radius: u32,
}

impl Appearance {
    /// The default corner radius.
    pub const CORNER_RADIUS: u32 = 12;

    /// How much rounder than the default: multiply a built-in radius by it.
    pub fn roundness(&self) -> f32 {
        self.corner_radius as f32 / Self::CORNER_RADIUS as f32
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            color_scheme: ColorScheme::default(),
            titlebars: Titlebars::default(),
            window_buttons: WindowButtons::default(),
            palette: Palette::default(),
            icon_theme: "cnde".into(),
            base_color: "#2b1a40".into(),
            accent_color: "#f4a7c6".into(),
            corner_radius: Self::CORNER_RADIUS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Palette {
    /// Dusk (deep plum, pastel pink) when dark, Blossom (blush, rose)
    /// when light.
    #[default]
    Cnde,
    /// libadwaita's greys and blue.
    Adwaita,
    /// Worked out from `base_color` and `accent_color`.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowButtons {
    /// Minimize, maximize and close.
    #[default]
    All,
    /// Close only, as in GNOME.
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Titlebars {
    /// cnde draws them, in its own style, for every app that allows it.
    /// Apps with their own header bars (GNOME apps) keep those.
    #[default]
    Cnde,
    /// Apps draw their own, whatever they look like.
    App,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Wallpaper {
    /// Image file (PNG, JPEG or WebP); "~/" means the home directory.
    /// Empty: the built-in gradient.
    pub light: String,
    /// Used in dark mode. Empty: `light` is used in both.
    pub dark: String,
    pub fit: WallpaperFit,
}

impl Default for Wallpaper {
    fn default() -> Self {
        Self {
            light: String::new(),
            dark: String::new(),
            fit: WallpaperFit::Fill,
        }
    }
}

impl Wallpaper {
    /// The image for `scheme`, with "~/" expanded; `None` for the built-in one.
    pub fn path(&self, scheme: ColorScheme) -> Option<PathBuf> {
        let path = match scheme {
            ColorScheme::Dark if !self.dark.is_empty() => &self.dark,
            _ => &self.light,
        };
        if path.is_empty() {
            return None;
        }
        match path.strip_prefix("~/") {
            Some(rest) => Some(PathBuf::from(std::env::var_os("HOME")?).join(rest)),
            None => Some(PathBuf::from(path)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WallpaperFit {
    /// Cover the whole screen, cropping what doesn't fit.
    #[default]
    Fill,
    /// Show the whole image, with bars where it doesn't fill the screen.
    Fit,
    /// Stretch to the screen, ignoring the aspect ratio.
    Stretch,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Animations {
    /// Multiplier for every animation duration: 0.5 is twice as fast,
    /// 0.0 turns animations off.
    pub speed: f64,
}

impl Default for Animations {
    fn default() -> Self {
        Self { speed: 1.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Panel {
    /// Show the top bar. Off: bring your own (waybar etc.) via autostart;
    /// the overview, dock and screenshot tool keep working.
    pub enabled: bool,
    /// Show the panel on every monitor instead of only the main one.
    pub all_monitors: bool,
    /// Logical pixels.
    pub height: u32,
    /// Rounded bottom corners and a soft shadow; off, a flat strip.
    pub rounded: bool,
    /// Translucent over the blurred wallpaper.
    pub frosted: bool,
    /// "#rrggbb" or "#rrggbbaa"; empty follows the light/dark style.
    pub background: String,
    pub foreground: String,
    pub font_size: f32,
    /// strftime-style, e.g. "%H:%M" or "%a %b %-d  %I:%M %p".
    pub clock_format: String,
    pub clock_position: ClockPosition,
    pub workspace_dot_size: f32,
    /// Width of the pill marking active workspaces.
    pub workspace_active_width: f32,
    /// Space between the screen's sides and the first and last buttons.
    pub padding: f32,
    /// Room either side of each button's content (Activities, status).
    pub button_padding: f32,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            enabled: true,
            all_monitors: false,
            height: 32,
            rounded: false,
            frosted: false,
            background: String::new(),
            foreground: String::new(),
            font_size: 14.0,
            clock_format: "%a %b %-d  %H:%M".into(),
            clock_position: ClockPosition::Center,
            workspace_dot_size: 7.0,
            workspace_active_width: 28.0,
            padding: 12.0,
            button_padding: 10.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockPosition {
    /// Next to the workspace dots.
    Left,
    #[default]
    Center,
    /// Next to the status icons.
    Right,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dock {
    pub position: DockPosition,
    /// For a dock on a screen edge.
    pub visibility: DockVisibility,
    /// Desktop file IDs, e.g. "firefox.desktop". Ones not installed are
    /// skipped.
    pub favorites: Vec<String>,
    /// Logical pixels.
    pub icon_size: u32,
    /// Room either side of each icon; twice this between two icons.
    pub padding: f32,
    /// Which end of the icons the app grid button sits at.
    pub launcher: LauncherSide,
    /// Space between the app grid button and the nearest icon.
    pub launcher_gap: f32,
    /// What's behind the icons.
    pub background: DockBackground,
    /// On a screen edge: stretch across the whole edge instead of sitting
    /// in the middle, sized to its icons.
    pub extend: bool,
    /// Where along its edge it sits; stretched, where its icons sit.
    pub align: DockAlign,
    /// On a screen edge: maximized windows stop at it. Off, it floats over
    /// them.
    pub reserve: bool,
    /// Icons and the dock itself can't be dragged, and edit mode is off.
    pub locked: bool,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            position: DockPosition::Overview,
            visibility: DockVisibility::Always,
            favorites: vec![
                "firefox.desktop".into(),
                "org.gnome.Nautilus.desktop".into(),
                "Alacritty.desktop".into(),
            ],
            icon_size: 48,
            padding: 8.0,
            launcher: LauncherSide::End,
            launcher_gap: 12.0,
            background: DockBackground::Solid,
            extend: false,
            align: DockAlign::Center,
            reserve: true,
            locked: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LauncherSide {
    /// Before the first icon (left, or top on a side edge).
    Start,
    #[default]
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockAlign {
    /// Left (or top, on a side edge).
    Start,
    #[default]
    Center,
    /// Right (or bottom, on a side edge).
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockBackground {
    /// A filled colour from the style.
    #[default]
    Solid,
    /// Translucent over a blurred copy of the wallpaper.
    Frosted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockPosition {
    /// Only in the overview (GNOME).
    #[default]
    Overview,
    Bottom,
    Left,
    Right,
    /// No dock.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockVisibility {
    /// Always shown; maximized windows stop at it.
    #[default]
    Always,
    /// Hides when a window would cover it. (Not yet: behaves as "always".)
    Intellihide,
    /// Hidden until the pointer touches that edge. (Not yet: as "always".)
    Autohide,
}

/// Parse "#rrggbb" or "#rrggbbaa" into (r, g, b, a).
pub fn parse_color(text: &str) -> Option<(u8, u8, u8, u8)> {
    let hex = text.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        6 => Some((byte(0)?, byte(2)?, byte(4)?, 255)),
        8 => Some((byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Launcher {
    /// Run when Super is tapped on its own, instead of opening the
    /// overview (e.g. "fuzzel"). Empty: the overview.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Autostart {
    /// Commands run once when the compositor starts.
    pub commands: Vec<String>,
    /// The shell (top bar, dock, overview search), kept running for the
    /// whole session and restarted if it crashes. Another program can take
    /// its place, or "" for none.
    pub shell: String,
    /// Start PipeWire if the system doesn't (no systemd, e.g. Alpine).
    pub audio: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lock {
    /// The program that locks the screen (Super+L): any ext-session-lock
    /// locker, e.g. "swaylock". Restarted if it dies while locked.
    pub command: String,
}

impl Default for Lock {
    fn default() -> Self {
        Self {
            command: "cnde-lock".into(),
        }
    }
}

impl Default for Autostart {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            shell: "cnde-shell".into(),
            audio: true,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, toml::de::Error),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(path, err) => write!(f, "{}: {err}", path.display()),
            Error::Parse(path, err) => write!(f, "{}: {err}", path.display()),
            Error::Invalid(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// `$XDG_CONFIG_HOME/cnde/config.toml`, falling back to `~/.config`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("cnde").join("config.toml"))
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let config: Config =
            toml::from_str(text).map_err(|err| Error::Parse(PathBuf::from("config"), err))?;
        config.validate()?;
        Ok(config)
    }

    /// Load `path`; a missing file is written with [`DEFAULT_CONFIG`] first.
    pub fn load_or_create(path: &Path) -> Result<Self, Error> {
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|err| Error::Io(dir.into(), err))?;
            }
            std::fs::write(path, DEFAULT_CONFIG).map_err(|err| Error::Io(path.into(), err))?;
        }
        let text = std::fs::read_to_string(path).map_err(|err| Error::Io(path.into(), err))?;
        let config: Config = toml::from_str(&text).map_err(|err| Error::Parse(path.into(), err))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Error> {
        let in_range = |name: &str, value: f64, min: f64, max: f64| {
            if (min..=max).contains(&value) {
                Ok(())
            } else {
                Err(Error::Invalid(format!(
                    "{name} must be between {min} and {max}, got {value}"
                )))
            }
        };
        in_range("input.mouse.speed", self.input.mouse.speed, -1.0, 1.0)?;
        in_range("input.touchpad.speed", self.input.touchpad.speed, -1.0, 1.0)?;
        in_range("animations.speed", self.animations.speed, 0.0, 10.0)?;
        in_range(
            "appearance.corner_radius",
            self.appearance.corner_radius as f64,
            0.0,
            48.0,
        )?;
        in_range(
            "input.touchpad.workspace_swipe_distance",
            self.input.touchpad.workspace_swipe_distance,
            100.0,
            5000.0,
        )?;
        if self.input.keyboard.repeat_rate < 0 || self.input.keyboard.repeat_delay < 0 {
            return Err(Error::Invalid(
                "keyboard repeat values can't be negative".into(),
            ));
        }
        let panel = &self.panel;
        for (name, color) in [
            ("panel.background", &panel.background),
            ("panel.foreground", &panel.foreground),
            ("appearance.base_color", &self.appearance.base_color),
            ("appearance.accent_color", &self.appearance.accent_color),
        ] {
            if !color.is_empty() && parse_color(color).is_none() {
                return Err(Error::Invalid(format!(
                    "{name} must be \"#rrggbb\" or \"#rrggbbaa\", got \"{color}\""
                )));
            }
        }
        if !(16..=96).contains(&panel.height) {
            return Err(Error::Invalid(format!(
                "panel.height must be 16 to 96, got {}",
                panel.height
            )));
        }
        in_range("panel.font_size", panel.font_size as f64, 6.0, 48.0)?;
        in_range(
            "panel.workspace_dot_size",
            panel.workspace_dot_size as f64,
            2.0,
            32.0,
        )?;
        in_range(
            "panel.workspace_active_width",
            panel.workspace_active_width as f64,
            2.0,
            96.0,
        )?;
        in_range("panel.padding", panel.padding as f64, 0.0, 400.0)?;
        in_range(
            "panel.button_padding",
            panel.button_padding as f64,
            0.0,
            32.0,
        )?;
        in_range("dock.padding", self.dock.padding as f64, 0.0, 32.0)?;
        in_range(
            "dock.launcher_gap",
            self.dock.launcher_gap as f64,
            0.0,
            400.0,
        )?;
        if !(24..=128).contains(&self.dock.icon_size) {
            return Err(Error::Invalid(format!(
                "dock.icon_size must be 24 to 128, got {}",
                self.dock.icon_size
            )));
        }
        for output in &self.outputs {
            if let Some(scale) = output.scale {
                in_range(
                    &format!("output \"{}\" scale", output.name),
                    scale,
                    0.5,
                    4.0,
                )?;
            }
        }
        schedule::validate(self)?;
        Ok(())
    }

    pub fn output(&self, name: &str) -> Option<&OutputConfig> {
        self.outputs.iter().find(|o| o.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_parses_to_defaults() {
        let parsed = Config::parse(DEFAULT_CONFIG).expect("default config must parse");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn empty_file_is_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn typos_are_errors() {
        assert!(Config::parse("[input.mouse]\nsped = 0.5\n").is_err());
    }

    #[test]
    fn out_of_range_is_an_error() {
        assert!(Config::parse("[input.mouse]\nspeed = 3.0\n").is_err());
    }

    #[test]
    fn dock_lock_parses() {
        assert!(!Config::default().dock.locked);
        let config = Config::parse("[dock]\nlocked = true\n").unwrap();
        assert!(config.dock.locked);
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#ff8000"), Some((255, 128, 0, 255)));
        assert_eq!(parse_color("#00000080"), Some((0, 0, 0, 128)));
        assert_eq!(parse_color("ff8000"), None);
        assert!(Config::parse("[panel]\nbackground = \"black\"\n").is_err());
    }

    #[test]
    fn wallpaper_variants() {
        let config = Config::parse(
            "[wallpaper]\nlight = \"/a.png\"\n[appearance]\ncolor_scheme = \"light\"\n",
        )
        .unwrap();
        assert_eq!(config.appearance.color_scheme, ColorScheme::Light);
        // No dark variant: the light one is used in dark mode too.
        assert_eq!(
            config.wallpaper.path(ColorScheme::Dark),
            Some(PathBuf::from("/a.png"))
        );
        assert_eq!(Wallpaper::default().path(ColorScheme::Dark), None);
    }

    #[test]
    fn outputs_parse() {
        let config = Config::parse(
            "[[output]]\nname = \"DP-2\"\nposition = { x = 1920, y = 0 }\nscale = 1.5\n",
        )
        .unwrap();
        let output = config.output("DP-2").unwrap();
        assert_eq!(output.position, Some(Position { x: 1920, y: 0 }));
        assert_eq!(output.scale, Some(1.5));
        assert_eq!(output.mode, None);
        let config = Config::parse(
            "[[output]]\nname = \"DP-2\"\nmode = { width = 2560, height = 1440, refresh = 179.96 }\n",
        )
        .unwrap();
        assert_eq!(
            config.output("DP-2").unwrap().mode,
            Some(ModeConfig {
                width: 2560,
                height: 1440,
                refresh: Some(179.96)
            })
        );
    }
}
