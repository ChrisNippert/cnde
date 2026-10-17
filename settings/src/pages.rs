//! What each page shows: groups of rows, each row one config key and the
//! control that edits it. Pages are rebuilt from the current config every
//! paint, so they always show what the file says.

use cnde_config::{Config, ConfigFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Appearance,
    Displays,
    Wifi,
    Mouse,
    Keyboard,
    Panel,
    Dock,
    Accessibility,
}

impl Page {
    pub const ALL: [Page; 8] = [
        Page::Appearance,
        Page::Displays,
        Page::Wifi,
        Page::Mouse,
        Page::Keyboard,
        Page::Panel,
        Page::Dock,
        Page::Accessibility,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Displays => "Displays",
            Page::Wifi => "Wi-Fi",
            Page::Mouse => "Mouse & Touchpad",
            Page::Keyboard => "Keyboard",
            Page::Panel => "Top Bar",
            Page::Dock => "Dock",
            Page::Accessibility => "Accessibility",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Appearance => "preferences-desktop-appearance-symbolic",
            Page::Displays => "video-display-symbolic",
            Page::Wifi => "network-wireless-symbolic",
            Page::Mouse => "input-mouse-symbolic",
            Page::Keyboard => "input-keyboard-symbolic",
            Page::Panel => "focus-top-bar-symbolic",
            Page::Dock => "view-app-grid-symbolic",
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

/// A value as written to the file. `Unset` removes the key, so the default
/// applies.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
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
    /// Which monitor has `main = true`: a name, or `Unset` for automatic.
    MainDisplay,
    /// Not in the config: Wi-Fi, done through NetworkManager or iwd.
    Wifi(WifiKey),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WifiKey {
    /// The radio on or off.
    Enabled,
    /// A network in the list (clicking it connects).
    Network(String),
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
            // Not the config's business: the window sends these to the
            // Wi-Fi worker instead.
            Key::Wifi(_) => {}
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
    /// A Wi-Fi network: its signal, a lock if it needs a password, a check
    /// if connected. Clicking the row joins it.
    Network {
        signal: u8,
        secured: bool,
        connected: bool,
    },
    /// A text entry that shows dots.
    Password,
    /// Read-only text on the right (connection details).
    Info,
    /// A colour: a hex entry, and swatches to pick from below the title.
    Color {
        placeholder: &'static str,
    },
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
}

/// What the Wi-Fi page shows: the worker's last report, and the network
/// being joined (its form shows under it).
#[derive(Clone, Copy, Default)]
pub struct WifiView<'a> {
    pub state: Option<&'a crate::wifi::State>,
    pub join: Option<&'a crate::wifi::Join>,
}

pub fn build(page: Page, c: &Config, displays: &[Display], wifi: WifiView<'_>) -> Vec<Group> {
    let d = Config::default();
    match page {
        Page::Appearance => appearance(c, &d),
        Page::Displays => displays_page(c, displays),
        Page::Wifi => wifi_page(wifi),
        Page::Mouse => mouse(c, &d),
        Page::Keyboard => keyboard(c, &d),
        Page::Panel => panel(c, &d),
        Page::Dock => dock(c, &d),
        Page::Accessibility => accessibility(c, &d),
    }
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
    let mut groups = vec![top];
    let join = view.join;
    let joining = |ssid: &str| join.is_some_and(|j| !j.hidden && j.ssid == ssid);

    let network_row = |network: &crate::wifi::Network| {
        let connecting = state.connecting.as_deref() == Some(network.ssid.as_str());
        let status = if network.connected {
            "Connected"
        } else if connecting {
            "Connecting…"
        } else if network.known {
            "Saved"
        } else {
            ""
        };
        let mut row = wifi_row(
            WifiKey::Network(network.ssid.clone()),
            &network.ssid,
            Control::Network {
                signal: network.signal,
                secured: network.security.secured(),
                connected: network.connected,
            },
            Value::Unset,
        );
        row.subtitle = match status {
            "" => network.security.label().to_string(),
            status => format!("{status} · {}", network.security.label()),
        };
        row
    };

    if let Some(network) = state.networks.iter().find(|n| n.connected) {
        let mut rows = vec![network_row(network)];
        if let Some(details) = &state.details {
            rows.extend(detail_rows(network, details));
        }
        rows.push(wifi_row(
            WifiKey::Actions(network.ssid.clone()),
            "",
            Control::Segmented(choices(&[
                ("Disconnect", s("disconnect")),
                ("Forget", s("forget")),
            ])),
            Value::Unset,
        ));
        groups.push(group("Connected", rows));
    }

    let mut rows = Vec::new();
    for network in state.networks.iter().filter(|n| !n.connected) {
        rows.push(network_row(network));
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
    groups
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
            cnde_config::Palette::Cnde => "cnde",
            cnde_config::Palette::Adwaita => "adwaita",
            cnde_config::Palette::Custom => "custom",
        })
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
            "Style",
            vec![
                Row::new(
                    "appearance.color_scheme",
                    "Style",
                    Control::Segmented(choices(&[("Dark", s("dark")), ("Light", s("light"))])),
                    scheme(c.appearance.color_scheme),
                    scheme(d.appearance.color_scheme),
                )
                .sub("Menus, quick settings and this window; also picks the wallpaper"),
                Row::new(
                    "appearance.palette",
                    "Colours",
                    Control::Segmented(choices(&[
                        ("cnde", s("cnde")),
                        ("GNOME", s("adwaita")),
                        ("Custom", s("custom")),
                    ])),
                    palette(c.appearance.palette),
                    palette(d.appearance.palette),
                )
                .sub(match c.appearance.palette {
                    cnde_config::Palette::Custom => "Worked out from the two colours below",
                    _ => "cnde: plum and pink when dark, blush and rose when light",
                }),
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
    // Custom colours: the two the palette is worked out from.
    if c.appearance.palette == cnde_config::Palette::Custom
        && let Some(style) = groups.first_mut()
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
            .sub("Switches, selections and sliders"),
        );
    }
    groups
}

fn displays_page(c: &Config, displays: &[Display]) -> Vec<Group> {
    if displays.is_empty() {
        return vec![Group {
            title: "No displays found".into(),
            description: "The compositor didn't list any monitors.".into(),
            rows: Vec::new(),
        }];
    }
    let mut groups = Vec::new();
    if displays.len() > 1 {
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
                }
            };
            let mut rows = vec![Row {
                key: output_key("scale"),
                title: "Scale".into(),
                subtitle: "Makes everything bigger on high-resolution screens".into(),
                control: Control::Dropdown(scales),
                value: scale,
                default: Value::Unset,
                advanced: false,
            }];
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
    groups
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

fn mouse(c: &Config, d: &Config) -> Vec<Group> {
    let (m, dm) = (&c.input.mouse, &d.input.mouse);
    let (t, dt) = (&c.input.touchpad, &d.input.touchpad);
    vec![
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
        group(
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
        ),
    ]
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
    vec![
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
                    "panel.frosted",
                    "Frosted",
                    Control::switch(),
                    Value::Bool(p.frosted),
                    Value::Bool(dp.frosted),
                )
                .sub("Translucent over the blurred wallpaper"),
                Row::new(
                    "panel.rounded",
                    "Rounded corners",
                    Control::switch(),
                    Value::Bool(p.rounded),
                    Value::Bool(dp.rounded),
                )
                .sub("Rounded bottom corners and a soft shadow")
                .advanced(),
                Row::new(
                    "panel.background",
                    "Background colour",
                    Control::text("From the style"),
                    s(&p.background),
                    s(&dp.background),
                )
                .sub("#rrggbb, or #rrggbbaa to see through it; empty follows the style")
                .advanced(),
                Row::new(
                    "panel.foreground",
                    "Text colour",
                    Control::text("From the style"),
                    s(&p.foreground),
                    s(&dp.foreground),
                )
                .advanced(),
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
    ]
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
            .sub("Intellihide and autohide are coming; both act as always for now"),
        );
    }
    if c.dock.position != DockPosition::Off {
        let background = |v: cnde_config::DockBackground| {
            s(match v {
                cnde_config::DockBackground::Solid => "solid",
                cnde_config::DockBackground::Frosted => "frosted",
            })
        };
        rows.push(
            Row::new(
                "dock.background",
                "Background",
                Control::Segmented(choices(&[("Solid", s("solid")), ("Frosted", s("frosted"))])),
                background(c.dock.background),
                background(d.dock.background),
            )
            .sub("Frosted shows the wallpaper through, blurred"),
        );
    }
    if on_edge {
        rows.push(
            Row::new(
                "dock.extend",
                "Stretch across the edge",
                Control::switch(),
                Value::Bool(c.dock.extend),
                Value::Bool(d.dock.extend),
            )
            .sub("Instead of sitting in the middle, sized to its icons"),
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
        let row = Row::new(
            "dock.align",
            "Alignment",
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
    }
    vec![group("Dock", rows)]
}

fn accessibility(c: &Config, d: &Config) -> Vec<Group> {
    vec![group(
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
        let displays = [Display {
            name: "DP-1".into(),
            description: String::new(),
        }];
        for page in Page::ALL {
            for group in build(page, &config, &displays, WifiView::default()) {
                for row in group.rows {
                    let mut file = ConfigFile::open(std::path::Path::new("/nonexistent")).unwrap();
                    let value = match &row.control {
                        Control::Switch { on, .. } => on.clone(),
                        Control::Segmented(c) | Control::Dropdown(c) => c[0].value.clone(),
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
    fn pages_by_name() {
        assert_eq!(Page::from_name("displays"), Some(Page::Displays));
        assert_eq!(Page::from_name("mouse"), Some(Page::Mouse));
        assert_eq!(Page::from_name("nope"), None);
    }
}
