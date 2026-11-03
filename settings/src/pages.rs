//! What each page shows: groups of rows, each row one config key and the
//! control that edits it. Pages are rebuilt from the current config every
//! paint, so they always show what the file says.

use cnde_config::{Config, ConfigFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Appearance,
    Displays,
    Wifi,
    Bluetooth,
    Sound,
    Mouse,
    Keyboard,
    Windows,
    Panel,
    Dock,
    Power,
    DefaultApps,
    Accessibility,
}

impl Page {
    /// In sidebar order: each section's pages run together.
    pub const ALL: [Page; 13] = [
        Page::Appearance,
        Page::Panel,
        Page::Dock,
        Page::Windows,
        Page::Wifi,
        Page::Bluetooth,
        Page::Displays,
        Page::Sound,
        Page::Mouse,
        Page::Keyboard,
        Page::Power,
        Page::DefaultApps,
        Page::Accessibility,
    ];

    /// The sidebar section this page sits under.
    pub fn section(self) -> &'static str {
        match self {
            Page::Appearance | Page::Panel | Page::Dock | Page::Windows => "Personalize",
            Page::Wifi | Page::Bluetooth => "Connections",
            Page::Displays | Page::Sound | Page::Mouse | Page::Keyboard | Page::Power => "Devices",
            Page::DefaultApps | Page::Accessibility => "System",
        }
    }

    /// The pages the sidebar lists: the top bar and dock pages go when
    /// another shell stands in for cnde's.
    pub fn visible(c: &Config) -> Vec<Page> {
        Page::ALL
            .into_iter()
            .filter(|p| !matches!(p, Page::Panel | Page::Dock) || !other_shell(c))
            .collect()
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Displays => "Displays",
            Page::Wifi => "Wi-Fi",
            Page::Bluetooth => "Bluetooth",
            Page::Sound => "Sound",
            Page::Mouse if has_touchpad() => "Mouse & Touchpad",
            Page::Mouse => "Mouse",
            Page::Keyboard => "Keyboard",
            Page::Windows => "Windows",
            Page::Panel => "Top Bar",
            Page::Dock => "Dock",
            Page::Power => "Power",
            Page::DefaultApps => "Default Apps",
            Page::Accessibility => "Accessibility",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Appearance => "preferences-desktop-appearance-symbolic",
            Page::Displays => "video-display-symbolic",
            Page::Wifi => "network-wireless-symbolic",
            Page::Bluetooth => "bluetooth-active-symbolic",
            Page::Sound => "audio-volume-high-symbolic",
            Page::Mouse => "input-mouse-symbolic",
            Page::Keyboard => "input-keyboard-symbolic",
            Page::Windows => "window-restore-symbolic",
            Page::Panel => "focus-top-bar-symbolic",
            Page::Dock => "view-app-grid-symbolic",
            Page::Power => "battery-symbolic",
            Page::DefaultApps => "applications-system-symbolic",
            Page::Accessibility => "preferences-desktop-accessibility-symbolic",
        }
    }

    /// By name, for `cnde-settings <page>`.
    pub fn from_name(name: &str) -> Option<Page> {
        let name = name.to_ascii_lowercase();
        if matches!(name.as_str(), "wifi" | "network" | "networks") {
            return Some(Page::Wifi);
        }
        Page::ALL.into_iter().find(|p| {
            let title = p.title().to_ascii_lowercase();
            title == name || title.split_whitespace().next() == Some(name.as_str())
        })
    }
}

/// Another program (or none) is set to run as the shell, so the top bar and
/// dock aren't cnde's.
pub fn other_shell(c: &Config) -> bool {
    c.autostart.shell != "cnde-shell"
}

/// Whether the top bar settings apply: the bar is on and cnde's shell runs it.
fn bar_shown(c: &Config) -> bool {
    c.panel.enabled && !other_shell(c)
}

/// Whether the dock settings apply.
fn dock_shown(c: &Config) -> bool {
    c.dock.position != cnde_config::DockPosition::Off && !other_shell(c)
}

/// A value as written to the file. `Unset` removes the key, so the default
/// applies.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A monitor's position: `{ x, y }`.
    Point(i64, i64),
    Unset,
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            _ => "",
        }
    }
}

/// A monitor turned a quarter further, clockwise or not: the `rotation`
/// value to write (unset for upright).
pub fn turned(now: cnde_config::Rotation, clockwise: bool) -> Value {
    use cnde_config::Rotation::*;
    let next = match (now, clockwise) {
        (Normal, true) | (UpsideDown, false) => Right,
        (Right, true) | (Left, false) => UpsideDown,
        (UpsideDown, true) | (Normal, false) => Left,
        (Left, true) | (Right, false) => Normal,
    };
    match next {
        Normal => Value::Unset,
        Right => s("right"),
        UpsideDown => s("upside-down"),
        Left => s("left"),
    }
}

fn s(text: &str) -> Value {
    Value::Str(text.into())
}

/// Where a row's value lives in config.toml.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// A dotted key, like "input.mouse.speed".
    Path(&'static str),
    /// A key in the `[[output]]` entry for one monitor.
    Output { name: String, field: &'static str },
    /// One coordinate of a monitor's position (`x` or `y`).
    OutputPosition { name: String, axis: char },
    /// A monitor's resolution (`"2560x1440"`), or its refresh rate too
    /// (`"2560x1440@179.96"`); both rows write `mode`.
    OutputMode { name: String, refresh: bool },
    /// The arrangement canvas; dragging writes each monitor's `position`.
    Arrangement,
    /// Which monitor has `main = true`: a name, or `Unset` for automatic.
    MainDisplay,
    /// Not in the config: Wi-Fi, done through NetworkManager or iwd.
    Wifi(WifiKey),
    /// Not in the config: Bluetooth, done through BlueZ.
    Bluetooth(BluetoothKey),
    /// Not in the config: an app's volume, mute or output, through `pactl`.
    /// The Sound page's mixer (it handles its own pointer input).
    Mixer,
    /// Not in the config: the panel's backlight, set directly.
    Brightness,
    /// Not in the config: the power mode, set through the power daemon.
    PowerMode,
    /// Not in the config: the default app for a category (by id), in
    /// `mimeapps.list`.
    DefaultApp(&'static str),
    /// `shortcuts.<action>`: its combos, comma-separated in the value.
    Shortcut(&'static str),
    /// `[[shortcuts.custom]]` number n: its combos, comma-separated like the
    /// built-ins. Unset removes the entry.
    CustomShortcut(usize),
    /// `[[shortcuts.custom]]` number n: its command.
    CustomCommand(usize),
    /// Not in the config: a new custom shortcut running the command typed.
    NewCustomShortcut,
    /// Not in the config: the sidebar's search field.
    Search,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BluetoothKey {
    /// The adapter on or off.
    Powered,
    /// A device (by address): its Connect, Disconnect, Forget or Pair
    /// buttons.
    Device(String),
    /// The code to type on the device being paired.
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WifiKey {
    /// The radio on or off.
    Enabled,
    /// A network in the list (clicking it connects).
    Network(String),
    /// A saved network (clicking it opens its details).
    Saved(String),
    /// Join a saved network by itself.
    AutoConnect(String),
    /// Disconnect from or forget a network.
    Actions(String),
    /// A read-only line about the connection.
    Detail(String),
    /// "Join Hidden Network…".
    Hidden,
    /// A field of the network being joined.
    Join(JoinField),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinField {
    Ssid,
    Security,
    Password,
    Eap,
    Phase2,
    Identity,
    AnonymousIdentity,
    CaCert,
    Domain,
    ClientCert,
    PrivateKey,
    /// Cancel or Connect.
    Submit,
}

impl Key {
    /// Write `value` into `file`; `config` is what the file said before.
    pub fn write(&self, file: &mut ConfigFile, config: &Config, value: &Value) {
        let toml = |value: &Value| -> Option<cnde_config::toml_edit::Value> {
            Some(match value {
                Value::Bool(v) => (*v).into(),
                Value::Int(v) => (*v).into(),
                // Two decimals: slider steps shouldn't write 0.30000000000000004.
                Value::Float(v) => ((v * 100.0).round() / 100.0).into(),
                Value::Str(v) => v.as_str().into(),
                Value::Point(x, y) => {
                    let mut table = cnde_config::toml_edit::InlineTable::new();
                    table.insert("x", (*x).into());
                    table.insert("y", (*y).into());
                    table.into()
                }
                Value::Unset => return None,
            })
        };
        match self {
            Key::Path(path) => match toml(value) {
                Some(v) => file.set(path, v),
                None => file.remove(path),
            },
            Key::MainDisplay => {
                for output in &config.outputs {
                    if output.main {
                        file.remove_output(&output.name, "main");
                    }
                }
                if let Value::Str(name) = value {
                    file.set_output(name, "main", true);
                }
            }
            Key::Output { name, field } => match toml(value) {
                Some(v) => file.set_output(name, field, v),
                None => file.remove_output(name, field),
            },
            Key::Arrangement => {}
            Key::Shortcut(name) => {
                let path = format!("shortcuts.{name}");
                if *value == Value::Unset {
                    file.remove(&path);
                } else {
                    let mut keys = cnde_config::toml_edit::Array::new();
                    for key in value.as_str().split(", ").filter(|k| !k.is_empty()) {
                        keys.push(key);
                    }
                    file.set(&path, keys);
                }
            }
            Key::CustomShortcut(index) => {
                if *value == Value::Unset {
                    file.remove_custom_shortcut(*index);
                } else {
                    let keys: Vec<String> = value
                        .as_str()
                        .split(", ")
                        .filter(|k| !k.is_empty())
                        .map(str::to_string)
                        .collect();
                    let command = config
                        .shortcuts
                        .custom
                        .get(*index)
                        .map_or_else(String::new, |c| c.command.clone());
                    file.set_custom_shortcut(*index, &keys, &command);
                }
            }
            Key::CustomCommand(index) => {
                // A shortcut with no command does nothing, so none is saved.
                let command = value.as_str();
                if let Some(custom) = config.shortcuts.custom.get(*index)
                    && !command.is_empty()
                {
                    file.set_custom_shortcut(*index, &custom.keys, command);
                }
            }
            Key::NewCustomShortcut => {
                let command = value.as_str();
                if !command.is_empty() {
                    file.add_custom_shortcut(command);
                }
            }
            Key::Search => {}
            Key::OutputMode { name, .. } => {
                let parsed = value.as_str().split_once('x').and_then(|(w, rest)| {
                    let (h, hz) = match rest.split_once('@') {
                        Some((h, hz)) => (h, hz.parse::<f64>().ok()),
                        None => (rest, None),
                    };
                    Some((w.parse::<i64>().ok()?, h.parse::<i64>().ok()?, hz))
                });
                let Some((w, h, hz)) = parsed else {
                    file.remove_output(name, "mode");
                    return;
                };
                let mut table = cnde_config::toml_edit::InlineTable::new();
                table.insert("width", w.into());
                table.insert("height", h.into());
                if let Some(hz) = hz {
                    table.insert("refresh", ((hz * 100.0).round() / 100.0).into());
                }
                file.set_output(name, "mode", table);
            }
            // Not the config's business: the window sends these to the
            // Wi-Fi and Bluetooth workers instead.
            Key::Wifi(_)
            | Key::Bluetooth(_)
            | Key::Mixer
            | Key::Brightness
            | Key::PowerMode
            | Key::DefaultApp(_) => {}
            Key::OutputPosition { name, axis } => {
                let old = config.output(name).and_then(|o| o.position);
                let Some(v) = value.as_f64() else {
                    file.remove_output(name, "position");
                    return;
                };
                let (mut x, mut y) = old.map_or((0, 0), |p| (p.x as i64, p.y as i64));
                if *axis == 'x' {
                    x = v as i64;
                } else {
                    y = v as i64;
                }
                let mut table = cnde_config::toml_edit::InlineTable::new();
                table.insert("x", x.into());
                table.insert("y", y.into());
                file.set_output(name, "position", table);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub label: String,
    pub value: Value,
}

fn choices(list: &[(&str, Value)]) -> Vec<Choice> {
    list.iter()
        .map(|(label, value)| Choice {
            label: (*label).into(),
            value: value.clone(),
        })
        .collect()
}

#[derive(Debug, Clone)]
pub enum Control {
    /// On is any value other than `off`; switching on writes `on`.
    Switch {
        on: Value,
        off: Value,
    },
    /// Linked buttons, for two or three short choices.
    Segmented(Vec<Choice>),
    Dropdown(Vec<Choice>),
    Slider {
        min: f64,
        max: f64,
        step: f64,
        /// Writes integers.
        int: bool,
        format: fn(f64) -> String,
    },
    Text {
        placeholder: &'static str,
        /// Must parse as a whole number.
        int: bool,
    },
    /// The wallpaper picker's grid of pictures.
    Wallpapers,
    /// A Wi-Fi network: its signal (if in range), a lock if it needs a
    /// password, a check if connected, an arrow if clicking opens its
    /// details. Otherwise clicking the row joins it.
    Network {
        signal: Option<u8>,
        secured: bool,
        connected: bool,
        details: bool,
    },
    /// A text entry that shows dots.
    Password,
    /// Read-only text on the right (connection details).
    Info,
    /// The monitors as boxes to drag into place.
    Arrangement,
    /// The Sound page's devices and the apps playing to them.
    Mixer,
    /// A colour: a hex entry, and swatches to pick from below the title.
    Color {
        placeholder: &'static str,
    },
    /// A shortcut's combos; clicking captures a new first one.
    Shortcut,
}

/// The colours offered under a colour row: a light, a bright and a deep
/// shade of each hue from red round to violet, then pink and grey. The
/// default base (deep violet) and accent (light pink) are among them.
pub const SWATCHES: [[&str; 10]; 3] = [
    [
        "#f7a8a8", "#f9c49a", "#f6e3a1", "#b5e3b0", "#a6e0dc", "#a8c8f5", "#b8b5f2", "#d4b3f0",
        "#f4a7c6", "#d6d3dc",
    ],
    [
        "#e5484d", "#f08a3c", "#e8c547", "#5bb974", "#2fb3a6", "#3d8bfd", "#6366f1", "#9b5de5",
        "#e0569b", "#8e8a96",
    ],
    [
        "#5c1a22", "#6b3414", "#5e4a12", "#1d4a2c", "#134a46", "#1a3260", "#2a2a66", "#2b1a40",
        "#5a1f40", "#2b2930",
    ],
];

impl Control {
    fn switch() -> Self {
        Control::Switch {
            on: Value::Bool(true),
            off: Value::Bool(false),
        }
    }

    fn text(placeholder: &'static str) -> Self {
        Control::Text {
            placeholder,
            int: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub key: Key,
    pub title: String,
    pub subtitle: String,
    pub control: Control,
    pub value: Value,
    /// What resetting writes back to; a reset button shows when the value
    /// differs.
    pub default: Value,
    /// Hidden until "Advanced settings" is on.
    pub advanced: bool,
    /// Greyed out and not clickable (the subtitle says why).
    pub disabled: bool,
}

impl Row {
    fn new(key: &'static str, title: &str, control: Control, value: Value, default: Value) -> Self {
        Self {
            key: Key::Path(key),
            title: title.into(),
            subtitle: String::new(),
            control,
            value,
            default,
            advanced: false,
            disabled: false,
        }
    }

    fn sub(mut self, subtitle: &str) -> Self {
        self.subtitle = subtitle.into();
        self
    }

    fn advanced(mut self) -> Self {
        self.advanced = true;
        self
    }

    pub fn is_on(&self) -> bool {
        match &self.control {
            Control::Switch { off, .. } => self.value != *off,
            _ => false,
        }
    }

    pub fn changed(&self) -> bool {
        !matches!(self.control, Control::Wallpapers) && self.value != self.default
    }
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub title: String,
    pub description: String,
    pub rows: Vec<Row>,
}

fn group(title: &str, rows: Vec<Row>) -> Group {
    Group {
        title: title.into(),
        description: String::new(),
        rows,
    }
}

/// A monitor, as the compositor describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub name: String,
    pub description: String,
    /// Where it is in the global layout, logical pixels.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Whether the monitor can do variable refresh.
    pub vrr_capable: bool,
    /// What it offers: width, height, refresh in mHz.
    pub modes: Vec<(i32, i32, i32)>,
    pub current: Option<(i32, i32, i32)>,
    pub preferred: Option<(i32, i32, i32)>,
}

/// "179.96" or "60".
fn hz(mhz: i32) -> String {
    let text = format!("{:.2}", mhz as f64 / 1000.0);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A display's resolution and refresh rate rows.
fn mode_rows(display: &Display, config: Option<cnde_config::ModeConfig>) -> Vec<Row> {
    let name = &display.name;
    let mut sizes: Vec<(i32, i32)> = Vec::new();
    for &(w, h, _) in &display.modes {
        if !sizes.contains(&(w, h)) {
            sizes.push((w, h));
        }
    }
    sizes.sort_by_key(|&(w, h)| std::cmp::Reverse((w as i64 * h as i64, w)));
    sizes.truncate(14);
    let size_value = |(w, h): (i32, i32)| Value::Str(format!("{w}x{h}"));
    let auto_label = match display.preferred {
        Some((w, h, _)) => format!("Automatic ({w} × {h})"),
        None => "Automatic".into(),
    };
    let mut resolutions = vec![Choice {
        label: auto_label,
        value: Value::Unset,
    }];
    resolutions.extend(sizes.iter().map(|&(w, h)| Choice {
        label: format!("{w} × {h}"),
        value: size_value((w, h)),
    }));
    let resolution = config.map_or(Value::Unset, |m| {
        size_value((m.width as i32, m.height as i32))
    });

    // Rates at the size it's running (or set to).
    let size = config
        .map(|m| (m.width as i32, m.height as i32))
        .or(display.current.map(|(w, h, _)| (w, h)));
    let mut rates: Vec<i32> = display
        .modes
        .iter()
        .filter(|(w, h, _)| Some((*w, *h)) == size)
        .map(|m| m.2)
        .collect();
    rates.sort_by_key(|r| std::cmp::Reverse(*r));
    rates.dedup_by(|a, b| hz(*a) == hz(*b));
    let fastest = config.map_or(Value::Unset, |m| {
        Value::Str(format!("{}x{}", m.width, m.height))
    });
    let mut refreshes = vec![Choice {
        label: match rates.first() {
            Some(&r) => format!("Fastest ({} Hz)", hz(r)),
            None => "Fastest".into(),
        },
        value: fastest.clone(),
    }];
    if let Some((w, h)) = size {
        refreshes.extend(rates.iter().map(|&r| Choice {
            label: format!("{} Hz", hz(r)),
            value: Value::Str(format!("{w}x{h}@{}", hz(r))),
        }));
    }
    let refresh = match config {
        Some(m) if m.refresh.is_some() => Value::Str(format!(
            "{}x{}@{}",
            m.width,
            m.height,
            hz((m.refresh.unwrap_or(0.0) * 1000.0).round() as i32)
        )),
        _ => fastest.clone(),
    };
    vec![
        Row {
            key: Key::OutputMode {
                name: name.clone(),
                refresh: false,
            },
            title: "Resolution".into(),
            subtitle: String::new(),
            control: Control::Dropdown(resolutions),
            value: resolution,
            default: Value::Unset,
            advanced: false,
            disabled: false,
        },
        Row {
            key: Key::OutputMode {
                name: name.clone(),
                refresh: true,
            },
            title: "Refresh rate".into(),
            subtitle: String::new(),
            control: Control::Dropdown(refreshes),
            value: refresh,
            default: fastest,
            advanced: false,
            disabled: false,
        },
    ]
}

/// What the Wi-Fi page shows: the worker's last report, the network
/// being joined (its form shows under it), and the connected network whose
/// details are open instead of the list.
#[derive(Clone, Copy, Default)]
pub struct WifiView<'a> {
    pub state: Option<&'a crate::wifi::State>,
    pub join: Option<&'a crate::wifi::Join>,
    pub details: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub fn build(
    page: Page,
    c: &Config,
    displays: &[Display],
    wifi: WifiView<'_>,
    bluetooth: Option<&crate::bluetooth::State>,
    brightness: Option<u32>,
    power_mode: Option<&crate::power_mode::State>,
    default_apps: Option<&crate::defaults::State>,
    sound: Option<&crate::sound::State>,
) -> Vec<Group> {
    let d = Config::default();
    match page {
        Page::Appearance => appearance(c, &d),
        Page::Displays => displays_page(c, displays, brightness),
        Page::Wifi => wifi_page(wifi),
        Page::Bluetooth => bluetooth_page(bluetooth),
        Page::Sound => sound_page(c, &d, sound),
        Page::Mouse => mouse(c, &d),
        Page::Keyboard => keyboard(c, &d),
        Page::Windows => windows(c, &d),
        Page::Panel => panel(c, &d),
        Page::Dock => dock(c, &d),
        Page::Power => power(c, &d, power_mode),
        Page::DefaultApps => default_apps_page(c, &d, default_apps),
        Page::Accessibility => accessibility(c, &d),
    }
}

/// The groups of `page` as the window has them now.
pub fn build_for(ui: &crate::view::Ui, page: Page) -> Vec<Group> {
    build(
        page,
        &ui.config,
        &ui.displays,
        WifiView {
            state: ui.wifi.as_ref(),
            join: ui.wifi_join.as_ref(),
            details: ui.sub_page(),
        },
        ui.bluetooth.as_ref(),
        ui.brightness,
        ui.power_mode.as_ref(),
        ui.default_apps.as_ref(),
        ui.sound.as_ref(),
    )
}

/// A row that isn't a config key (its value is also its default, so no
/// reset button).
fn wifi_row(key: WifiKey, title: &str, control: Control, value: Value) -> Row {
    Row {
        key: Key::Wifi(key),
        title: title.into(),
        subtitle: String::new(),
        control,
        default: value.clone(),
        value,
        advanced: false,
        disabled: false,
    }
}

fn wifi_page(view: WifiView<'_>) -> Vec<Group> {
    use crate::wifi::Backend;
    let Some(state) = view.state else {
        return vec![Group {
            title: "Wi-Fi".into(),
            description: "Looking for Wi-Fi…".into(),
            rows: Vec::new(),
        }];
    };
    match state.backend {
        Backend::Unknown => {
            return vec![Group {
                title: "Wi-Fi".into(),
                description: "Looking for Wi-Fi…".into(),
                rows: Vec::new(),
            }];
        }
        Backend::None => {
            return vec![Group {
                title: "Wi-Fi".into(),
                description: "Wi-Fi is managed by NetworkManager or iwd, and neither is running."
                    .into(),
                rows: Vec::new(),
            }];
        }
        _ => {}
    }
    if !state.has_device {
        return vec![Group {
            title: "Wi-Fi".into(),
            description: "No Wi-Fi hardware found.".into(),
            rows: Vec::new(),
        }];
    }
    let top = Group {
        title: "Wi-Fi".into(),
        description: state.error.clone().unwrap_or_default(),
        rows: vec![wifi_row(
            WifiKey::Enabled,
            "Wi-Fi",
            Control::switch(),
            Value::Bool(state.enabled),
        )],
    };
    if !state.enabled {
        return vec![top];
    }
    if let Some(network) = view.details.and_then(|ssid| {
        state
            .networks
            .iter()
            .find(|n| (n.connected || n.known) && n.ssid == ssid)
    }) {
        return details_page(network, state.details.as_ref());
    }
    let mut groups = vec![top];
    let join = view.join;
    let joining = |ssid: &str| join.is_some_and(|j| !j.hidden && j.ssid == ssid);

    let network_row = |network: &crate::wifi::Network, key: WifiKey| {
        let connecting = state.connecting.as_deref() == Some(network.ssid.as_str());
        let saved_list = matches!(key, WifiKey::Saved(_));
        let status = if network.connected {
            "Connected"
        } else if connecting {
            "Connecting…"
        } else if !network.in_range {
            "Not in range"
        } else if network.known && !saved_list {
            "Saved"
        } else {
            ""
        };
        let mut row = wifi_row(
            key,
            &network.ssid,
            Control::Network {
                signal: network.in_range.then_some(network.signal),
                secured: network.security.secured(),
                connected: network.connected,
                details: network.connected || saved_list,
            },
            Value::Unset,
        );
        row.subtitle = match status {
            "" => network.security.label().to_string(),
            status => format!("{status} · {}", network.security.label()),
        };
        row
    };

    // Clicking it opens its details.
    if let Some(network) = state.networks.iter().find(|n| n.connected) {
        let key = WifiKey::Network(network.ssid.clone());
        groups.push(group("Connected", vec![network_row(network, key)]));
    }

    let mut rows = Vec::new();
    for network in state.networks.iter().filter(|n| !n.connected && n.in_range) {
        rows.push(network_row(network, WifiKey::Network(network.ssid.clone())));
        if joining(&network.ssid)
            && let Some(join) = join
        {
            rows.extend(join_rows(join));
        }
    }
    let description = if rows.is_empty() {
        "No other networks found yet."
    } else {
        ""
    };
    let mut hidden = wifi_row(
        WifiKey::Hidden,
        "Hidden network",
        Control::Segmented(choices(&[("Join…", s("join"))])),
        Value::Unset,
    );
    hidden.subtitle = "One that doesn't show its name".into();
    rows.push(hidden);
    if let Some(join) = join.filter(|j| j.hidden) {
        rows.extend(join_rows(join));
    }
    groups.push(Group {
        title: "Networks".into(),
        description: description.into(),
        rows,
    });

    // Clicking one opens its details, to change or forget it without
    // connecting.
    let saved: Vec<Row> = state
        .networks
        .iter()
        .filter(|n| n.known && !n.connected)
        .map(|n| network_row(n, WifiKey::Saved(n.ssid.clone())))
        .collect();
    if !saved.is_empty() {
        groups.push(group("Saved Networks", saved));
    }
    groups
}

/// A network's own page. The connected one: how the connection's doing,
/// and disconnecting or forgetting it; a saved one: connecting or
/// forgetting it. Either can be set to join by itself.
fn details_page(
    network: &crate::wifi::Network,
    details: Option<&crate::wifi::Details>,
) -> Vec<Group> {
    let mut rows = Vec::new();
    if network.connected {
        if let Some(details) = details {
            rows.extend(detail_rows(network, details));
        }
    } else {
        let info = |title: &str, text: &str| {
            wifi_row(
                WifiKey::Detail(title.to_string()),
                title,
                Control::Info,
                s(text),
            )
        };
        rows.push(info(
            "Status",
            if network.in_range {
                "In range"
            } else {
                "Not in range"
            },
        ));
        rows.push(info("Security", network.security.label()));
    }
    if network.known {
        let mut auto = wifi_row(
            WifiKey::AutoConnect(network.ssid.clone()),
            "Connect automatically",
            Control::switch(),
            Value::Bool(network.autoconnect),
        );
        auto.subtitle = "Join it whenever it's in range".into();
        rows.push(auto);
    }
    let actions: &[(&str, Value)] = if network.connected {
        &[("Disconnect", s("disconnect")), ("Forget", s("forget"))]
    } else if network.in_range {
        &[("Connect", s("connect")), ("Forget", s("forget"))]
    } else {
        &[("Forget", s("forget"))]
    };
    rows.push(wifi_row(
        WifiKey::Actions(network.ssid.clone()),
        "",
        Control::Segmented(choices(actions)),
        Value::Unset,
    ));
    let title = if network.connected {
        "Connection"
    } else {
        "Saved Network"
    };
    vec![group(title, rows)]
}

/// Read-only rows about the connection that's up.
fn detail_rows(network: &crate::wifi::Network, d: &crate::wifi::Details) -> Vec<Row> {
    let info = |title: &str, text: String| {
        wifi_row(
            WifiKey::Detail(title.to_string()),
            title,
            Control::Info,
            Value::Str(text),
        )
    };
    let quality = match network.signal {
        0..=24 => "Weak",
        25..=49 => "Fair",
        50..=74 => "Good",
        _ => "Excellent",
    };
    let mut rows = vec![info(
        "Signal",
        match d.dbm {
            Some(dbm) => format!("{quality} · {dbm} dBm"),
            None => format!("{quality} · {}%", network.signal),
        },
    )];
    let security = if d.security.is_empty() {
        network.security.label().to_string()
    } else {
        d.security.clone()
    };
    rows.push(info("Security", security));
    if let Some(frequency) = d.frequency {
        rows.push(info("Band", crate::wifi::band(frequency)));
    }
    if let Some(bitrate) = d.bitrate {
        rows.push(info("Link speed", format!("{bitrate} Mb/s")));
    }
    if !d.ipv4.is_empty() {
        rows.push(info("IPv4 address", d.ipv4.join(", ")));
    }
    if !d.ipv6.is_empty() {
        rows.push(info("IPv6 address", d.ipv6.join(", ")).advanced());
    }
    if let Some(gateway) = &d.gateway {
        rows.push(info("Gateway", gateway.clone()));
    }
    if !d.dns.is_empty() {
        rows.push(info("DNS", d.dns.join(", ")));
    }
    if let Some(mac) = &d.mac {
        rows.push(info("Hardware address", mac.clone()).advanced());
    }
    if let Some(bssid) = &d.bssid {
        rows.push(info("Access point", bssid.clone()).advanced());
    }
    if !d.interface.is_empty() {
        rows.push(info("Interface", d.interface.clone()).advanced());
    }
    rows
}

/// The form for joining a network: what its security needs.
fn join_rows(join: &crate::wifi::Join) -> Vec<Row> {
    use crate::wifi::{Eap, Phase2, Security};
    let field = |f: JoinField, title: &str, control: Control, value: &str| {
        wifi_row(WifiKey::Join(f), title, control, s(value))
    };
    let text = |placeholder| Control::Text {
        placeholder,
        int: false,
    };
    let mut rows = Vec::new();
    if join.hidden {
        rows.push(field(
            JoinField::Ssid,
            "Name",
            text("Network name"),
            &join.ssid,
        ));
        let options: Vec<(&str, Value)> = Security::CHOICES
            .iter()
            .map(|sec| (sec.label(), s(sec.name())))
            .collect();
        rows.push(field(
            JoinField::Security,
            "Security",
            Control::Dropdown(choices(&options)),
            join.security.name(),
        ));
    }
    match join.security {
        security if security.takes_password() => {
            let mut row = field(
                JoinField::Password,
                "Password",
                Control::Password,
                &join.password,
            );
            row.subtitle = if security == Security::Wep {
                "The WEP key or passphrase".into()
            } else {
                "Enter to connect".into()
            };
            rows.push(row);
        }
        Security::Enterprise => {
            let options: Vec<(&str, Value)> =
                Eap::ALL.iter().map(|e| (e.label(), s(e.name()))).collect();
            rows.push(field(
                JoinField::Eap,
                "Authentication",
                Control::Dropdown(choices(&options)),
                join.eap.name(),
            ));
            if join.eap.tunnelled() {
                let options: Vec<(&str, Value)> = Phase2::ALL
                    .iter()
                    .map(|p| (p.label(), s(p.name())))
                    .collect();
                rows.push(
                    field(
                        JoinField::Phase2,
                        "Inner authentication",
                        Control::Dropdown(choices(&options)),
                        join.phase2.name(),
                    )
                    .sub("Usually MSCHAPv2"),
                );
            }
            rows.push(field(
                JoinField::Identity,
                "Username",
                text("name@example.edu"),
                &join.identity,
            ));
            if join.eap == Eap::Tls {
                rows.push(field(
                    JoinField::ClientCert,
                    "Your certificate",
                    text("/path/to/cert.pem"),
                    &join.client_cert,
                ));
                rows.push(field(
                    JoinField::PrivateKey,
                    "Private key",
                    text("/path/to/key.pem"),
                    &join.private_key,
                ));
                rows.push(field(
                    JoinField::Password,
                    "Private key password",
                    Control::Password,
                    &join.password,
                ));
            } else {
                rows.push(field(
                    JoinField::Password,
                    "Password",
                    Control::Password,
                    &join.password,
                ));
            }
            rows.push(
                field(
                    JoinField::AnonymousIdentity,
                    "Anonymous identity",
                    text("Optional"),
                    &join.anonymous_identity,
                )
                .sub("Sent before the secure tunnel, e.g. anonymous@example.edu"),
            );
            rows.push(
                field(
                    JoinField::Domain,
                    "Domain",
                    text("Optional, e.g. example.edu"),
                    &join.domain,
                )
                .sub("Checks the server's certificate is for this domain"),
            );
            rows.push(
                field(
                    JoinField::CaCert,
                    "CA certificate",
                    text("/path/to/ca.pem"),
                    &join.ca_cert,
                )
                .sub("Empty: the system's certificates if a domain is set, else no check"),
            );
        }
        _ => {}
    }
    rows.push(field(
        JoinField::Submit,
        "",
        Control::Segmented(choices(&[
            ("Cancel", s("cancel")),
            ("Connect", s("connect")),
        ])),
        "",
    ));
    rows
}

// --- Bluetooth

fn bluetooth_row(key: BluetoothKey, title: &str, control: Control, value: Value) -> Row {
    Row {
        key: Key::Bluetooth(key),
        ..wifi_row(WifiKey::Hidden, title, control, value)
    }
}

/// The adapter's switch, paired devices with their buttons, and what's
/// nearby to pair with (looked for while the page shows).
fn bluetooth_page(state: Option<&crate::bluetooth::State>) -> Vec<Group> {
    use crate::bluetooth::{Availability, Busy};
    let note = |text: &str| {
        vec![Group {
            title: "Bluetooth".into(),
            description: text.into(),
            rows: Vec::new(),
        }]
    };
    let Some(state) = state else {
        return note("Looking for Bluetooth…");
    };
    match state.availability {
        Availability::Unknown => return note("Looking for Bluetooth…"),
        Availability::NoService => {
            return note("Bluetooth is managed by BlueZ, and it isn't running.");
        }
        Availability::NoAdapter => return note("No Bluetooth hardware found."),
        Availability::Ready => {}
    }
    let top = Group {
        title: "Bluetooth".into(),
        description: state.error.clone().unwrap_or_default(),
        rows: vec![bluetooth_row(
            BluetoothKey::Powered,
            "Bluetooth",
            Control::switch(),
            Value::Bool(state.powered),
        )],
    };
    if !state.powered {
        return vec![top];
    }
    let busy = |address: &str| {
        state
            .busy
            .as_ref()
            .filter(|(a, _)| a == address)
            .map(|(_, busy)| *busy)
    };
    let device_row = |device: &crate::bluetooth::Device| {
        let buttons: &[(&str, Value)] = match (device.paired, device.connected) {
            (false, _) => &[("Pair", s("pair"))],
            (true, true) => &[("Disconnect", s("disconnect")), ("Forget", s("forget"))],
            (true, false) => &[("Connect", s("connect")), ("Forget", s("forget"))],
        };
        let mut row = bluetooth_row(
            BluetoothKey::Device(device.address.clone()),
            &device.name,
            Control::Segmented(choices(buttons)),
            Value::Unset,
        );
        let status = match busy(&device.address) {
            Some(Busy::Pairing) => "Pairing…",
            Some(Busy::Connecting) => "Connecting…",
            Some(Busy::Disconnecting) => "Disconnecting…",
            None if device.connected => "Connected",
            None if device.paired => "Not connected",
            None => "",
        };
        let battery = device.battery.map(|b| format!("{b}% battery"));
        row.subtitle = [Some(status), Some(device.kind()), battery.as_deref()]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        row
    };

    let mut groups = vec![top];
    let paired: Vec<Row> = state
        .devices
        .iter()
        .filter(|d| d.paired)
        .map(device_row)
        .collect();
    groups.push(Group {
        title: "My Devices".into(),
        description: if paired.is_empty() {
            "Nothing paired yet.".into()
        } else {
            String::new()
        },
        rows: paired,
    });

    let mut nearby = Vec::new();
    for device in state.devices.iter().filter(|d| !d.paired) {
        nearby.push(device_row(device));
        // A keyboard shows the code to type on it under its row.
        if busy(&device.address) == Some(Busy::Pairing)
            && let Some(code) = &state.code
        {
            nearby.push(
                bluetooth_row(BluetoothKey::Code, "Code", Control::Info, s(code))
                    .sub(&format!("Type it on {}, then press Enter", device.name)),
            );
        }
    }
    let description = match (state.discovering, nearby.is_empty()) {
        (true, true) => "Looking for devices…",
        (false, true) => "No devices found.",
        _ => "Make sure the device is in pairing mode.",
    };
    groups.push(Group {
        title: "Nearby Devices".into(),
        description: description.into(),
        rows: nearby,
    });
    groups
}

// --- Sound

/// The Sound page: the mixer (each output with its apps, then the inputs),
/// and the event sounds switch.
fn sound_page(c: &Config, d: &Config, state: Option<&crate::sound::State>) -> Vec<Group> {
    use crate::sound::Availability;
    let mut mixer = Group::default();
    match state {
        Some(st) if st.availability == Availability::Ready => mixer.rows.push(Row {
            key: Key::Mixer,
            title: String::new(),
            subtitle: String::new(),
            control: Control::Mixer,
            value: Value::Unset,
            default: Value::Unset,
            advanced: false,
            disabled: false,
        }),
        Some(st) if st.availability == Availability::Unavailable => {
            mixer.description =
                "Sound needs pactl (PipeWire or PulseAudio), and it isn't answering.".into();
        }
        _ => mixer.description = "Looking for the audio server…".into(),
    }
    vec![
        mixer,
        group(
            "System sounds",
            vec![
                Row::new(
                    "sound.event_sounds",
                    "Event sounds",
                    Control::switch(),
                    Value::Bool(c.sound.event_sounds),
                    Value::Bool(d.sound.event_sounds),
                )
                .sub("Volume keys, screenshots, low battery, plugging in"),
            ],
        ),
    ]
}

fn speed(v: f64) -> String {
    if v.abs() < 0.005 {
        "Default".into()
    } else {
        format!("{v:+.2}")
    }
}

fn appearance(c: &Config, d: &Config) -> Vec<Group> {
    use cnde_config::{ColorScheme, WallpaperFit};
    let scheme = |v: ColorScheme| {
        s(match v {
            ColorScheme::Dark => "dark",
            ColorScheme::Light => "light",
        })
    };
    let palette = |v: cnde_config::Palette| {
        s(match v {
            cnde_config::Palette::Quiet => "quiet",
            cnde_config::Palette::Cnde => "cnde",
            cnde_config::Palette::Adwaita => "adwaita",
            cnde_config::Palette::Custom => "custom",
            cnde_config::Palette::Adaptive => "adaptive",
        })
    };
    // The old `adaptive_color = true` means the adaptive palette.
    let shown_palette = |a: &cnde_config::Appearance| {
        if a.adaptive() {
            cnde_config::Palette::Adaptive
        } else {
            a.palette
        }
    };
    let fit = |v: WallpaperFit| {
        s(match v {
            WallpaperFit::Fill => "fill",
            WallpaperFit::Fit => "fit",
            WallpaperFit::Stretch => "stretch",
        })
    };
    let mut groups = vec![
        group(
            "Colour & material",
            vec![
                Row::new(
                    "appearance.color_scheme",
                    "Style",
                    Control::Segmented(choices(&[("Dark", s("dark")), ("Light", s("light"))])),
                    scheme(c.appearance.color_scheme),
                    scheme(d.appearance.color_scheme),
                )
                .sub(match c.dark_mode.schedule {
                    cnde_config::DarkSchedule::Off => {
                        "Menus, quick settings and this window; also picks the wallpaper"
                    }
                    _ => "Switches by itself on the schedule below",
                }),
                Row::new(
                    "appearance.palette",
                    "Colours",
                    Control::Segmented(choices(&[
                        ("Quiet", s("quiet")),
                        ("Dusk", s("cnde")),
                        ("GNOME", s("adwaita")),
                        ("Custom", s("custom")),
                        ("Adaptive", s("adaptive")),
                    ])),
                    palette(shown_palette(&c.appearance)),
                    palette(d.appearance.palette),
                )
                .sub(match shown_palette(&c.appearance) {
                    cnde_config::Palette::Custom => "Worked out from the two colours below",
                    cnde_config::Palette::Quiet => "Warm greys with an amber accent",
                    cnde_config::Palette::Cnde => "Plum and pink when dark, blush and rose when light",
                    cnde_config::Palette::Adwaita => "GNOME's greys and blue",
                    cnde_config::Palette::Adaptive => {
                        "From the wallpaper; the bars match windows"
                    }
                }),
                Row::new(
                    "appearance.corner_radius",
                    "Corner radius",
                    Control::Slider {
                        min: 0.0,
                        max: 32.0,
                        step: 1.0,
                        int: true,
                        format: |v| format!("{v:.0} px"),
                    },
                    Value::Int(c.appearance.corner_radius as i64),
                    Value::Int(d.appearance.corner_radius as i64),
                )
                .sub("Windows; the dock, top bar and menus round in step"),
                Row::new(
                    "appearance.material",
                    "Material",
                    Control::Segmented(choices(&[
                        ("Frosted", s("frosted")),
                        ("Solid", s("solid")),
                    ])),
                    material(c.appearance.material),
                    material(d.appearance.material),
                )
                .sub("The top bar, dock, menus and this window; solid over a window"),
            ],
        ),
        group(
            "Title bars",
            vec![
                Row::new(
                    "appearance.titlebars",
                    "Matching title bars",
                    Control::Switch {
                        on: s("cnde"),
                        off: s("app"),
                    },
                    s(match c.appearance.titlebars {
                        cnde_config::Titlebars::Cnde => "cnde",
                        cnde_config::Titlebars::App => "app",
                    }),
                    s(match d.appearance.titlebars {
                        cnde_config::Titlebars::Cnde => "cnde",
                        cnde_config::Titlebars::App => "app",
                    }),
                )
                .sub("Draw title bars for apps that would otherwise use their toolkit's plain one"),
                Row::new(
                    "appearance.window_buttons",
                    "Title bar buttons",
                    Control::Segmented(choices(&[
                        ("Minimize, Maximize, Close", s("all")),
                        ("Close", s("close")),
                    ])),
                    s(match c.appearance.window_buttons {
                        cnde_config::WindowButtons::All => "all",
                        cnde_config::WindowButtons::Close => "close",
                    }),
                    s(match d.appearance.window_buttons {
                        cnde_config::WindowButtons::All => "all",
                        cnde_config::WindowButtons::Close => "close",
                    }),
                ),
            ],
        ),
        group(
            "Background",
            vec![
                Row::new(
                    "wallpaper.light",
                    "",
                    Control::Wallpapers,
                    s(&c.wallpaper.light),
                    s(""),
                ),
                Row::new(
                    "wallpaper.fit",
                    "Fit",
                    Control::Segmented(choices(&[
                        ("Fill", s("fill")),
                        ("Fit", s("fit")),
                        ("Stretch", s("stretch")),
                    ])),
                    fit(c.wallpaper.fit),
                    fit(d.wallpaper.fit),
                )
                .sub("Fill crops to cover the screen; Fit shows the whole picture"),
                Row::new(
                    "wallpaper.light",
                    "Picture",
                    Control::text("Built-in gradient"),
                    s(&c.wallpaper.light),
                    s(""),
                )
                .sub("Any PNG, JPEG or WebP file")
                .advanced(),
                Row::new(
                    "wallpaper.dark",
                    "Dark style picture",
                    Control::text("Same as above"),
                    s(&c.wallpaper.dark),
                    s(""),
                )
                .advanced(),
            ],
        ),
    ];
    groups.insert(1, dark_schedule(c, d));
    if let Some(look) = groups.iter_mut().find(|g| g.title == "Colour & material") {
        if bar_shown(c) {
            look.rows.extend(panel_look(c, d));
        }
        if dock_shown(c) {
            look.rows.extend(dock_look(c, d));
        }
    }
    if other_shell(c) {
        groups.insert(
            0,
            Group {
                title: String::new(),
                description:
                    "Top bar and dock settings are hidden because another shell is set up.".into(),
                rows: Vec::new(),
            },
        );
    }
    // Custom colours: the two the palette is worked out from.
    if c.appearance.palette == cnde_config::Palette::Custom
        && let Some(style) = groups.iter_mut().find(|g| g.title == "Colour & material")
    {
        style.rows.insert(
            2,
            Row::new(
                "appearance.base_color",
                "Base colour",
                Control::Color {
                    placeholder: "#2b1a40",
                },
                s(&c.appearance.base_color),
                s(&d.appearance.base_color),
            )
            .sub("Backgrounds and bars take its hue"),
        );
        style.rows.insert(
            3,
            Row::new(
                "appearance.accent_color",
                "Accent colour",
                Control::Color {
                    placeholder: "#f4a7c6",
                },
                s(&c.appearance.accent_color),
                s(&d.appearance.accent_color),
            )
            .sub("Switches, selections and sliders; Auto follows the wallpaper"),
        );
    }
    groups
}

/// Switching to the dark style and back by the time of day.
fn dark_schedule(c: &Config, d: &Config) -> Group {
    use cnde_config::DarkSchedule;
    let schedule = |v: DarkSchedule| {
        s(match v {
            DarkSchedule::Off => "off",
            DarkSchedule::Sunset => "sunset",
            DarkSchedule::Custom => "custom",
        })
    };
    let dark = &c.dark_mode;
    let located = c.location.coordinates().is_some();
    let subtitle = match dark.schedule {
        DarkSchedule::Off => "Only when you switch it".to_string(),
        DarkSchedule::Sunset if !located => {
            "No location set: the times below are used until you add one".to_string()
        }
        DarkSchedule::Sunset => match sun_today(c) {
            Some(text) => format!("{text}; a switch by hand lasts until the next one"),
            None => "A switch by hand lasts until the next sunrise or sunset".to_string(),
        },
        DarkSchedule::Custom => "A switch by hand lasts until the next scheduled one".to_string(),
    };
    let mut rows = vec![
        Row::new(
            "dark_mode.schedule",
            "Switch automatically",
            Control::Segmented(choices(&[
                ("Off", s("off")),
                ("Sunset to sunrise", s("sunset")),
                ("Custom", s("custom")),
            ])),
            schedule(dark.schedule),
            schedule(d.dark_mode.schedule),
        )
        .sub(&subtitle),
    ];
    let times = match dark.schedule {
        DarkSchedule::Off => false,
        DarkSchedule::Sunset => !located,
        DarkSchedule::Custom => true,
    };
    if times {
        rows.push(
            Row::new(
                "dark_mode.from",
                "Dark from",
                Control::text("19:00"),
                s(&dark.from),
                s(&d.dark_mode.from),
            )
            .sub("24-hour time, like 19:30"),
        );
        rows.push(Row::new(
            "dark_mode.to",
            "Light from",
            Control::text("07:00"),
            s(&dark.to),
            s(&d.dark_mode.to),
        ));
    }
    if dark.schedule == DarkSchedule::Sunset {
        rows.push(location_row(c));
    }
    group("Dark style schedule", rows)
}

/// Where sunset and sunrise are worked out for (dark style, night light).
fn location_row(c: &Config) -> Row {
    Row::new(
        "location.coordinates",
        "Location",
        Control::text("Latitude, longitude"),
        s(&c.location.coordinates),
        s(""),
    )
    .sub("In degrees, e.g. 52.52, 13.40. Worked out here: nothing goes online")
}

/// "Sunset 18:42, sunrise 07:21" today at the configured location.
fn sun_today(c: &Config) -> Option<String> {
    use cnde_config::schedule::{Clock, Sun, format_time, sun};
    let (lat, lon) = c.location.coordinates()?;
    Some(match sun(lat, lon, &Clock::now()) {
        Sun::Rises { sunrise, sunset } => format!(
            "Sunset {}, sunrise {}",
            format_time(sunset),
            format_time(sunrise)
        ),
        Sun::AlwaysUp => "The sun doesn't set today".into(),
        Sun::AlwaysDown => "The sun doesn't rise today".into(),
    })
}

fn displays_page(c: &Config, displays: &[Display], brightness: Option<u32>) -> Vec<Group> {
    if displays.is_empty() {
        return vec![Group {
            title: "No displays found".into(),
            description: "The compositor didn't list any monitors.".into(),
            rows: Vec::new(),
        }];
    }
    let mut groups = Vec::new();
    if let Some(percent) = brightness {
        groups.push(group(
            "Brightness",
            vec![Row {
                key: Key::Brightness,
                title: "Brightness".into(),
                subtitle: "The built-in screen".into(),
                control: Control::Slider {
                    min: 1.0,
                    max: 100.0,
                    step: 1.0,
                    int: true,
                    format: |v| format!("{v:.0}%"),
                },
                value: Value::Int(percent as i64),
                default: Value::Int(percent as i64),
                advanced: false,
                disabled: false,
            }],
        ));
    }
    if displays.len() > 1 {
        groups.push(Group {
            title: "Arrangement".into(),
            description: "Drag the screens to match how they sit on your desk".into(),
            rows: vec![Row {
                key: Key::Arrangement,
                title: String::new(),
                subtitle: String::new(),
                control: Control::Arrangement,
                value: Value::Unset,
                default: Value::Unset,
                advanced: false,
                disabled: false,
            }],
        });
        let mut mains = vec![Choice {
            label: "Automatic (leftmost)".into(),
            value: Value::Unset,
        }];
        mains.extend(displays.iter().map(|d| Choice {
            label: d.name.clone(),
            value: s(&d.name),
        }));
        let main = c
            .outputs
            .iter()
            .find(|o| o.main)
            .map_or(Value::Unset, |o| s(&o.name));
        groups.push(group(
            "Displays",
            vec![Row {
                key: Key::MainDisplay,
                title: "Main display".into(),
                subtitle: "Where workspaces and the top bar live".into(),
                control: Control::Dropdown(mains),
                value: main,
                default: Value::Unset,
                advanced: false,
                disabled: false,
            }],
        ));
    }
    let per_display = displays
        .iter()
        .map(|display| {
            let output = c.output(&display.name);
            let scale = output
                .and_then(|o| o.scale)
                .map_or(Value::Unset, Value::Float);
            let mut scales = choices(&[
                ("Default (100%)", Value::Unset),
                ("100%", Value::Float(1.0)),
                ("125%", Value::Float(1.25)),
                ("150%", Value::Float(1.5)),
                ("175%", Value::Float(1.75)),
                ("200%", Value::Float(2.0)),
                ("250%", Value::Float(2.5)),
                ("300%", Value::Float(3.0)),
            ]);
            if !scales.iter().any(|c| c.value == scale)
                && let Some(v) = scale.as_f64()
            {
                scales.push(Choice {
                    label: format!("{:.0}%", v * 100.0),
                    value: scale.clone(),
                });
            }
            let name = display.name.clone();
            let output_key = |field| Key::Output {
                name: name.clone(),
                field,
            };
            let position = output.and_then(|o| o.position);
            let coordinate = |axis: char| {
                let value = position.map_or(Value::Unset, |p| {
                    Value::Int(if axis == 'x' { p.x } else { p.y } as i64)
                });
                Row {
                    key: Key::OutputPosition {
                        name: name.clone(),
                        axis,
                    },
                    title: if axis == 'x' {
                        "Position, left edge".into()
                    } else {
                        "Position, top edge".into()
                    },
                    subtitle: String::new(),
                    control: Control::Text {
                        placeholder: "Automatic",
                        int: true,
                    },
                    value,
                    default: Value::Unset,
                    advanced: true,
                    disabled: false,
                }
            };
            let mut rows = mode_rows(display, output.and_then(|o| o.mode));
            rows.push(Row {
                key: output_key("scale"),
                title: "Scale".into(),
                subtitle: "Makes everything bigger on high-resolution screens".into(),
                control: Control::Dropdown(scales),
                value: scale,
                default: Value::Unset,
                advanced: false,
                disabled: false,
            });
            let vrr = match output.map(|o| o.vrr) {
                Some(cnde_config::Vrr::On) => s("on"),
                Some(cnde_config::Vrr::Fullscreen) => s("fullscreen"),
                _ => Value::Unset,
            };
            rows.push(Row {
                key: output_key("vrr"),
                title: "Variable refresh rate".into(),
                subtitle: if display.vrr_capable {
                    "FreeSync: smoother games on monitors that support it".into()
                } else {
                    "Not supported by this monitor".into()
                },
                control: Control::Dropdown(choices(&[
                    ("Off", Value::Unset),
                    ("Fullscreen only", s("fullscreen")),
                    ("Always", s("on")),
                ])),
                value: vrr,
                default: Value::Unset,
                advanced: false,
                disabled: !display.vrr_capable,
            });
            let rotation = match output.map(|o| o.rotation) {
                Some(cnde_config::Rotation::Left) => s("left"),
                Some(cnde_config::Rotation::Right) => s("right"),
                Some(cnde_config::Rotation::UpsideDown) => s("upside-down"),
                _ => Value::Unset,
            };
            rows.push(Row {
                key: output_key("rotation"),
                title: "Rotation".into(),
                subtitle: "Which way the monitor is turned; or use the arrows on it above".into(),
                control: Control::Dropdown(choices(&[
                    ("Upright", Value::Unset),
                    ("Turned clockwise", s("right")),
                    ("Upside down", s("upside-down")),
                    ("Turned anticlockwise", s("left")),
                ])),
                value: rotation,
                default: Value::Unset,
                advanced: false,
                disabled: false,
            });
            rows.push(coordinate('x'));
            rows.push(coordinate('y'));
            Group {
                title: display.name.clone(),
                description: display.description.clone(),
                rows,
            }
        })
        .collect::<Vec<_>>();
    groups.extend(per_display);
    groups.push(night_light(c, &Config::default()));
    groups
}

/// The warm colour filter and when it's on.
fn night_light(c: &Config, d: &Config) -> Group {
    use cnde_config::NightSchedule;
    let schedule = |v: NightSchedule| {
        s(match v {
            NightSchedule::Off => "off",
            NightSchedule::Always => "always",
            NightSchedule::Sunset => "sunset",
            NightSchedule::Custom => "custom",
        })
    };
    let light = &c.night_light;
    let located = c.location.coordinates().is_some();
    let subtitle = match light.schedule {
        NightSchedule::Off => "Warmer colours, less blue light".to_string(),
        NightSchedule::Always => "On all the time".to_string(),
        NightSchedule::Sunset if !located => {
            "No location set: the times below are used until you add one".to_string()
        }
        NightSchedule::Sunset => sun_today(c).unwrap_or_else(|| "From sunset to sunrise".into()),
        NightSchedule::Custom => "On between the times below".to_string(),
    };
    let mut rows = vec![
        Row::new(
            "night_light.schedule",
            "Night light",
            Control::Segmented(choices(&[
                ("Off", s("off")),
                ("Always", s("always")),
                ("Sunset to sunrise", s("sunset")),
                ("Custom", s("custom")),
            ])),
            schedule(light.schedule),
            schedule(d.night_light.schedule),
        )
        .sub(&subtitle),
    ];
    if light.schedule != NightSchedule::Off {
        rows.push(
            Row::new(
                "night_light.temperature",
                "Colour temperature",
                Control::Slider {
                    min: 2500.0,
                    max: 6500.0,
                    step: 100.0,
                    int: true,
                    format: |v| format!("{v:.0} K"),
                },
                Value::Int(light.temperature as i64),
                Value::Int(d.night_light.temperature as i64),
            )
            .sub("Lower is warmer"),
        );
        rows.push(Row::new(
            "night_light.intensity",
            "Intensity",
            Control::Slider {
                min: 0.0,
                max: 100.0,
                step: 5.0,
                int: true,
                format: |v| format!("{v:.0}%"),
            },
            Value::Int(light.intensity as i64),
            Value::Int(d.night_light.intensity as i64),
        ));
    }
    let times = match light.schedule {
        NightSchedule::Off | NightSchedule::Always => false,
        NightSchedule::Sunset => !located,
        NightSchedule::Custom => true,
    };
    if times {
        rows.push(
            Row::new(
                "night_light.from",
                "On from",
                Control::text("20:00"),
                s(&light.from),
                s(&d.night_light.from),
            )
            .sub("24-hour time, like 20:30"),
        );
        rows.push(Row::new(
            "night_light.to",
            "Off from",
            Control::text("06:00"),
            s(&light.to),
            s(&d.night_light.to),
        ));
    }
    if light.schedule == NightSchedule::Sunset {
        rows.push(location_row(c));
    }
    group("Night light", rows)
}

fn accel(v: cnde_config::AccelProfile) -> Value {
    s(match v {
        cnde_config::AccelProfile::Adaptive => "adaptive",
        cnde_config::AccelProfile::Flat => "flat",
    })
}

fn accel_row(key: &'static str, value: Value, default: Value) -> Row {
    Row::new(
        key,
        "Acceleration",
        Control::Segmented(choices(&[("Adaptive", s("adaptive")), ("Flat", s("flat"))])),
        value,
        default,
    )
    .sub("Flat moves the pointer exactly as far as you do; good for games")
    .advanced()
}

fn speed_slider() -> Control {
    Control::Slider {
        min: -1.0,
        max: 1.0,
        step: 0.05,
        int: false,
        format: speed,
    }
}

/// Whether udev's database lists a touchpad among the input devices. Read
/// once; when the database can't be read at all, assume there is one.
fn has_touchpad() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        let Ok(dir) = std::fs::read_dir("/run/udev/data") else {
            return true;
        };
        dir.flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("c13:"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .any(|db| is_touchpad(&db))
    })
}

/// Whether one udev database entry marks its device as a touchpad.
fn is_touchpad(db: &str) -> bool {
    db.lines().any(|l| l.trim() == "E:ID_INPUT_TOUCHPAD=1")
}

fn mouse(c: &Config, d: &Config) -> Vec<Group> {
    let (m, dm) = (&c.input.mouse, &d.input.mouse);
    let (t, dt) = (&c.input.touchpad, &d.input.touchpad);
    let mut groups = vec![
        group(
            "Mouse",
            vec![
                Row::new(
                    "input.mouse.speed",
                    "Pointer speed",
                    speed_slider(),
                    Value::Float(m.speed),
                    Value::Float(dm.speed),
                ),
                Row::new(
                    "input.mouse.natural_scroll",
                    "Natural scrolling",
                    Control::switch(),
                    Value::Bool(m.natural_scroll),
                    Value::Bool(dm.natural_scroll),
                )
                .sub("Scrolling moves the content, not the view"),
                accel_row(
                    "input.mouse.accel_profile",
                    accel(m.accel_profile),
                    accel(dm.accel_profile),
                ),
            ],
        ),
    ];
    if has_touchpad() {
        groups.push(group(
            "Touchpad",
            vec![
                Row::new(
                    "input.touchpad.speed",
                    "Pointer speed",
                    speed_slider(),
                    Value::Float(t.speed),
                    Value::Float(dt.speed),
                ),
                Row::new(
                    "input.touchpad.natural_scroll",
                    "Natural scrolling",
                    Control::switch(),
                    Value::Bool(t.natural_scroll),
                    Value::Bool(dt.natural_scroll),
                )
                .sub("Scrolling moves the content, not the view"),
                Row::new(
                    "input.touchpad.tap_to_click",
                    "Tap to click",
                    Control::switch(),
                    Value::Bool(t.tap_to_click),
                    Value::Bool(dt.tap_to_click),
                ),
                Row::new(
                    "input.touchpad.disable_while_typing",
                    "Disable while typing",
                    Control::switch(),
                    Value::Bool(t.disable_while_typing),
                    Value::Bool(dt.disable_while_typing),
                ),
                Row::new(
                    "input.touchpad.tap_and_drag",
                    "Tap and drag",
                    Control::switch(),
                    Value::Bool(t.tap_and_drag),
                    Value::Bool(dt.tap_and_drag),
                )
                .sub("Tap, then touch again and move to drag")
                .advanced(),
                accel_row(
                    "input.touchpad.accel_profile",
                    accel(t.accel_profile),
                    accel(dt.accel_profile),
                ),
                Row::new(
                    "input.touchpad.workspace_swipe_distance",
                    "Workspace swipe distance",
                    Control::Slider {
                        min: 300.0,
                        max: 2000.0,
                        step: 50.0,
                        int: false,
                        format: |v| format!("{v:.0}"),
                    },
                    Value::Float(t.workspace_swipe_distance),
                    Value::Float(dt.workspace_swipe_distance),
                )
                .sub("How far three fingers travel per workspace; longer feels slower")
                .advanced(),
            ],
        ));
    }
    groups
}

fn keyboard(c: &Config, d: &Config) -> Vec<Group> {
    let (k, dk) = (&c.input.keyboard, &d.input.keyboard);
    vec![
        group(
            "Layout",
            vec![
                Row::new(
                    "input.keyboard.layout",
                    "Layout",
                    Control::text("us"),
                    s(&k.layout),
                    s(&dk.layout),
                )
                .sub("XKB layout codes; several with commas, like us,de"),
                Row::new(
                    "input.keyboard.variant",
                    "Variant",
                    Control::text("None"),
                    s(&k.variant),
                    s(&dk.variant),
                )
                .sub("Like dvorak or intl")
                .advanced(),
                Row::new(
                    "input.keyboard.options",
                    "Options",
                    Control::text("None"),
                    s(&k.options),
                    s(&dk.options),
                )
                .sub("XKB options, like caps:escape")
                .advanced(),
            ],
        ),
        group(
            "Typing",
            vec![
                Row::new(
                    "input.keyboard.repeat_delay",
                    "Repeat delay",
                    Control::Slider {
                        min: 100.0,
                        max: 1000.0,
                        step: 10.0,
                        int: true,
                        format: |v| format!("{v:.0} ms"),
                    },
                    Value::Int(k.repeat_delay as i64),
                    Value::Int(dk.repeat_delay as i64),
                )
                .sub("How long a key is held before it repeats"),
                Row::new(
                    "input.keyboard.repeat_rate",
                    "Repeat speed",
                    Control::Slider {
                        min: 5.0,
                        max: 80.0,
                        step: 1.0,
                        int: true,
                        format: |v| format!("{v:.0}/s"),
                    },
                    Value::Int(k.repeat_rate as i64),
                    Value::Int(dk.repeat_rate as i64),
                ),
            ],
        ),
        group(
            "Workspaces",
            vec![
                Row::new(
                    "input.keyboard.natural_workspace_keys",
                    "Natural workspace keys",
                    Control::switch(),
                    Value::Bool(k.natural_workspace_keys),
                    Value::Bool(dk.natural_workspace_keys),
                )
                .sub("Super+Alt+Left shows the next workspace, as the content moves left"),
                Row::new(
                    "workspaces.mode",
                    "Workspaces",
                    Control::Dropdown(choices(&[
                        ("Span all monitors", s("continuous")),
                        ("Separate per monitor", s("individual")),
                    ])),
                    workspace_mode(c.workspaces.mode),
                    workspace_mode(d.workspaces.mode),
                )
                .sub("Span: monitors show neighbouring workspaces and switch together. Separate: each monitor has its own, and switching moves the one you used last"),
                Row::new(
                    "workspaces.hot_corner",
                    "Hot corner",
                    Control::switch(),
                    Value::Bool(c.workspaces.hot_corner),
                    Value::Bool(d.workspaces.hot_corner),
                )
                .sub("Pushing the pointer into the top-left corner opens the overview"),
            ],
        ),
        group(
            "Screen Lock",
            vec![
                Row::new(
                    "lock.command",
                    "Lock program",
                    Control::text("cnde-lock"),
                    s(&c.lock.command),
                    s(&d.lock.command),
                )
                .sub("Run by Super+L and the Lock button, like swaylock or gtklock")
                .advanced(),
            ],
        ),
        shortcuts_group(&c.shortcuts),
        custom_shortcuts_group(&c.shortcuts),
    ]
}

/// The built-in shortcuts as set, edited here by clicking.
fn shortcuts_group(shortcuts: &cnde_config::Shortcuts) -> Group {
    let rows: Vec<Row> = cnde_config::SHORTCUT_DEFAULTS
        .iter()
        .map(|(name, default, label)| {
            let keys = shortcuts.bindings.get(*name).map_or(&[][..], |k| k);
            let default: Vec<String> = default.iter().map(|k| k.to_string()).collect();
            // The value is the combos alone; "Off" is only how it shows.
            Row {
                key: Key::Shortcut(name),
                title: label.to_string(),
                subtitle: String::new(),
                control: Control::Shortcut,
                value: Value::Str(keys.join(", ")),
                default: Value::Str(default.join(", ")),
                advanced: false,
                disabled: false,
            }
        })
        .collect();
    Group {
        title: "Shortcuts".into(),
        description: "Click one to press a new combo; Backspace turns it off. Super is Alt when running nested.".into(),
        rows,
    }
}

/// Custom shortcuts, each running a command from its combo, and a row to
/// add one. Each has its command (a text field) and its combos (clicked to
/// capture, like the built-ins).
fn custom_shortcuts_group(shortcuts: &cnde_config::Shortcuts) -> Group {
    let mut rows = Vec::new();
    for (index, custom) in shortcuts.custom.iter().enumerate() {
        let keys = custom.keys.join(", ");
        rows.push(Row {
            key: Key::CustomCommand(index),
            title: "Command".into(),
            subtitle: String::new(),
            control: Control::Text {
                placeholder: "Command to run",
                int: false,
            },
            value: Value::Str(custom.command.clone()),
            default: Value::Str(custom.command.clone()),
            advanced: false,
            disabled: false,
        });
        rows.push(Row {
            key: Key::CustomShortcut(index),
            title: custom.command.clone(),
            subtitle: String::new(),
            control: Control::Shortcut,
            value: Value::Str(keys.clone()),
            default: Value::Str(keys),
            advanced: false,
            disabled: false,
        });
    }
    rows.push(Row {
        key: Key::NewCustomShortcut,
        title: "Add a shortcut".into(),
        subtitle: String::new(),
        control: Control::Text {
            placeholder: "Command to run",
            int: false,
        },
        value: Value::Str(String::new()),
        default: Value::Str(String::new()),
        advanced: false,
        disabled: false,
    });
    Group {
        title: "Your shortcuts".into(),
        description: "Each runs its command. Click its keys to press a new combo; Backspace removes the shortcut.".into(),
        rows,
    }
}

fn window_layout(v: cnde_config::WindowLayout) -> Value {
    s(match v {
        cnde_config::WindowLayout::Floating => "floating",
        cnde_config::WindowLayout::Tiling => "tiling",
    })
}

fn workspace_mode(v: cnde_config::WorkspaceMode) -> Value {
    s(match v {
        cnde_config::WorkspaceMode::Continuous => "continuous",
        cnde_config::WorkspaceMode::Individual => "individual",
    })
}

fn tiling_toggle(v: cnde_config::TilingToggle) -> Value {
    s(match v {
        cnde_config::TilingToggle::All => "all",
        cnde_config::TilingToggle::Workspace => "workspace",
    })
}

fn tiling_style(v: cnde_config::TilingStyle) -> Value {
    s(match v {
        cnde_config::TilingStyle::Dwindle => "dwindle",
        cnde_config::TilingStyle::Master => "master",
    })
}

fn windows(c: &Config, d: &Config) -> Vec<Group> {
    let (t, dt) = (&c.tiling, &d.tiling);
    let px = |v: f64| format!("{v:.0} px");
    vec![
        group(
            "Snapping",
            vec![Row::new(
                "snap.edge",
                "Snap zone",
                Control::Slider {
                    min: 2.0,
                    max: 96.0,
                    step: 1.0,
                    int: true,
                    format: px,
                },
                Value::Int(c.snap.edge as i64),
                Value::Int(d.snap.edge as i64),
            )
            .sub("How near a monitor's side or top a dragged window snaps. Shown on every monitor as you change it")],
        ),
        group(
            "Tiling",
            vec![
                Row::new(
                    "tiling.layout",
                    "Tiling",
                    Control::Segmented(choices(&[
                        ("Floating", s("floating")),
                        ("Tiling", s("tiling")),
                    ])),
                    window_layout(t.layout),
                    window_layout(dt.layout),
                )
                .sub("Tiled windows share the screen without overlapping. Applies to every workspace unless Super+T is set to one workspace"),
                Row::new(
                    "tiling.toggle",
                    "Super+T toggles",
                    Control::Segmented(choices(&[
                        ("All workspaces", s("all")),
                        ("This workspace", s("workspace")),
                    ])),
                    tiling_toggle(t.toggle),
                    tiling_toggle(dt.toggle),
                )
                .sub("With one workspace, the Tiling setting is how new workspaces start"),
                Row::new(
                    "tiling.style",
                    "Tiling style",
                    Control::Segmented(choices(&[
                        ("Split", s("dwindle")),
                        ("Main + Stack", s("master")),
                    ])),
                    tiling_style(t.style),
                    tiling_style(dt.style),
                )
                .sub("Split halves the last window for each new one; Main + Stack stacks the rest beside one main window"),
                Row::new(
                    "tiling.master_ratio",
                    "First window width",
                    Control::Slider {
                        min: 0.1,
                        max: 0.9,
                        step: 0.05,
                        int: false,
                        format: |v| format!("{:.0}%", v * 100.0),
                    },
                    Value::Float(t.master_ratio),
                    Value::Float(dt.master_ratio),
                )
                .sub("Super+[ and Super+] change it for one workspace"),
                Row::new(
                    "tiling.gap",
                    "Gaps",
                    Control::Slider {
                        min: 0.0,
                        max: 64.0,
                        step: 1.0,
                        int: true,
                        format: px,
                    },
                    Value::Int(t.gap as i64),
                    Value::Int(dt.gap as i64),
                )
                .sub("Space between tiled windows"),
                Row::new(
                    "tiling.rounded",
                    "Rounded corners",
                    Control::switch(),
                    Value::Bool(t.rounded),
                    Value::Bool(dt.rounded),
                )
                .sub("Tiled windows keep their rounded corners and shadows"),
                Row::new(
                    "tiling.outer_gap",
                    "Screen edge gaps",
                    Control::Slider {
                        min: 0.0,
                        max: 128.0,
                        step: 1.0,
                        int: true,
                        format: px,
                    },
                    Value::Int(t.outer_gap as i64),
                    Value::Int(dt.outer_gap as i64),
                )
                .sub("Space between tiled windows and the edges of the screen")
                .advanced(),
            ],
        ),
    ]
}

fn clock_position(v: cnde_config::ClockPosition) -> Value {
    s(match v {
        cnde_config::ClockPosition::Left => "left",
        cnde_config::ClockPosition::Center => "center",
        cnde_config::ClockPosition::Right => "right",
    })
}

/// Clock formats offered in the dropdown; anything else shows as custom.
const CLOCKS: &[(&str, &str)] = &[
    ("Fri Oct 16  14:30", "%a %b %-d  %H:%M"),
    ("Fri Oct 16  2:30 PM", "%a %b %-d  %-I:%M %p"),
    ("14:30", "%H:%M"),
    ("2:30 PM", "%-I:%M %p"),
    ("Fri 14:30", "%a %H:%M"),
    ("Fri 2:30 PM", "%a %-I:%M %p"),
];

fn material(material: cnde_config::Material) -> Value {
    s(match material {
        cnde_config::Material::Frosted => "frosted",
        cnde_config::Material::Solid => "solid",
    })
}

/// The top bar's and dock's placements.
fn bar_modes() -> Vec<Choice> {
    choices(&[
        ("Attached", s("attached")),
        ("Floating", s("floating")),
        ("Auto", s("auto")),
    ])
}

fn bar_mode(mode: cnde_config::BarMode) -> Value {
    s(match mode {
        cnde_config::BarMode::Attached => "attached",
        cnde_config::BarMode::Floating => "floating",
        cnde_config::BarMode::Auto => "auto",
    })
}

/// The top bar's colour and material rows, shown on Appearance.
fn panel_look(c: &Config, d: &Config) -> Vec<Row> {
    let (p, dp) = (&c.panel, &d.panel);
    let px = |v: f64| format!("{v:.0} px");
    vec![
        Row::new(
            "panel.style",
            "Style",
            Control::Segmented(choices(&[
                ("Full bar", s("full")),
                ("Islands", s("islands")),
                ("No background", s("clear")),
            ])),
            s(p.style().as_str()),
            s(dp.style().as_str()),
        )
        .sub("Islands and no background become a full bar while a window touches it"),
        Row::new(
            "panel.rounded",
            "Top bar rounded corners",
            Control::switch(),
            Value::Bool(p.rounded),
            Value::Bool(dp.rounded),
        )
        .sub("Rounded bottom corners and a soft shadow")
        .advanced(),
        Row::new(
            "panel.edge_line",
            "Top bar edge line",
            Control::Slider {
                min: 0.0,
                max: 8.0,
                step: 1.0,
                int: true,
                format: px,
            },
            Value::Int(p.edge_line as i64),
            Value::Int(dp.edge_line as i64),
        )
        .sub("A thin shade along its bottom while it takes a window's colour; 0 is off"),
        Row::new(
            "panel.background",
            "Top bar background",
            Control::text("From the style"),
            s(&p.background),
            s(&dp.background),
        )
        .sub("#rrggbb, or #rrggbbaa to see through it; empty follows the style")
        .advanced(),
        Row::new(
            "panel.foreground",
            "Top bar text colour",
            Control::text("From the style"),
            s(&p.foreground),
            s(&dp.foreground),
        )
        .advanced(),
    ]
}

/// The dock's colour and material rows, shown on Appearance.
fn dock_look(c: &Config, d: &Config) -> Vec<Row> {
    let mut rows = Vec::new();
    let adaptive = |v: cnde_config::DockAdaptive| {
        s(match v {
            cnde_config::DockAdaptive::Bar => "bar",
            cnde_config::DockAdaptive::Darker => "darker",
        })
    };
    if !c.appearance.adaptive() {
        return rows;
    }
    rows.push(
        Row::new(
            "dock.adaptive_color",
            "Dock colour",
            Control::Segmented(choices(&[
                ("Same as bar", s("bar")),
                ("Darker", s("darker")),
            ])),
            adaptive(c.dock.adaptive_color),
            adaptive(d.dock.adaptive_color),
        )
        .sub("The top bar's, or a darker shade of the window against the bottom"),
    );
    rows
}

fn panel(c: &Config, d: &Config) -> Vec<Group> {
    let (p, dp) = (&c.panel, &d.panel);
    let mut clocks: Vec<Choice> = CLOCKS
        .iter()
        .map(|(label, format)| Choice {
            label: (*label).into(),
            value: s(format),
        })
        .collect();
    if !CLOCKS.iter().any(|(_, f)| *f == p.clock_format) {
        clocks.push(Choice {
            label: "Custom".into(),
            value: s(&p.clock_format),
        });
    }
    let px = |v: f64| format!("{v:.0} px");
    let mut groups = vec![
        group(
            "Top Bar",
            vec![
                Row::new(
                    "panel.enabled",
                    "Show the top bar",
                    Control::switch(),
                    Value::Bool(p.enabled),
                    Value::Bool(dp.enabled),
                )
                .sub("Off: bring your own bar, like waybar, via autostart"),
                Row::new(
                    "panel.all_monitors",
                    "On every display",
                    Control::switch(),
                    Value::Bool(p.all_monitors),
                    Value::Bool(dp.all_monitors),
                ),
                Row::new(
                    "panel.tray",
                    "Tray icons",
                    Control::Segmented(choices(&[
                        ("Collapsed", s("collapsed")),
                        ("Inline", s("inline")),
                        ("Off", s("off")),
                    ])),
                    s(p.tray.as_str()),
                    s(dp.tray.as_str()),
                )
                .sub("Icons from apps like Discord, Steam and Nextcloud: behind a chevron, or right in the bar"),
                Row::new(
                    "panel.power_icon",
                    "Power icon",
                    Control::switch(),
                    Value::Bool(p.power_icon),
                    Value::Bool(dp.power_icon),
                )
                .sub("A power icon on the status button; power options are in quick settings"),
                Row::new(
                    "panel.media_controls",
                    "Media controls",
                    Control::switch(),
                    Value::Bool(p.media_controls),
                    Value::Bool(dp.media_controls),
                )
                .sub("The song that's playing, with play and skip buttons, in the clock's popover"),
                Row::new(
                    "panel.clock_format",
                    "Clock",
                    Control::Dropdown(clocks),
                    s(&p.clock_format),
                    s(&dp.clock_format),
                ),
                Row::new(
                    "panel.clock_position",
                    "Clock position",
                    Control::Segmented(choices(&[
                        ("Left", s("left")),
                        ("Center", s("center")),
                        ("Right", s("right")),
                    ])),
                    clock_position(p.clock_position),
                    clock_position(dp.clock_position),
                ),
                Row::new(
                    "panel.clock_format",
                    "Clock format",
                    Control::text(""),
                    s(&p.clock_format),
                    s(&dp.clock_format),
                )
                .sub("strftime codes: %H:%M is 14:30, %a is Fri")
                .advanced(),
                Row::new(
                    "panel.height",
                    "Height",
                    Control::Slider {
                        min: 16.0,
                        max: 96.0,
                        step: 1.0,
                        int: true,
                        format: px,
                    },
                    Value::Int(p.height as i64),
                    Value::Int(dp.height as i64),
                )
                .advanced(),
                Row::new(
                    "panel.padding",
                    "Space at the ends",
                    Control::Slider {
                        min: 0.0,
                        max: 400.0,
                        step: 2.0,
                        int: false,
                        format: px,
                    },
                    Value::Float(p.padding as f64),
                    Value::Float(dp.padding as f64),
                )
                .sub("Between the screen's sides and the first and last buttons"),
                Row::new(
                    "panel.button_padding",
                    "Button padding",
                    Control::Slider {
                        min: 0.0,
                        max: 32.0,
                        step: 1.0,
                        int: false,
                        format: px,
                    },
                    Value::Float(p.button_padding as f64),
                    Value::Float(dp.button_padding as f64),
                )
                .sub("Room either side of each button"),
                Row::new(
                    "panel.font_size",
                    "Text size",
                    Control::Slider {
                        min: 8.0,
                        max: 32.0,
                        step: 0.5,
                        int: false,
                        format: |v| format!("{v}"),
                    },
                    Value::Float(p.font_size as f64),
                    Value::Float(dp.font_size as f64),
                )
                .advanced(),
                Row::new(
                    "panel.mode",
                    "Placement",
                    Control::Segmented(bar_modes()),
                    bar_mode(p.mode),
                    bar_mode(dp.mode),
                )
                .sub("Auto floats clear of the edge until a window touches it"),
                Row::new(
                    "panel.workspace_dot_size",
                    "Workspace dot size",
                    Control::Slider {
                        min: 2.0,
                        max: 32.0,
                        step: 1.0,
                        int: false,
                        format: px,
                    },
                    Value::Float(p.workspace_dot_size as f64),
                    Value::Float(dp.workspace_dot_size as f64),
                )
                .advanced(),
                Row::new(
                    "panel.workspace_active_width",
                    "Current workspace width",
                    Control::Slider {
                        min: 2.0,
                        max: 96.0,
                        step: 1.0,
                        int: false,
                        format: px,
                    },
                    Value::Float(p.workspace_active_width as f64),
                    Value::Float(dp.workspace_active_width as f64),
                )
                .advanced(),
            ],
        ),
        group(
            "Super Key",
            vec![
                Row::new(
                    "launcher.command",
                    "Run instead of the overview",
                    Control::text("Nothing: open the overview"),
                    s(&c.launcher.command),
                    s(&d.launcher.command),
                )
                .sub("A command, like fuzzel")
                .advanced(),
            ],
        ),
    ];
    // Off, the switch is all there is.
    if !p.enabled {
        groups.truncate(1);
        groups[0].rows.truncate(1);
    }
    groups
}

fn dock(c: &Config, d: &Config) -> Vec<Group> {
    use cnde_config::{DockPosition, DockVisibility};
    let position = |v: DockPosition| {
        s(match v {
            DockPosition::Overview => "overview",
            DockPosition::Bottom => "bottom",
            DockPosition::Left => "left",
            DockPosition::Right => "right",
            DockPosition::Off => "off",
        })
    };
    let visibility = |v: DockVisibility| {
        s(match v {
            DockVisibility::Always => "always",
            DockVisibility::Intellihide => "intellihide",
            DockVisibility::Autohide => "autohide",
        })
    };
    let mut rows = vec![Row::new(
        "dock.position",
        "Position",
        Control::Dropdown(choices(&[
            ("In the overview", s("overview")),
            ("Bottom of the screen", s("bottom")),
            ("Left of the screen", s("left")),
            ("Right of the screen", s("right")),
            ("No dock", s("off")),
        ])),
        position(c.dock.position),
        position(d.dock.position),
    )];
    let on_edge = matches!(
        c.dock.position,
        DockPosition::Bottom | DockPosition::Left | DockPosition::Right
    );
    if on_edge {
        rows.push(
            Row::new(
                "dock.visibility",
                "Visibility",
                Control::Segmented(choices(&[
                    ("Always", s("always")),
                    ("Intellihide", s("intellihide")),
                    ("Autohide", s("autohide")),
                ])),
                visibility(c.dock.visibility),
                visibility(d.dock.visibility),
            )
            .sub("Intellihide and autohide slide the dock off the edge while a window touches it; point at the edge to bring it back"),
        );
    }
    if on_edge {
        rows.push(Row::new(
            "dock.all_monitors",
            "On every display",
            Control::switch(),
            Value::Bool(c.dock.all_monitors),
            Value::Bool(d.dock.all_monitors),
        ));
    }
    if on_edge {
        rows.push(
            Row::new(
                "dock.mode",
                "Placement",
                Control::Segmented(bar_modes()),
                bar_mode(c.dock.mode),
                bar_mode(d.dock.mode),
            )
            .sub("Auto floats clear of the edge until a window touches it"),
        );
        rows.push(
            Row::new(
                "dock.extend",
                "Stretch across the edge",
                Control::switch(),
                Value::Bool(c.dock.extend),
                Value::Bool(d.dock.extend),
            )
            .sub("While attached, instead of being sized to its icons"),
        );
        rows.push(
            Row::new(
                "dock.reserve",
                "Windows stop at the dock",
                Control::switch(),
                Value::Bool(c.dock.reserve),
                Value::Bool(d.dock.reserve),
            )
            .sub("Off, the dock floats over maximized windows"),
        );
    }
    if c.dock.position != DockPosition::Off {
        let align = |v: cnde_config::DockAlign| {
            s(match v {
                cnde_config::DockAlign::Start => "start",
                cnde_config::DockAlign::Center => "center",
                cnde_config::DockAlign::End => "end",
            })
        };
        let side = matches!(c.dock.position, DockPosition::Left | DockPosition::Right);
        let (start, end) = if side {
            ("Top", "Bottom")
        } else {
            ("Left", "Right")
        };
        let float_align = |v: Option<cnde_config::DockAlign>| v.map_or(Value::Unset, align);
        let row = Row::new(
            "dock.align",
            "Alignment when attached",
            Control::Segmented(choices(&[
                (start, s("start")),
                ("Center", s("center")),
                (end, s("end")),
            ])),
            align(c.dock.align),
            align(d.dock.align),
        );
        rows.push(if on_edge && c.dock.extend {
            row.sub("Where the icons sit along the stretched dock")
        } else {
            row
        });
        rows.push(
            Row::new(
                "dock.float_align",
                "Alignment when floating",
                Control::Segmented(choices(&[
                    ("Same", Value::Unset),
                    (start, s("start")),
                    ("Center", s("center")),
                    (end, s("end")),
                ])),
                float_align(c.dock.float_align),
                float_align(d.dock.float_align),
            )
            .sub("Where the dock sits along its edge while it floats"),
        );
    }
    if c.dock.position != DockPosition::Off {
        rows.push(Row::new(
            "dock.icon_size",
            "Icon size",
            Control::Slider {
                min: 24.0,
                max: 128.0,
                step: 4.0,
                int: true,
                format: |v| format!("{v:.0} px"),
            },
            Value::Int(c.dock.icon_size as i64),
            Value::Int(d.dock.icon_size as i64),
        ));
        rows.push(
            Row::new(
                "dock.padding",
                "Icon spacing",
                Control::Slider {
                    min: 0.0,
                    max: 32.0,
                    step: 1.0,
                    int: false,
                    format: |v| format!("{v:.0} px"),
                },
                Value::Float(c.dock.padding as f64),
                Value::Float(d.dock.padding as f64),
            )
            .sub("Room either side of each icon"),
        );
        let side = matches!(c.dock.position, DockPosition::Left | DockPosition::Right);
        let (before, after) = if side {
            ("Top", "Bottom")
        } else {
            ("Left", "Right")
        };
        let launcher = |v: cnde_config::LauncherSide| {
            s(match v {
                cnde_config::LauncherSide::Start => "start",
                cnde_config::LauncherSide::End => "end",
            })
        };
        rows.push(
            Row::new(
                "dock.launcher",
                "App grid button",
                Control::Segmented(choices(&[(before, s("start")), (after, s("end"))])),
                launcher(c.dock.launcher),
                launcher(d.dock.launcher),
            )
            .sub("Which end of the icons it sits at"),
        );
        rows.push(Row::new(
            "dock.launcher_gap",
            "App grid button gap",
            Control::Slider {
                min: 0.0,
                max: 400.0,
                step: 2.0,
                int: false,
                format: |v| format!("{v:.0} px"),
            },
            Value::Float(c.dock.launcher_gap as f64),
            Value::Float(d.dock.launcher_gap as f64),
        ));
        rows.push(
            Row::new(
                "dock.locked",
                "Lock the dock",
                Control::switch(),
                Value::Bool(c.dock.locked),
                Value::Bool(d.dock.locked),
            )
            .sub("Icons and the dock can't be dragged or edited"),
        );
        rows.push(
            Row::new(
                "dock.click_minimizes",
                "Click again to minimize",
                Control::switch(),
                Value::Bool(c.dock.click_minimizes),
                Value::Bool(d.dock.click_minimizes),
            )
            .sub("Clicking the focused app's icon minimizes its windows"),
        );
        rows.push(
            Row::new(
                "dock.system_monitor",
                "System monitor",
                Control::text("gnome-system-monitor"),
                s(&c.dock.system_monitor),
                s(&d.dock.system_monitor),
            )
            .sub("Run by System Monitor in the dock's menu; empty hides it")
            .advanced(),
        );
    }
    vec![group("Dock", rows)]
}

/// Idle timeouts offered, in seconds (0 is "Never").
const IDLE_TIMES: [u32; 8] = [0, 60, 120, 300, 600, 900, 1800, 3600];

fn idle_time_label(secs: u32) -> String {
    match secs {
        0 => "Never".into(),
        3600 => "1 hour".into(),
        s if s % 3600 == 0 => format!("{} hours", s / 3600),
        60 => "1 minute".into(),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
}

/// The apps that can open each kind of file, from the index once it's built.
fn default_apps_page(
    c: &Config,
    d: &Config,
    state: Option<&crate::defaults::State>,
) -> Vec<Group> {
    use crate::defaults::CATEGORIES;
    let Some(state) = state else {
        return vec![Group {
            title: "Default Apps".into(),
            description: "Looking for installed apps…".into(),
            rows: Vec::new(),
        }];
    };
    let mut rows = Vec::new();
    for cat in &CATEGORIES {
        let offered = state.offered(cat);
        let current = state.current(cat).map_or("", |a| a.id.as_str());
        let mut list: Vec<Choice> = offered
            .iter()
            .map(|a| Choice {
                label: a.name.clone(),
                value: s(&a.id),
            })
            .collect();
        let mut row = Row {
            key: Key::DefaultApp(cat.id),
            subtitle: cat.subtitle.into(),
            ..wifi_row(WifiKey::Hidden, cat.title, Control::Dropdown(Vec::new()), s(current))
        };
        if list.is_empty() {
            list.push(Choice {
                label: "None".into(),
                value: s(""),
            });
            row.subtitle = "No installed app opens this".into();
            row.disabled = true;
        }
        row.control = Control::Dropdown(list);
        rows.push(row);
    }
    // The terminal has no MIME type: it's a config key.
    let mut terminals = vec![Choice {
        label: "Automatic".into(),
        value: s(""),
    }];
    terminals.extend(state.terminals().iter().map(|a| Choice {
        label: a.name.clone(),
        value: s(&a.id),
    }));
    let current = &c.apps.terminal;
    if !current.is_empty() && !terminals.iter().any(|t| t.value.as_str() == current) {
        terminals.push(Choice {
            label: current.clone(),
            value: s(current),
        });
    }
    rows.push(
        Row::new(
            "apps.terminal",
            "Terminal",
            Control::Dropdown(terminals),
            s(current),
            s(&d.apps.terminal),
        )
        .sub("Opens with Super+Q and for terminal apps; automatic picks one it finds"),
    );
    vec![Group {
        title: "Default Apps".into(),
        description: "What opens links, folders and files. Picking an app sets it for every file type it handles in the group.".into(),
        rows,
    }]
}

/// A row that isn't a config key: the power mode.
fn power_mode_row(key: Key, title: &str, control: Control, value: Value) -> Row {
    Row {
        key,
        ..wifi_row(WifiKey::Hidden, title, control, value)
    }
}

/// The power mode choice (Power Saver, Balanced, Performance) from the
/// daemon's last report, or a line saying why there isn't one.
fn power_mode_group(c: &Config, d: &Config, state: Option<&crate::power_mode::State>) -> Group {
    use crate::power_mode::Availability;
    let mut group = Group {
        title: "Power Mode".into(),
        description: String::new(),
        rows: Vec::new(),
    };
    let ready = state.filter(|s| s.availability == Availability::Ready);
    match (state, ready) {
        (None, _) => group.description = "Looking for the power mode service…".into(),
        (Some(s), None) if s.availability == Availability::Unknown => {
            group.description = "Looking for the power mode service…".into();
        }
        (Some(_), None) => {
            group.description =
                "Power modes need power-profiles-daemon, tuned-ppd or tlp-pd, and none is running."
                    .into();
        }
        (Some(_), Some(st)) => {
            group.description = st.error.clone().unwrap_or_default();
            let label = |mode: &str| match mode {
                "power-saver" => "Power Saver",
                "balanced" => "Balanced",
                _ => "Performance",
            };
            let list: Vec<(&str, Value)> = st.available.iter().map(|m| (label(m), s(m))).collect();
            if !list.is_empty() {
                group.rows.push(
                    power_mode_row(
                        Key::PowerMode,
                        "Power mode",
                        Control::Segmented(choices(&list)),
                        s(&st.active),
                    )
                    .sub("Performance runs faster and hotter; Power Saver lasts longer"),
                );
            }
        }
    }
    group.rows.push(
        Row::new(
            "power.profile_command",
            "Power mode command",
            Control::text("powerprofilesctl set"),
            s(&c.power.profile_command),
            s(&d.power.profile_command),
        )
        .sub("Without a daemon, run by the top bar's power mode buttons with power-saver, balanced or performance added")
        .advanced(),
    );
    group
}

fn power(c: &Config, d: &Config, power_mode: Option<&crate::power_mode::State>) -> Vec<Group> {
    use cnde_config::PowerProfile;
    // The usual times, plus a hand-edited one so the row can show it.
    let times = |current: u32| {
        let mut list = IDLE_TIMES.to_vec();
        if !list.contains(&current) {
            list.push(current);
            list.sort_unstable();
        }
        Control::Dropdown(
            list.into_iter()
                .map(|secs| Choice {
                    label: idle_time_label(secs),
                    value: Value::Int(secs as i64),
                })
                .collect(),
        )
    };
    let rows = |keys: [&'static str; 3], now: &PowerProfile, default: &PowerProfile| {
        let row = |key, title: &str, sub: &str, now: u32, default: u32| {
            Row::new(
                key,
                title,
                times(now),
                Value::Int(now as i64),
                Value::Int(default as i64),
            )
            .sub(sub)
        };
        vec![
            row(
                keys[0],
                "Dim the screen",
                "After no input; any key or movement undoes it",
                now.dim_after,
                default.dim_after,
            ),
            row(
                keys[1],
                "Turn the screen off",
                "",
                now.screen_off_after,
                default.screen_off_after,
            ),
            row(
                keys[2],
                "Suspend",
                "Locks first if a lock command is set",
                now.suspend_after,
                default.suspend_after,
            ),
        ]
    };
    vec![
        power_mode_group(c, d, power_mode),
        group(
            "Locking",
            vec![
                Row::new(
                    "power.lock_on_screen_off",
                    "Lock when the screen turns off",
                    Control::switch(),
                    Value::Bool(c.power.lock_on_screen_off),
                    Value::Bool(d.power.lock_on_screen_off),
                )
                .sub("Needs a lock command"),
                Row::new(
                    "power.lock_before_suspend",
                    "Lock before suspending",
                    Control::switch(),
                    Value::Bool(c.power.lock_before_suspend),
                    Value::Bool(d.power.lock_before_suspend),
                )
                .sub("Needs a lock command"),
            ],
        ),
        group(
            "Battery",
            vec![
                Row::new(
                    "power.battery_percent",
                    "Show percentage",
                    Control::switch(),
                    Value::Bool(c.power.battery_percent),
                    Value::Bool(d.power.battery_percent),
                )
                .sub("Next to the battery icon in the top bar"),
                Row::new(
                    "power.low_battery",
                    "Low battery warning",
                    Control::Slider {
                        min: 0.0,
                        max: 50.0,
                        step: 5.0,
                        int: true,
                        format: |v| {
                            if v == 0.0 {
                                "Off".to_string()
                            } else {
                                format!("{v:.0}%")
                            }
                        },
                    },
                    Value::Int(c.power.low_battery as i64),
                    Value::Int(d.power.low_battery as i64),
                )
                .sub("Warns when the charge falls below this while unplugged"),
            ],
        ),
        Group {
            title: "On battery".into(),
            description: "Videos playing keep the computer awake".into(),
            rows: rows(
                [
                    "power.battery.dim_after",
                    "power.battery.screen_off_after",
                    "power.battery.suspend_after",
                ],
                &c.power.battery,
                &d.power.battery,
            ),
        },
        group(
            "Plugged in",
            rows(
                [
                    "power.ac.dim_after",
                    "power.ac.screen_off_after",
                    "power.ac.suspend_after",
                ],
                &c.power.ac,
                &d.power.ac,
            ),
        ),
    ]
}

fn accessibility(c: &Config, d: &Config) -> Vec<Group> {
    vec![
        group(
        "Motion",
        vec![
            Row::new(
                "animations.speed",
                "Animations",
                Control::Switch {
                    on: Value::Unset,
                    off: Value::Float(0.0),
                },
                Value::Float(c.animations.speed),
                Value::Float(d.animations.speed),
            )
            .sub("Windows and menus move as they open and close"),
            Row::new(
                "animations.speed",
                "Animation length",
                Control::Slider {
                    min: 0.0,
                    max: 3.0,
                    step: 0.1,
                    int: false,
                    format: |v| {
                        if v == 0.0 {
                            "Off".into()
                        } else {
                            format!("{v:.1}×")
                        }
                    },
                },
                Value::Float(c.animations.speed),
                Value::Float(d.animations.speed),
            )
            .sub("Below 1× is faster, above is slower")
            .advanced(),
        ],
    ),
    group(
        "Notifications",
        vec![
            Row::new(
                "notifications.enabled",
                "Show notifications",
                Control::switch(),
                Value::Bool(c.notifications.enabled),
                Value::Bool(d.notifications.enabled),
            )
            .sub("Off to use another notification daemon"),
            Row::new(
                "notifications.do_not_disturb",
                "Do Not Disturb",
                Control::switch(),
                Value::Bool(c.notifications.do_not_disturb),
                Value::Bool(d.notifications.do_not_disturb),
            )
            .sub("Only urgent ones show"),
            Row::new(
                "notifications.history",
                "Notification history",
                Control::switch(),
                Value::Bool(c.notifications.history),
                Value::Bool(d.notifications.history),
            )
            .sub("The clock's popover lists past notifications"),
        ],
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writing every row's default back must leave a valid config: catches
    /// a row pointing at a misspelled key or writing the wrong type.
    #[test]
    fn every_row_writes_a_valid_config() {
        let config = Config::default();
        let display = |name: &str| Display {
            name: name.into(),
            description: String::new(),
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            vrr_capable: true,
            modes: vec![
                (1920, 1080, 165_000),
                (1920, 1080, 59_940),
                (1280, 720, 60_000),
            ],
            current: Some((1920, 1080, 165_000)),
            preferred: Some((1920, 1080, 165_000)),
        };
        let displays = [display("DP-1"), display("DP-2")];
        for page in Page::ALL {
            for group in build(
                page,
                &config,
                &displays,
                WifiView::default(),
                None,
                Some(50),
                None,
                None,
                None,
            ) {
                for row in group.rows {
                    let mut file = ConfigFile::open(std::path::Path::new("/nonexistent")).unwrap();
                    let value = match &row.control {
                        Control::Switch { on, .. } => on.clone(),
                        Control::Segmented(c) | Control::Dropdown(c) => {
                            c[c.len() - 1].value.clone()
                        }
                        Control::Slider { min, int, .. } => {
                            if *int {
                                Value::Int(*min as i64)
                            } else {
                                Value::Float(*min)
                            }
                        }
                        _ => row.value.clone(),
                    };
                    row.key.write(&mut file, &config, &value);
                    if let Err(err) = file.config() {
                        panic!("{}: {:?} -> {err}", row.title, row.key);
                    }
                }
            }
        }
    }

    #[test]
    fn power_mode_group_follows_the_daemon() {
        use crate::power_mode::{Availability, State};
        let config = Config::default();
        let groups = |state: Option<&State>| power(&config, &Config::default(), state);
        let mode_row = |g: &Vec<Group>| g[0].rows.iter().find(|r| r.key == Key::PowerMode).cloned();

        // Not heard from yet, and no daemon: a line, no buttons, but the
        // advanced command row stays.
        let none = groups(None);
        assert_eq!(none[0].title, "Power Mode");
        assert!(mode_row(&none).is_none());
        let gone = groups(Some(&State {
            availability: Availability::NoService,
            ..State::default()
        }));
        assert!(gone[0].description.contains("tlp-pd"));
        assert!(mode_row(&gone).is_none());
        assert!(
            gone[0]
                .rows
                .iter()
                .any(|r| r.key == Key::Path("power.profile_command") && r.advanced)
        );

        // A daemon without Performance offers two.
        let ready = groups(Some(&State {
            availability: Availability::Ready,
            active: "balanced".into(),
            available: vec!["power-saver".into(), "balanced".into()],
            error: None,
        }));
        let row = mode_row(&ready).unwrap();
        assert_eq!(row.value, Value::Str("balanced".into()));
        let Control::Segmented(choices) = &row.control else {
            panic!("a segmented control");
        };
        let labels: Vec<_> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Power Saver", "Balanced"]);
    }

    #[test]
    fn dark_schedule_rows() {
        let mut config = Config::default();
        let titles = |config: &Config| -> Vec<String> {
            dark_schedule(config, &Config::default())
                .rows
                .into_iter()
                .map(|r| r.title)
                .collect()
        };
        assert_eq!(titles(&config), ["Switch automatically"]);
        // Sunset with no location: the custom times stand in.
        config.dark_mode.schedule = cnde_config::DarkSchedule::Sunset;
        assert_eq!(
            titles(&config),
            [
                "Switch automatically",
                "Dark from",
                "Light from",
                "Location"
            ]
        );
        config.location.coordinates = "52.52, 13.40".into();
        assert_eq!(titles(&config), ["Switch automatically", "Location"]);
        let group = dark_schedule(&config, &Config::default());
        assert!(group.rows[0].subtitle.starts_with("Sunset "));
        for row in group.rows {
            let mut file = ConfigFile::open(std::path::Path::new("/nonexistent")).unwrap();
            row.key.write(&mut file, &config, &row.value);
            assert!(file.config().is_ok(), "{}", row.title);
        }
    }

    #[test]
    fn custom_shortcut_rows_write() {
        let mut file = ConfigFile::open(std::path::Path::new("/nonexistent")).unwrap();
        // Each write sees the file as it is now, like the settings app does.
        let write = |file: &mut ConfigFile, key: Key, value: Value| {
            let before = file.config().unwrap();
            key.write(file, &before, &value);
            file.config().unwrap()
        };
        // Added with a command and no keys yet; an empty command adds nothing.
        let config = write(&mut file, Key::NewCustomShortcut, Value::Str("htop".into()));
        assert_eq!(config.shortcuts.custom.len(), 1);
        assert!(config.shortcuts.custom[0].keys.is_empty());
        let config = write(&mut file, Key::NewCustomShortcut, Value::Str(String::new()));
        assert_eq!(config.shortcuts.custom.len(), 1);
        // Keys typed in, then the command edited; keys are kept.
        let keys = Value::Str("Super+G, Ctrl+G".into());
        let config = write(&mut file, Key::CustomShortcut(0), keys);
        assert_eq!(config.shortcuts.custom[0].keys, ["Super+G", "Ctrl+G"]);
        let config = write(&mut file, Key::CustomCommand(0), Value::Str(String::new()));
        assert_eq!(config.shortcuts.custom[0].command, "htop");
        let config = write(&mut file, Key::CustomCommand(0), Value::Str("foot".into()));
        assert_eq!(config.shortcuts.custom[0].command, "foot");
        assert_eq!(config.shortcuts.custom[0].keys, ["Super+G", "Ctrl+G"]);
        // Unset removes the entry.
        let config = write(&mut file, Key::CustomShortcut(0), Value::Unset);
        assert!(config.shortcuts.custom.is_empty());
    }

    #[test]
    fn night_light_rows() {
        let mut config = Config::default();
        let titles = |config: &Config| -> Vec<String> {
            night_light(config, &Config::default())
                .rows
                .into_iter()
                .map(|r| r.title)
                .collect()
        };
        assert_eq!(titles(&config), ["Night light"]);
        config.night_light.schedule = cnde_config::NightSchedule::Custom;
        assert_eq!(
            titles(&config),
            [
                "Night light",
                "Colour temperature",
                "Intensity",
                "On from",
                "Off from"
            ]
        );
        config.night_light.schedule = cnde_config::NightSchedule::Sunset;
        config.location.coordinates = "52.52, 13.40".into();
        assert_eq!(
            titles(&config),
            ["Night light", "Colour temperature", "Intensity", "Location"]
        );
        for row in night_light(&config, &Config::default()).rows {
            let mut file = ConfigFile::open(std::path::Path::new("/nonexistent")).unwrap();
            row.key.write(&mut file, &config, &row.value);
            assert!(file.config().is_ok(), "{}", row.title);
        }
    }

    #[test]
    fn other_shell_hides_bar_and_dock() {
        let mut config = Config::default();
        assert!(Page::visible(&config).contains(&Page::Panel));
        assert!(Page::visible(&config).contains(&Page::Dock));
        config.autostart.shell = "waybar".into();
        let pages = Page::visible(&config);
        assert!(!pages.contains(&Page::Panel) && !pages.contains(&Page::Dock));
        assert!(!bar_shown(&config) && !dock_shown(&config));
        config.autostart.shell = String::new();
        assert!(!Page::visible(&config).contains(&Page::Dock));
    }

    #[test]
    fn disabled_bar_and_dock_keep_only_their_switch() {
        let mut config = Config::default();
        config.panel.enabled = false;
        config.dock.position = cnde_config::DockPosition::Off;
        let d = Config::default();
        let bar = panel(&config, &d);
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].rows.len(), 1);
        assert_eq!(dock(&config, &d)[0].rows.len(), 1);
        let keys: Vec<_> = appearance(&config, &d)
            .into_iter()
            .flat_map(|g| g.rows)
            .collect();
        assert!(
            keys.iter()
                .all(|r| r.title != "Top bar edge line" && r.title != "Dock colour")
        );
        let titles = |c: &Config| -> Vec<String> {
            appearance(c, &d)
                .into_iter()
                .flat_map(|g| g.rows)
                .map(|r| r.title)
                .collect()
        };
        let on = titles(&Config::default());
        assert!(on.iter().any(|t| t == "Top bar edge line"));
        // The dock's adaptive colour shows only with that palette.
        assert!(!on.iter().any(|t| t == "Dock colour"));
        let mut adaptive = Config::default();
        adaptive.appearance.palette = cnde_config::Palette::Adaptive;
        assert!(titles(&adaptive).iter().any(|t| t == "Dock colour"));
    }

    #[test]
    fn sound_page_has_the_mixer_once_ready() {
        use crate::sound::{Availability, State};
        let c = Config::default();
        let groups = sound_page(&c, &c, None);
        assert!(groups[0].rows.is_empty());
        assert_eq!(groups[0].description, "Looking for the audio server…");
        let mut state = State {
            availability: Availability::Unavailable,
            ..State::default()
        };
        assert!(sound_page(&c, &c, Some(&state))[0].description.contains("pactl"));
        state.availability = Availability::Ready;
        let groups = sound_page(&c, &c, Some(&state));
        assert!(matches!(groups[0].rows[0].control, Control::Mixer));
        assert_eq!(groups[1].title, "System sounds");
        assert_eq!(groups[1].rows.len(), 1);
    }

    #[test]
    fn turning_goes_round_both_ways() {
        use cnde_config::Rotation::*;
        assert_eq!(turned(Normal, true), s("right"));
        assert_eq!(turned(Right, true), s("upside-down"));
        assert_eq!(turned(UpsideDown, true), s("left"));
        assert_eq!(turned(Left, true), Value::Unset);
        assert_eq!(turned(Normal, false), s("left"));
        assert_eq!(turned(Left, false), s("upside-down"));
        assert_eq!(turned(Right, false), Value::Unset);
    }

    #[test]
    fn touchpad_in_udev_data() {
        let pad = "I:1\nE:ID_INPUT=1\nE:ID_INPUT_TOUCHPAD=1\nG:seat\n";
        let mouse = "I:1\nE:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\n";
        assert!(is_touchpad(pad));
        assert!(!is_touchpad(mouse));
        assert!(!is_touchpad("E:ID_INPUT_TOUCHPAD=0\n"));
        assert!(!is_touchpad(""));
    }

    #[test]
    fn pages_by_name() {
        assert_eq!(Page::from_name("displays"), Some(Page::Displays));
        assert_eq!(Page::from_name("mouse"), Some(Page::Mouse));
        assert_eq!(Page::from_name("bluetooth"), Some(Page::Bluetooth));
        assert_eq!(Page::from_name("nope"), None);
    }
}
