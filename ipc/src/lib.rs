//! The private compositor <-> shell channel (ARCHITECTURE.md §6): things no
//! Wayland protocol covers, like the overview opening and closing.
//!
//! A Unix socket at `$CNDE_SOCKET`, carrying one JSON object per line. Every
//! connected client receives every [`Event`]; clients send [`Request`]s.
//! Unknown messages are ignored on both sides, so either end can be newer.

use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

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
    /// The answer to [`Request::NewToken`] (sent to the asker alone).
    Token {
        token: String,
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
    /// Which edges of each monitor's free area have a window against them
    /// (maximized, snapped or pushed there): bars there turn solid. Sent
    /// when it changes; nothing touches anything while the overview's open.
    Edges {
        outputs: Vec<OutputEdges>,
    },
    /// The pointer moved onto (`inside`) or off the dock's window preview.
    DockPreviewHover {
        inside: bool,
    },
    /// The dock's window preview went away by itself (a thumbnail was
    /// clicked, or its windows closed).
    DockPreviewGone,
}

/// Which side of its anchor a popup opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Above,
    Below,
    Left,
    Right,
}

/// One notification card, for the frost behind it: its rectangle within the
/// notification stack's surface and how visible it is (0-1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardFrost {
    pub rect: [f64; 4],
    pub alpha: f32,
}

/// One dock icon: its windows and its cell in the dock's surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DockIcon {
    pub windows: Vec<u64>,
    pub cell: [f64; 4],
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OutputEdges {
    pub output: String,
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
    /// The wallpaper along the top is light: a clear bar needs dark text.
    #[serde(default)]
    pub top_light: bool,
    /// With `appearance.adaptive_color`: the colour along the top edge of
    /// the frontmost window against the monitor's top edge, for the bar
    /// (and quick settings, menus) to take, solid.
    #[serde(default)]
    pub top_tint: Option<[u8; 3]>,
    /// The same along the bottom edge of the frontmost window against the
    /// bottom edge, for the dock.
    #[serde(default)]
    pub bottom_tint: Option<[u8; 3]>,
    /// With `appearance.adaptive_color`: the wallpaper's colour, for when no
    /// window is against an edge (bars keep their own translucency).
    #[serde(default)]
    pub wallpaper_tint: Option<[u8; 3]>,
    /// Whether the monitor can do variable refresh (`None`: can't tell, as
    /// in a nested window).
    #[serde(default)]
    pub vrr_capable: Option<bool>,
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
    /// Wants attention (asked to show something while out of sight); clears
    /// when it's focused.
    #[serde(default)]
    pub urgent: bool,
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
    /// Minimize these windows (those on screen; others are left alone).
    MinimizeWindows {
        ids: Vec<u64>,
    },
    /// Show live thumbnails of `windows` beside a dock icon: `cell` is the
    /// icon's (x, y, w, h) within the dock's surface on `output`, and the
    /// panel opens on `side` of the dock.
    DockPreview {
        output: String,
        windows: Vec<u64>,
        cell: [f64; 4],
        side: Side,
    },
    DockPreviewHide,
    /// Where the notification cards are on `output` right now (they animate,
    /// so this is sent as they move), for the frost behind them. Empty: none.
    NotificationFrost {
        output: String,
        cards: Vec<CardFrost>,
    },
    /// Keep the open dock preview up (a click on its icon) until a thumbnail
    /// is picked, Escape, or a click elsewhere.
    DockPreviewPin,
    /// Where the dock's icons are on `output`, so minimizing and restoring
    /// can fly windows to and from them. Each entry is an icon's windows
    /// and its (x, y, w, h) within the dock's surface. Sent when the layout
    /// changes.
    DockIcons {
        output: String,
        icons: Vec<DockIcon>,
    },
    /// Ask this window to close (as its close button would).
    CloseWindow {
        id: u64,
    },
    /// An app was just started: show a busy pointer until a window opens
    /// (or a few seconds pass).
    Launching,
    /// Ask for an activation token to launch an app with
    /// (`XDG_ACTIVATION_TOKEN`), so it may raise itself or the instance it
    /// hands its work to. Answered to the asking connection only, with
    /// [`Event::Token`].
    NewToken,
    /// Lock the screen, as Super+L does.
    LockScreen,
    /// End the session: stop the compositor, as Ctrl+Alt+Backspace does.
    LogOut,
    /// Testing aid, honoured only by debug builds: move the pointer to
    /// (x, y) in the global space, then press or release the left button
    /// (the right one with `right`).
    FakePointer {
        x: f64,
        y: f64,
        #[serde(default)]
        press: Option<bool>,
        #[serde(default)]
        right: bool,
    },
    /// Testing aid, debug builds only: as if this key were pressed.
    FakeMediaKey {
        key: MediaKey,
    },
    /// Debug builds only: run the key binding for keysym `sym` with the
    /// mod key (and Shift, Alt) held, as if pressed.
    FakeKey {
        sym: u32,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        alt: bool,
    },
}

/// The overview while the app grid shows: workspace thumbnails in a row
/// under the search box, the grid below them. Shared so the compositor's
/// thumbnails and the shell's grid fit together.
/// The top bar, as the compositor needs to know it to frost behind it.
pub mod panel {
    /// How far a floating bar sits from the screen's edges, logical px.
    /// Its layer margin is this times how far it has floated (0 to 1), so
    /// the compositor can tell the bar's shape from the margin alone.
    pub const GAP: i32 = 6;
}

/// The dock, as the compositor needs to know it to frost what's behind it.
pub mod dock {
    /// The dock's layer-shell namespace.
    pub const NAMESPACE: &str = "cnde-dock";
    /// Corner radius of a dock that doesn't stretch across its edge.
    pub const RADIUS: f64 = 24.0;
    /// An edge dock's corners square off as its side margins close below
    /// this (logical px, both sides together), so it meets the screen's
    /// ends square; the compositor's frost follows the same rule.
    pub const SQUARE_WITHIN: i32 = 16;

    /// How rounded an edge dock is, 0 to 1, from its side margins.
    pub fn roundness(side_margins: i32) -> f64 {
        (side_margins as f64 / SQUARE_WITHIN as f64).clamp(0.0, 1.0)
    }
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

/// A fresh activation token from the compositor, for something we're about
/// to launch: it may then raise itself, or the running instance it hands
/// its work to. A connection of its own, so the answer comes straight back;
/// `None` if there's no compositor socket or it doesn't answer in time.
pub fn new_token() -> Option<String> {
    let path = std::env::var_os(SOCKET_ENV)?;
    let mut stream = std::os::unix::net::UnixStream::connect(path).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(300)))
        .ok()?;
    stream.write_all(&encode(&Request::NewToken)).ok()?;
    // The compositor greets a new connection with its state; skip to the
    // answer.
    std::io::BufReader::new(stream)
        .split(b'\n')
        .map_while(Result::ok)
        .find_map(|line| match decode::<Event>(&line)? {
            Event::Token { token } => Some(token),
            _ => None,
        })
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
    fn dock_preview_round_trip() {
        let request = Request::DockPreview {
            output: "DP-1".into(),
            windows: vec![3, 4],
            cell: [10.0, 4.0, 56.0, 56.0],
            side: Side::Above,
        };
        let line = encode(&request);
        assert_eq!(decode::<Request>(&line[..line.len() - 1]), Some(request));
    }

    #[test]
    fn unknown_messages_are_ignored() {
        assert_eq!(decode::<Event>(b"{\"event\":\"from_the_future\"}"), None);
    }
}
