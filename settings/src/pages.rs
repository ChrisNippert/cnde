//! What each page shows: groups of rows, each row one config key and the
//! control that edits it. Pages are rebuilt from the current config every
//! paint, so they always show what the file says.

use cnde_config::{Config, ConfigFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Appearance,
    Displays,
    Mouse,
    Keyboard,
    Panel,
    Dock,
    Accessibility,
}

impl Page {
    pub const ALL: [Page; 7] = [
        Page::Appearance,
        Page::Displays,
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
}

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

pub fn build(page: Page, c: &Config, displays: &[Display]) -> Vec<Group> {
    let d = Config::default();
    match page {
        Page::Appearance => appearance(c, &d),
        Page::Displays => displays_page(c, displays),
        Page::Mouse => mouse(c, &d),
        Page::Keyboard => keyboard(c, &d),
        Page::Panel => panel(c, &d),
        Page::Dock => dock(c, &d),
        Page::Accessibility => accessibility(c, &d),
    }
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
    let fit = |v: WallpaperFit| {
        s(match v {
            WallpaperFit::Fill => "fill",
            WallpaperFit::Fit => "fit",
            WallpaperFit::Stretch => "stretch",
        })
    };
    vec![
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
    ]
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
                    "panel.background",
                    "Background colour",
                    Control::text("#000000"),
                    s(&p.background),
                    s(&dp.background),
                )
                .sub("#rrggbb, or #rrggbbaa to see through it")
                .advanced(),
                Row::new(
                    "panel.foreground",
                    "Text colour",
                    Control::text("#ffffff"),
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
            for group in build(page, &config, &displays) {
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
