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
pub mod paths;
pub mod power;
pub use power::{IdleStage, Power, PowerProfile};
pub mod schedule;
pub use schedule::{DarkMode, DarkSchedule, Location, NightLight, NightSchedule};
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
    /// Screen sharing through xdg-desktop-portal.
    pub portal: Portal,
    pub tiling: Tiling,
    /// How workspaces relate to monitors.
    pub workspaces: Workspaces,
    /// Snapping windows dragged to a monitor's edges.
    pub snap: Snap,
    /// Switching to the dark style and back by the time of day.
    pub dark_mode: DarkMode,
    /// A warm colour filter, on a schedule.
    pub night_light: NightLight,
    /// For sunset and sunrise (schedule.rs).
    pub location: Location,
    /// Dimming, screen off and suspend when idle.
    pub power: Power,
    pub sound: Sound,
    pub notifications: Notifications,
    /// The file manager.
    pub files: Files,
    /// Which program does what.
    pub apps: Apps,
    /// Keyboard shortcuts.
    pub shortcuts: Shortcuts,
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
    /// Variable refresh rate (FreeSync / Adaptive-Sync): the monitor waits
    /// for each frame instead of refreshing on a fixed clock.
    #[serde(default)]
    pub vrr: Vrr,
    /// Which way the monitor is turned; the picture turns to match.
    #[serde(default)]
    pub rotation: Rotation,
}

/// `rotation = "normal" | "left" | "right" | "upside-down"`: which way the
/// monitor itself was turned (its top now at the left, at the right, or at
/// the bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rotation {
    #[default]
    Normal,
    Left,
    Right,
    UpsideDown,
}

/// `vrr = "off" | "on" | "fullscreen"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vrr {
    #[default]
    Off,
    /// Always; some monitors flicker at low frame rates on the desktop.
    On,
    /// Only while a fullscreen window (a game, a video) is on top.
    Fullscreen,
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
    /// is what matters, lightness is chosen per style) and the accent
    /// (or "auto": a clear shade of the wallpaper's hue).
    pub base_color: String,
    pub accent_color: String,
    /// Window corners, logical px. Everything else rounded (dock, bar,
    /// menus, cards) scales with it: 0 squares them all.
    pub corner_radius: u32,
    /// The shell's popups and cnde's own windows: see-through over the
    /// blurred wallpaper, or solid. Snapped or maximized windows are
    /// always solid.
    pub material: Material,
    /// The old switch for `palette = "adaptive"`; true means the same.
    #[serde(alias = "match_windows")]
    pub adaptive_color: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Material {
    #[default]
    Frosted,
    Solid,
}

impl Appearance {
    /// Whether the colours follow the screen (`palette = "adaptive"`).
    pub fn adaptive(&self) -> bool {
        self.palette == Palette::Adaptive || self.adaptive_color
    }

    /// Whether the accent is "auto": a shade of the wallpaper's hue.
    pub fn accent_auto(&self) -> bool {
        self.accent_color.trim().eq_ignore_ascii_case("auto")
    }

    /// The default corner radius.
    pub const CORNER_RADIUS: u32 = 10;
    /// The radius the built-in shapes are drawn for.
    const DESIGN_RADIUS: f32 = 12.0;

    /// How much rounder than designed: multiply a built-in radius by it.
    pub fn roundness(&self) -> f32 {
        self.corner_radius as f32 / Self::DESIGN_RADIUS
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
            material: Material::Frosted,
            adaptive_color: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Palette {
    /// Warm charcoal or off-white with an amber accent; no cards.
    #[default]
    Quiet,
    /// Dusk (deep plum, pastel pink) when dark, Blossom (blush, rose)
    /// when light.
    Cnde,
    /// libadwaita's greys and blue.
    Adwaita,
    /// Worked out from `base_color` and `accent_color`.
    Custom,
    /// Worked out like `Custom` from the wallpaper's colour. The bar and
    /// dock match a window against them exactly.
    Adaptive,
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

/// Default programs that have no MIME type of their own. (The rest are in
/// `mimeapps.list`.)
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Apps {
    /// The terminal emulator, as a desktop file ID (`foot.desktop`); ""
    /// looks for one (`$TERMINAL`, then common ones).
    pub terminal: String,
}

/// One shortcut action's value: a combo, or a list of them.
#[derive(Deserialize)]
#[serde(untagged)]
enum Combos {
    One(String),
    Many(Vec<String>),
}

impl Combos {
    /// The combos; "" counts as none, so it turns an action off like [].
    fn into_vec(self) -> Vec<String> {
        let all = match self {
            Combos::One(k) => vec![k],
            Combos::Many(k) => k,
        };
        all.into_iter().filter(|k| !k.trim().is_empty()).collect()
    }
}

fn combos<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<String>, D::Error> {
    Combos::deserialize(de).map(Combos::into_vec)
}

/// Every bindable action: its config name, default combos and a label.
/// "Super" is the compositor's mod key (Alt when nested). The escape hatch
/// (Ctrl+Alt+Backspace), VT switching and the media keys are not listed:
/// they are fixed.
pub const SHORTCUT_DEFAULTS: &[(&str, &[&str], &str)] = &[
    ("terminal", &["Super+Q"], "Open a terminal"),
    ("browser", &["Super+F"], "Open the web browser"),
    ("files", &["Super+E"], "Open the file manager"),
    ("system_monitor", &["Ctrl+Shift+Escape"], "Open the system monitor"),
    ("close", &["Super+C", "Alt+F4"], "Close the window"),
    ("lock", &["Super+L"], "Lock the screen"),
    ("app_grid", &["Super+A"], "Show the app grid"),
    ("show_desktop", &["Super+D"], "Show the desktop, again to bring windows back"),
    ("screenshot", &["Super+Shift+S", "Print"], "Take a screenshot"),
    ("switcher", &["Alt+Tab", "Super+Tab"], "Switch windows"),
    ("switcher_back", &["Alt+Shift+Tab", "Super+Shift+Tab"], "Switch windows, backwards"),
    ("maximize", &["Super+Up"], "Maximize (tiling: focus up)"),
    ("restore", &["Super+Down"], "Restore (tiling: focus down)"),
    ("snap_left", &["Super+Left"], "Snap left (tiling: focus left)"),
    ("snap_right", &["Super+Right"], "Snap right (tiling: focus right)"),
    ("fullscreen", &["Super+Shift+F"], "Toggle fullscreen"),
    ("toggle_tiling", &["Super+T"], "Tile or float this workspace"),
    ("toggle_floating", &["Super+Shift+T"], "Float or tile the focused window"),
    ("swap_master", &["Super+Return", "Super+KP_Enter"], "Swap with the main tile"),
    ("master_wider", &["Super+bracketright"], "Widen the main tile"),
    ("master_narrower", &["Super+bracketleft"], "Narrow the main tile"),
    ("move_tile_left", &["Super+Shift+Left"], "Move the tile left"),
    ("move_tile_right", &["Super+Shift+Right"], "Move the tile right"),
    ("move_tile_up", &["Super+Shift+Up"], "Move the tile up"),
    ("move_tile_down", &["Super+Shift+Down"], "Move the tile down"),
    ("workspace_prev", &["Super+Alt+Left"], "Previous workspace"),
    ("workspace_next", &["Super+Alt+Right"], "Next workspace"),
    ("workspace_prev_carry", &["Super+Alt+Shift+Left"], "Previous workspace, with the window"),
    ("workspace_next_carry", &["Super+Alt+Shift+Right"], "Next workspace, with the window"),
    ("next_layout", &["Super+Space"], "Switch keyboard layout"),
];

/// A key that runs a command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomShortcut {
    /// A combo like "Super+B", or a list of them.
    #[serde(deserialize_with = "combos")]
    pub keys: Vec<String>,
    pub command: String,
}

/// `[shortcuts]`: action name to combos. Names this build doesn't know are
/// kept (the compositor warns about them) so a typo never fails the load.
#[derive(Debug, Clone, PartialEq)]
pub struct Shortcuts {
    /// Action name to combos, defaults filled in; empty = off.
    pub bindings: std::collections::BTreeMap<String, Vec<String>>,
    pub custom: Vec<CustomShortcut>,
}

impl Default for Shortcuts {
    fn default() -> Self {
        let owned = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect();
        Self {
            bindings: SHORTCUT_DEFAULTS
                .iter()
                .map(|(name, keys, _)| (name.to_string(), owned(keys)))
                .collect(),
            custom: Vec::new(),
        }
    }
}

impl<'de> Deserialize<'de> for Shortcuts {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            custom: Vec<CustomShortcut>,
            #[serde(flatten)]
            rest: std::collections::BTreeMap<String, Combos>,
        }
        let raw = Raw::deserialize(de)?;
        let mut out = Shortcuts {
            custom: raw.custom,
            ..Shortcuts::default()
        };
        for (name, value) in raw.rest {
            out.bindings.insert(name, value.into_vec());
        }
        Ok(out)
    }
}

/// The file manager's icon sizes, in logical pixels.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Files {
    /// Icon size in the grid view, 48 to 192.
    pub grid_icon_size: u32,
    /// Icon size in the list view, 16 to 48.
    pub list_icon_size: u32,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            grid_icon_size: 48,
            list_icon_size: 18,
        }
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
    /// Tray icons (StatusNotifierItem apps: Discord, Steam, ...): behind a
    /// chevron popover, inline in the bar, or off.
    pub tray: TrayMode,
    /// A power icon at the end of the status button.
    pub power_icon: bool,
    /// Media controls (MPRIS players) in the clock's popover.
    pub media_controls: bool,
    /// A microphone icon in the status button while an app is recording.
    pub mic_indicator: bool,
    /// Logical pixels.
    pub height: u32,
    /// Rounded bottom corners and a soft shadow; off, a flat strip.
    pub rounded: bool,
    /// Translucent over the blurred wallpaper (until a window touches it).
    /// Unset follows `appearance.material`.
    pub frosted: Option<bool>,
    /// Attached to the top edge, floating clear of it, or floating until
    /// a window touches it.
    pub mode: BarMode,
    /// How the bar looks: a full-width strip, separate islands, or no
    /// background at all. Unset follows the old `clear` switch.
    pub style: Option<PanelStyle>,
    /// The old switch for `style = "clear"`; still read when `style` is
    /// unset.
    pub clear: bool,
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
    /// Logical pixels of a line along the bar's bottom edge while it takes
    /// a window's colour (adaptive colour): a little darker on a light
    /// colour, lighter on a dark one, so the bar and the window's own title
    /// bar don't run together. 0 is off.
    pub edge_line: u32,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            enabled: true,
            all_monitors: false,
            tray: TrayMode::Collapsed,
            power_icon: true,
            media_controls: true,
            mic_indicator: true,
            height: 32,
            rounded: false,
            frosted: None,
            mode: BarMode::Auto,
            style: None,
            clear: false,
            background: String::new(),
            foreground: String::new(),
            font_size: 14.0,
            clock_format: "%a %b %-d  %H:%M".into(),
            clock_position: ClockPosition::Center,
            workspace_dot_size: 7.0,
            workspace_active_width: 28.0,
            padding: 12.0,
            button_padding: 10.0,
            edge_line: 2,
        }
    }
}

impl Panel {
    /// The bar's style, reading the old `clear` switch when unset.
    pub fn style(&self) -> PanelStyle {
        self.style.unwrap_or(if self.clear {
            PanelStyle::Clear
        } else {
            PanelStyle::Full
        })
    }
}

/// What the top bar's background is while it floats free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelStyle {
    /// One full-width bar.
    #[default]
    Full,
    /// Separate rounded pills for the workspaces, clock and status.
    Islands,
    /// No background: text and icons over the wallpaper.
    Clear,
}

impl PanelStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Islands => "islands",
            Self::Clear => "clear",
        }
    }
}

/// Where tray icons go. Also reads the old `true` / `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrayMode {
    /// Behind a chevron next to the status button.
    #[default]
    Collapsed,
    /// Each icon in the bar.
    Inline,
    Off,
}

impl TrayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Collapsed => "collapsed",
            Self::Inline => "inline",
            Self::Off => "off",
        }
    }
}

impl<'de> Deserialize<'de> for TrayMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Bool(true) => Ok(Self::Collapsed),
            Raw::Bool(false) => Ok(Self::Off),
            Raw::Str(s) => match s.as_str() {
                "collapsed" => Ok(Self::Collapsed),
                "inline" => Ok(Self::Inline),
                "off" => Ok(Self::Off),
                other => Err(serde::de::Error::custom(format!(
                    "tray must be \"collapsed\", \"inline\" or \"off\", not {other:?}"
                ))),
            },
        }
    }
}

/// How the top bar (and the dock) sits against its screen edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BarMode {
    /// Flush against the edge, full width.
    Attached,
    /// Clear of the edges, with rounded corners.
    Floating,
    /// Floating, attaching while a window touches it.
    #[default]
    Auto,
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
    /// On an edge: a dock on every monitor, not only the main one.
    pub all_monitors: bool,
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
    /// What's behind the icons. Unset follows `appearance.material`.
    pub background: Option<DockBackground>,
    /// On a screen edge, while attached: stretch across the whole edge
    /// instead of being sized to its icons.
    pub extend: bool,
    /// On a screen edge: attached to it, floating clear of it, or floating
    /// until a window touches it.
    pub mode: BarMode,
    /// Where along its edge it sits; stretched, where its icons sit.
    pub align: DockAlign,
    /// Where the floating pill sits along its edge; unset, `align`.
    pub float_align: Option<DockAlign>,
    /// With `appearance.adaptive_color`: the bar's colour, or a darker shade.
    pub adaptive_color: DockAdaptive,
    /// On a screen edge: maximized windows stop at it. Off, it floats over
    /// them.
    pub reserve: bool,
    /// Icons and the dock itself can't be dragged, and edit mode is off.
    pub locked: bool,
    /// Clicking the icon of the app whose window is focused minimizes its
    /// windows; clicking again brings them back.
    pub click_minimizes: bool,
    /// Run by "System Monitor" in the dock's menu; empty hides the item.
    pub system_monitor: String,
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            position: DockPosition::Overview,
            visibility: DockVisibility::Always,
            all_monitors: false,
            favorites: vec![
                "firefox.desktop".into(),
                "org.gnome.Nautilus.desktop".into(),
                "Alacritty.desktop".into(),
            ],
            icon_size: 48,
            padding: 8.0,
            launcher: LauncherSide::End,
            launcher_gap: 12.0,
            background: None,
            extend: false,
            mode: BarMode::Auto,
            align: DockAlign::Center,
            float_align: None,
            adaptive_color: DockAdaptive::Bar,
            reserve: true,
            locked: false,
            click_minimizes: true,
            system_monitor: "gnome-system-monitor".into(),
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
pub enum DockAdaptive {
    /// Whatever colour the bar takes.
    #[default]
    Bar,
    /// A darker shade of the window at the bottom edge, or the wallpaper.
    Darker,
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
    /// Slides off the edge while a window touches it (as autohide).
    Intellihide,
    /// Slides off the edge while a window touches it; the pointer on the
    /// edge brings it back.
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
pub struct Sound {
    /// Short sounds from the freedesktop sound theme: a click when the
    /// volume keys change it, a shutter for screenshots, low battery and
    /// plugging in or out.
    pub event_sounds: bool,
}

impl Default for Sound {
    fn default() -> Self {
        Self { event_sounds: true }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    /// The shell shows apps' notifications. Off, or with another daemon
    /// (mako, dunst) already running, it leaves them to that.
    pub enabled: bool,
    /// Only urgent notifications show (quick settings toggles it).
    pub do_not_disturb: bool,
    /// The clock's popover lists the notifications received, grouped by app.
    pub history: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            do_not_disturb: false,
            history: true,
        }
    }
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

/// Which window picks the screen to share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharePicker {
    /// `cnde-share-picker`: cards with a preview of each monitor.
    #[default]
    Cnde,
    /// Leave xdg-desktop-portal-wlr's settings alone.
    Keep,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Portal {
    pub share_picker: SharePicker,
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

/// `[workspaces]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workspaces {
    pub mode: WorkspaceMode,
    /// Pushing the pointer into the main monitor's top-left corner toggles
    /// the overview.
    pub hot_corner: bool,
}

impl Default for Workspaces {
    fn default() -> Self {
        Self {
            mode: WorkspaceMode::default(),
            hot_corner: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    /// One strip across all monitors: they show consecutive workspaces and
    /// switching moves them together.
    #[default]
    Continuous,
    /// Every monitor has its own workspaces, and switching moves only the
    /// monitor you last used.
    Individual,
}

/// `[snap]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Snap {
    /// Logical pixels from a monitor's side or top within which a dragged
    /// window snaps (to that half, or maximized), 2 to 96.
    pub edge: u32,
}

impl Default for Snap {
    fn default() -> Self {
        Self { edge: 10 }
    }
}

/// Hyprland-style master-stack tiling (ARCHITECTURE.md §4.4). Each
/// workspace can be switched with Super+T; until then it uses `layout`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tiling {
    /// How workspaces arrange their windows unless switched.
    pub layout: WindowLayout,
    /// What Super+T switches.
    pub toggle: TilingToggle,
    /// How tiling arranges windows.
    pub style: TilingStyle,
    /// Share of the width the first window takes, 0.1 to 0.9.
    pub master_ratio: f64,
    /// Logical pixels between tiled windows.
    pub gap: u32,
    /// Logical pixels between tiled windows and the screen's edges.
    pub outer_gap: u32,
}

impl Default for Tiling {
    fn default() -> Self {
        Self {
            layout: WindowLayout::Floating,
            toggle: TilingToggle::All,
            style: TilingStyle::Dwindle,
            master_ratio: 0.5,
            gap: 8,
            outer_gap: 8,
        }
    }
}

/// What Super+T (and the `layout` setting) applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TilingToggle {
    /// Tiling is one global mode: `layout` is the live mode of every
    /// workspace, and Super+T flips (and saves) it.
    #[default]
    All,
    /// Super+T switches one workspace; `layout` is how new ones start.
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowLayout {
    /// Windows stay where they're put (GNOME).
    #[default]
    Floating,
    /// Windows share the screen without overlapping (see [`TilingStyle`]).
    Tiling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TilingStyle {
    /// Hyprland's default: each new window splits the last one in half,
    /// along its longer side.
    #[default]
    Dwindle,
    /// One main window on the left, the rest stacked on the right.
    Master,
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
    /// Whether the top bar is frosted: its own setting, else the material.
    pub fn panel_frosted(&self) -> bool {
        self.panel
            .frosted
            .unwrap_or(self.appearance.material == Material::Frosted)
    }

    /// Whether the dock is frosted: its own setting, else the material.
    pub fn dock_frosted(&self) -> bool {
        match self.dock.background {
            Some(background) => background == DockBackground::Frosted,
            None => self.appearance.material == Material::Frosted,
        }
    }

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
        in_range("tiling.master_ratio", self.tiling.master_ratio, 0.1, 0.9)?;
        in_range(
            "files.grid_icon_size",
            self.files.grid_icon_size as f64,
            48.0,
            192.0,
        )?;
        in_range(
            "files.list_icon_size",
            self.files.list_icon_size as f64,
            16.0,
            48.0,
        )?;
        in_range("tiling.gap", self.tiling.gap as f64, 0.0, 64.0)?;
        in_range("snap.edge", self.snap.edge as f64, 2.0, 96.0)?;
        in_range("tiling.outer_gap", self.tiling.outer_gap as f64, 0.0, 128.0)?;
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
        let term = &self.apps.terminal;
        if !term.is_empty() && (!term.ends_with(".desktop") || term.contains('/')) {
            return Err(Error::Invalid(format!(
                "apps.terminal must be a desktop file ID like \"foot.desktop\", got \"{term}\""
            )));
        }
        let panel = &self.panel;
        for (name, color) in [
            ("panel.background", &panel.background),
            ("panel.foreground", &panel.foreground),
            ("appearance.base_color", &self.appearance.base_color),
            ("appearance.accent_color", &self.appearance.accent_color),
        ] {
            if !color.is_empty()
                && parse_color(color).is_none()
                && !(name == "appearance.accent_color" && self.appearance.accent_auto())
            {
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
        if panel.edge_line > 8 {
            return Err(Error::Invalid(format!(
                "panel.edge_line must be 0 to 8, got {}",
                panel.edge_line
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
        power::validate(self)?;
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
    fn tray_reads_modes_and_old_booleans() {
        let tray = |v: &str| Config::parse(&format!("[panel]\ntray = {v}\n")).map(|c| c.panel.tray);
        assert_eq!(tray("true").unwrap(), TrayMode::Collapsed);
        assert_eq!(tray("false").unwrap(), TrayMode::Off);
        assert_eq!(tray("\"inline\"").unwrap(), TrayMode::Inline);
        assert_eq!(tray("\"collapsed\"").unwrap(), TrayMode::Collapsed);
        assert!(tray("\"sideways\"").is_err());
    }

    #[test]
    fn workspace_mode_reads_both_modes() {
        let mode = |v: &str| {
            Config::parse(&format!("[workspaces]\nmode = \"{v}\"\n")).map(|c| c.workspaces.mode)
        };
        assert_eq!(mode("continuous").unwrap(), WorkspaceMode::Continuous);
        assert_eq!(mode("individual").unwrap(), WorkspaceMode::Individual);
        assert!(mode("spiral").is_err());
        assert_eq!(Config::default().workspaces.mode, WorkspaceMode::Continuous);
    }

    #[test]
    fn shortcuts_merge_over_defaults() {
        let c = Config::parse(
            "[shortcuts]\nterminal = \"Ctrl+T\"\nlock = \"\"\nclose = [\"Super+W\"]\nbogus = \"Super+Z\"\n\
             [[shortcuts.custom]]\nkeys = \"Super+B\"\ncommand = \"firefox\"\n",
        )
        .unwrap();
        let b = &c.shortcuts.bindings;
        assert_eq!(b["terminal"], ["Ctrl+T"]);
        assert!(b["lock"].is_empty());
        assert_eq!(b["close"], ["Super+W"]);
        assert_eq!(b["files"], ["Super+E"]);
        assert!(b.contains_key("bogus"));
        assert_eq!(c.shortcuts.custom[0].keys, ["Super+B"]);
        assert_eq!(c.shortcuts.custom[0].command, "firefox");
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
    fn files_sizes_are_validated() {
        let c = Config::parse("[files]\ngrid_icon_size = 128\nlist_icon_size = 32\n").unwrap();
        assert_eq!((c.files.grid_icon_size, c.files.list_icon_size), (128, 32));
        assert!(Config::parse("[files]\ngrid_icon_size = 400\n").is_err());
        assert!(Config::parse("[files]\nlist_icon_size = 8\n").is_err());
    }

    #[test]
    fn apps_terminal_is_validated() {
        let c = Config::parse("[apps]\nterminal = \"foot.desktop\"\n").unwrap();
        assert_eq!(c.apps.terminal, "foot.desktop");
        assert_eq!(Config::default().apps.terminal, "");
        assert!(Config::parse("[apps]\nterminal = \"foot\"\n").is_err());
        assert!(Config::parse("[apps]\nterminal = \"/usr/bin/foot.desktop\"\n").is_err());
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
    fn tiling_parses_and_validates() {
        assert_eq!(Config::default().tiling.layout, WindowLayout::Floating);
        let config =
            Config::parse("[tiling]\nlayout = \"tiling\"\nmaster_ratio = 0.6\ngap = 0\n").unwrap();
        assert_eq!(config.tiling.layout, WindowLayout::Tiling);
        assert_eq!(config.tiling.master_ratio, 0.6);
        assert_eq!(config.tiling.gap, 0);
        assert!(Config::parse("[tiling]\nmaster_ratio = 0.95\n").is_err());
        assert_eq!(config.tiling.style, TilingStyle::Dwindle);
        let master = Config::parse("[tiling]\nstyle = \"master\"\n").unwrap();
        assert_eq!(master.tiling.style, TilingStyle::Master);
        assert!(Config::parse("[tiling]\ngap = 100\n").is_err());
        assert!(Config::parse("[tiling]\nlayout = \"spiral\"\n").is_err());
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
        assert_eq!(output.rotation, Rotation::Normal);
        let config =
            Config::parse("[[output]]\nname = \"DP-2\"\nrotation = \"upside-down\"\n").unwrap();
        assert_eq!(config.output("DP-2").unwrap().rotation, Rotation::UpsideDown);
        assert!(Config::parse("[[output]]\nname = \"DP-2\"\nrotation = \"90\"\n").is_err());
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
