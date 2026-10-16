//! The private compositor <-> shell channel (ARCHITECTURE.md §6): things no
//! Wayland protocol covers, like the overview opening and closing.
//!
//! A Unix socket at `$CNDE_SOCKET`, carrying one JSON object per line. Every
//! connected client receives every [`Event`]; clients send [`Request`]s.
//! Unknown messages are ignored on both sides, so either end can be newer.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Set by the compositor for everything it launches.
pub const SOCKET_ENV: &str = "CNDE_SOCKET";

/// Compositor -> clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The overview opened; the search box belongs on `output`.
    OverviewOpened {
        output: String,
    },
    OverviewClosed,
    /// Super+A: show the app grid (or hide it, if it's showing).
    ToggleAppGrid,
    /// Every open window, sent whenever the list, a title or the focus
    /// changes (and right after connecting).
    Windows {
        windows: Vec<WindowInfo>,
    },
    /// The screen is frozen for a screenshot: show the selection tool.
    /// `windows`: what window mode can pick, top first, global logical px.
    ScreenshotUi {
        windows: Vec<ScreenshotWindow>,
    },
    /// Saved (and copied), or `None` if cancelled or failed.
    ScreenshotDone {
        path: Option<String>,
    },
    /// The wallpaper was right-clicked: show the desktop menu at (`x`,
    /// `y`), logical pixels within `output`. `serial` is the click's: the
    /// shell never saw it (it went to no surface), and a menu's grab must
    /// name the click that opened it.
    DesktopMenu {
        output: String,
        x: i32,
        y: i32,
        serial: u32,
    },
    /// A volume or brightness key: the shell changes it and shows the
    /// level. Sent again while the key's held.
    MediaKey {
        key: MediaKey,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKey {
    VolumeUp,
    VolumeDown,
    Mute,
    MicMute,
    BrightnessUp,
    BrightnessDown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotWindow {
    pub id: u64,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// What to capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenshotTarget {
    /// Global logical pixels.
    Region {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    Output {
        name: String,
    },
    Window {
        id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub focused: bool,
}

/// Clients -> compositor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    SetOverview {
        open: bool,
    },
    ToggleOverview,
    /// Open the overview on the app grid (the dock's apps button).
    ShowAppGrid,
    /// Focus this window, switching to its workspace if needed.
    ActivateWindow {
        id: u64,
    },
    /// The app grid appeared or went away (so Super knows what's next).
    AppGridShown {
        shown: bool,
    },
    Screenshot {
        target: ScreenshotTarget,
    },
    CancelScreenshot,
    /// Open the screenshot tool, as Super+Shift+S does.
    OpenScreenshot,
    /// Ask this window to close (as its close button would).
    CloseWindow {
        id: u64,
    },
    /// An app was just started: show a busy pointer until a window opens
    /// (or a few seconds pass).
    Launching,
    /// Testing aid, honoured only by debug builds: move the pointer to
    /// (x, y) in the global space, then press or release the left button.
    FakePointer {
        x: f64,
        y: f64,
        #[serde(default)]
        press: Option<bool>,
    },
    /// Testing aid, debug builds only: as if this key were pressed.
    FakeMediaKey {
        key: MediaKey,
    },
}

/// The overview while the app grid shows: workspace thumbnails in a row
/// under the search box, the grid below them. Shared so the compositor's
/// thumbnails and the shell's grid fit together.
/// The dock, as the compositor needs to know it to frost what's behind it.
pub mod dock {
    /// The dock's layer-shell namespace.
    pub const NAMESPACE: &str = "cnde-dock";
    /// Corner radius of a dock that doesn't stretch across its edge.
    pub const RADIUS: f64 = 24.0;
}

pub mod app_grid {
    /// The thumbnails' top, logical px below the top of the area the panel
    /// leaves free: under the search box (its margin, height and a gap).
    pub const THUMBS_TOP: f64 = 20.0 + 48.0 + 18.0;
    /// Thumbnail height as a fraction of the monitor's height.
    pub const THUMB_SCALE: f64 = 0.11;
    /// Between thumbnails.
    pub const THUMB_GAP: f64 = 14.0;
    /// Between the thumbnails and the grid.
    const BELOW: f64 = 18.0;

    /// Where the grid starts, below the free area's top.
    pub fn grid_top(output_height: f64) -> f64 {
        THUMBS_TOP + output_height * THUMB_SCALE + BELOW
    }
}

/// Where a compositor with process id `pid` listens.
pub fn socket_path(pid: u32) -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join(format!("cnde-{pid}.sock")))
}

/// One message as a line, ready to write.
pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
    let mut line = serde_json::to_vec(message).unwrap_or_default();
    line.push(b'\n');
    line
}

/// Parse one line (without its newline); `None` if it isn't a `T`.
pub fn decode<'a, T: Deserialize<'a>>(line: &'a [u8]) -> Option<T> {
    serde_json::from_slice(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let event = Event::OverviewOpened {
            output: "DP-2".into(),
        };
        let line = encode(&event);
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(decode::<Event>(&line[..line.len() - 1]), Some(event));
    }

    #[test]
    fn wire_format_is_readable() {
        assert_eq!(
            encode(&Request::SetOverview { open: false }),
            b"{\"request\":\"set_overview\",\"open\":false}\n"
        );
    }

    #[test]
    fn unknown_messages_are_ignored() {
        assert_eq!(decode::<Event>(b"{\"event\":\"from_the_future\"}"), None);
    }
}
