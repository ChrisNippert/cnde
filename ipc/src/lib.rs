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
