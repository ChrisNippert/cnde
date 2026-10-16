//! The cnde configuration: schema, defaults, loading and validation
//! (ARCHITECTURE.md §8). Shared by the compositor, and later the shell and
//! the settings app, so hand-edits and the GUI always agree.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// Written to disk on first run so every option is discoverable.
pub const DEFAULT_CONFIG: &str = include_str!("default-config.toml");

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub input: Input,
    /// Per-monitor settings, matched by connector name (e.g. "DP-2").
    #[serde(rename = "output")]
    pub outputs: Vec<OutputConfig>,
    pub animations: Animations,
    pub panel: Panel,
    pub launcher: Launcher,
    pub autostart: Autostart,
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
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            layout: String::new(),
            variant: String::new(),
            options: String::new(),
            repeat_delay: 200,
            repeat_rate: 25,
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
    /// The monitor the workspace strip is anchored to (its workspace is
    /// "the current one"). Default: the leftmost monitor.
    #[serde(default)]
    pub main: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub x: i32,
    pub y: i32,
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
    /// Run cnde-shell. Off: bring your own bar (waybar etc.) via autostart.
    pub enabled: bool,
    /// Show the panel on every monitor instead of only the main one.
    pub all_monitors: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            enabled: true,
            all_monitors: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Launcher {
    /// Run when Super is tapped on its own. Temporary until cnde-shell's
    /// overview exists (ARCHITECTURE.md §11, Milestone 2).
    pub command: String,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            command: "fuzzel".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Autostart {
    /// Commands run once when the compositor starts.
    pub commands: Vec<String>,
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
        if self.input.keyboard.repeat_rate < 0 || self.input.keyboard.repeat_delay < 0 {
            return Err(Error::Invalid(
                "keyboard repeat values can't be negative".into(),
            ));
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
    fn outputs_parse() {
        let config = Config::parse(
            "[[output]]\nname = \"DP-2\"\nposition = { x = 1920, y = 0 }\nscale = 1.5\n",
        )
        .unwrap();
        let output = config.output("DP-2").unwrap();
        assert_eq!(output.position, Some(Position { x: 1920, y: 0 }));
        assert_eq!(output.scale, Some(1.5));
    }
}
