//! The clipboard and its file formats: what Ctrl+C offers, what Ctrl+V reads,
//! over the Wayland data device.

use std::{
    ffi::OsString,
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
};

use smithay_client_toolkit::{
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::DataDevice,
        data_source::{CopyPasteSource, DataSourceHandler, DragSource},
    },
    reexports::client::{
        Connection, QueueHandle,
        protocol::{wl_data_device_manager::DndAction, wl_data_source::WlDataSource},
    },
};
use tracing::{debug, warn};

use crate::{App, Event, transfer::Msg};

pub const URI_LIST: &str = "text/uri-list";
pub const GNOME: &str = "x-special/gnome-copied-files";
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// Pasted data bigger than this is ignored.
const MAX_PASTE: usize = 8 << 20;

/// What a source offers: files, or just text.
#[derive(Clone)]
pub enum Payload {
    Files { paths: Vec<PathBuf>, cut: bool },
    Text(String),
}

impl Payload {
    pub fn mimes(&self) -> Vec<&'static str> {
        match self {
            Payload::Files { .. } => {
                let mut m = vec![URI_LIST, GNOME];
                m.push(TEXT_MIMES[0]);
                m.push(TEXT_MIMES[1]);
                m
            }
            Payload::Text(_) => TEXT_MIMES.to_vec(),
        }
    }

    /// The bytes to send for `mime`, if we offer it.
    pub fn bytes(&self, mime: &str) -> Option<Vec<u8>> {
        match self {
            Payload::Files { paths, cut } => match mime {
                URI_LIST => Some(uri_list(paths).into_bytes()),
                GNOME => Some(gnome_copied(paths, *cut).into_bytes()),
                m if TEXT_MIMES.contains(&m) => Some(path_lines(paths)),
                _ => None,
            },
            Payload::Text(t) => TEXT_MIMES.contains(&mime).then(|| t.clone().into_bytes()),
        }
    }
}

fn path_lines(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(p.as_os_str().as_bytes());
    }
    out
}

/// `file://` and the percent-encoded absolute path.
pub fn encode_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `text/uri-list` form: one URI a line, CRLF after each.
pub fn uri_list(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| encode_uri(p) + "\r\n").collect()
}

/// What `x-special/gnome-copied-files` holds: "copy" or "cut", then URIs.
pub fn gnome_copied(paths: &[PathBuf], cut: bool) -> String {
    let uris: Vec<String> = paths.iter().map(|p| encode_uri(p)).collect();
    format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"))
}

/// Bytes of `s` with %XX escapes undone.
pub fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// The local path a `file://` URI names (a host of `localhost` is fine).
pub fn decode_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.trim().strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost")
            .filter(|r| r.starts_with('/'))?
    };
    let path = path.split(['?', '#']).next()?;
    Some(PathBuf::from(OsString::from_vec(percent_decode_bytes(
        path,
    ))))
}

/// The paths in a `text/uri-list` (comments and other schemes skipped).
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(decode_uri)
        .collect()
}

/// `x-special/gnome-copied-files`: (cut, paths).
pub fn parse_gnome(text: &str) -> Option<(bool, Vec<PathBuf>)> {
    let (action, uris) = text.split_once('\n').unwrap_or((text, ""));
    let cut = match action.trim() {
        "cut" => true,
        "copy" => false,
        _ => return None,
    };
    Some((cut, parse_uri_list(uris)))
}

/// Paths from plain text: file:// lines, or absolute paths that exist.
pub fn parse_text(text: &str) -> Vec<PathBuf> {
    let uris = parse_uri_list(text);
    if !uris.is_empty() {
        return uris;
    }
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('/') && Path::new(l).exists())
        .map(PathBuf::from)
        .collect()
}

/// (cut, paths) from data of the given type.
pub fn parse_offer(mime: &str, data: &[u8]) -> (bool, Vec<PathBuf>) {
    let text = String::from_utf8_lossy(data);
    match mime {
        GNOME => parse_gnome(&text).unwrap_or((false, Vec::new())),
        URI_LIST => (false, parse_uri_list(&text)),
        _ => (false, parse_text(&text)),
    }
}

/// The best type to paste files from, among an offer's `mimes`.
pub fn pick_mime(mimes: &[String]) -> Option<String> {
    [
        GNOME,
        URI_LIST,
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
    ]
    .into_iter()
    .find(|want| mimes.iter().any(|m| m == want))
    .map(str::to_string)
}

/// The data device and what we currently offer through it.
pub struct Data {
    pub manager: DataDeviceManagerState,
    pub device: Option<DataDevice>,
    pub selection: Option<(CopyPasteSource, Payload)>,
    pub drag: Option<(DragSource, Payload)>,
}

impl Data {
    /// Whether a `source` is the one we hold.
    fn payload_for(&self, source: &WlDataSource) -> Option<&Payload> {
        if let Some((s, p)) = &self.selection
            && s.inner() == source
        {
            return Some(p);
        }
        if let Some((s, p)) = &self.drag
            && s.inner() == source
        {
            return Some(p);
        }
        None
    }
}

impl DataSourceHandler for App {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        let bytes = self
            .data
            .as_ref()
            .and_then(|d| d.payload_for(source))
            .and_then(|p| p.bytes(&mime));
        let Some(bytes) = bytes else {
            return;
        };
        // A slow reader mustn't hold the window up.
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(OwnedFd::from(fd));
            let _ = file.write_all(&bytes);
        });
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        let Some(data) = &mut self.data else {
            return;
        };
        if data
            .selection
            .as_ref()
            .is_some_and(|(s, _)| s.inner() == source)
        {
            data.selection = None;
        }
        if data.drag.as_ref().is_some_and(|(s, _)| s.inner() == source) {
            data.drag = None;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if let Some(data) = &mut self.data
            && data.drag.as_ref().is_some_and(|(s, _)| s.inner() == source)
        {
            data.drag = None;
        }
        self.refresh_dir();
    }

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl App {
    /// Offer `payload` as the clipboard.
    pub(crate) fn set_clipboard(&mut self, payload: Payload) {
        let serial = self.serial;
        let Some(data) = &mut self.data else {
            self.show_error("The compositor has no clipboard".into());
            return;
        };
        let Some(device) = &data.device else {
            return;
        };
        let source = data
            .manager
            .create_copy_paste_source(&self.qh, payload.mimes());
        source.set_selection(device, serial);
        data.selection = Some((source, payload));
    }

    /// Ctrl+C / Ctrl+X on the selection.
    pub(crate) fn copy_files(&mut self, cut: bool) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        debug!(n = paths.len(), cut, "clipboard offers files");
        self.set_clipboard(Payload::Files { paths, cut });
        let n = self.selected_paths().len();
        let what = if n == 1 {
            "1 item".to_string()
        } else {
            format!("{n} items")
        };
        self.show_notice(format!("{} {what}", if cut { "Cut" } else { "Copied" }));
    }

    /// Ctrl+V: read the clipboard on a thread and paste what it holds.
    pub(crate) fn paste(&mut self) {
        let offer = self
            .data
            .as_ref()
            .and_then(|d| d.device.as_ref())
            .and_then(|d| d.data().selection_offer());
        let Some(offer) = offer else {
            self.show_error("The clipboard is empty".into());
            return;
        };
        let Some(mime) = offer.with_mime_types(pick_mime) else {
            self.show_error("There are no files on the clipboard".into());
            return;
        };
        let pipe = match offer.receive(mime.clone()) {
            Ok(p) => p,
            Err(err) => {
                warn!(?err, "can't read the clipboard");
                self.show_error("Can't read the clipboard".into());
                return;
            }
        };
        // The request must reach the source before it can write.
        let _ = self.conn.flush();
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(OwnedFd::from(pipe));
            let mut data = Vec::new();
            let _ = (&mut file).take(MAX_PASTE as u64).read_to_end(&mut data);
            let _ = tx.send(Event::Xfer(Msg::Pasted(mime, data)));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn encodes_uris() {
        assert_eq!(
            encode_uri(&p("/home/me/a b.txt")),
            "file:///home/me/a%20b.txt"
        );
        assert_eq!(encode_uri(&p("/x/é#%")), "file:///x/%C3%A9%23%25");
        assert_eq!(
            uri_list(&[p("/a"), p("/b c")]),
            "file:///a\r\nfile:///b%20c\r\n"
        );
    }

    #[test]
    fn decodes_uris() {
        assert_eq!(decode_uri("file:///a%20b/%C3%A9"), Some(p("/a b/é")));
        assert_eq!(decode_uri("file://localhost/etc/x"), Some(p("/etc/x")));
        assert_eq!(decode_uri("file://otherhost/etc/x"), None);
        assert_eq!(decode_uri("https://example.com/x"), None);
        assert_eq!(decode_uri("file:///100%"), Some(p("/100%")));
        assert_eq!(decode_uri("file:///a%zzb"), Some(p("/a%zzb")));
        let paths = vec![p("/h/a b"), p("/h/日本語.txt"), p("/h/50%/x#y")];
        assert_eq!(parse_uri_list(&uri_list(&paths)), paths);
    }

    #[test]
    fn parses_lists() {
        let text = "# a comment\r\nfile:///one\r\n\r\nhttp://nope/\r\nfile:///two%20x\r\n";
        assert_eq!(parse_uri_list(text), vec![p("/one"), p("/two x")]);
    }

    #[test]
    fn gnome_format() {
        let paths = vec![p("/a b"), p("/c")];
        let s = gnome_copied(&paths, true);
        assert_eq!(s, "cut\nfile:///a%20b\nfile:///c");
        assert_eq!(parse_gnome(&s), Some((true, paths.clone())));
        assert_eq!(
            parse_gnome("copy\nfile:///c\n"),
            Some((false, vec![p("/c")]))
        );
        assert_eq!(
            parse_gnome("copy\r\nfile:///c\r\n"),
            Some((false, vec![p("/c")]))
        );
        assert_eq!(parse_gnome("junk\nfile:///c"), None);
        assert_eq!(parse_offer(GNOME, s.as_bytes()), (true, paths.clone()));
        assert_eq!(
            parse_offer(URI_LIST, b"file:///c\r\n"),
            (false, vec![p("/c")])
        );
    }

    #[test]
    fn picks_types() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            pick_mime(&m(&["text/uri-list", GNOME])).as_deref(),
            Some(GNOME)
        );
        assert_eq!(pick_mime(&m(&["text/uri-list"])).as_deref(), Some(URI_LIST));
        assert_eq!(pick_mime(&m(&["image/png"])), None);
    }

    #[test]
    fn payload_bytes() {
        let files = Payload::Files {
            paths: vec![p("/a b")],
            cut: false,
        };
        assert_eq!(files.bytes(URI_LIST).unwrap(), b"file:///a%20b\r\n");
        assert_eq!(files.bytes(GNOME).unwrap(), b"copy\nfile:///a%20b");
        assert_eq!(files.bytes("text/plain;charset=utf-8").unwrap(), b"/a b");
        assert!(files.bytes("image/png").is_none());
        let text = Payload::Text("hi".into());
        assert!(text.bytes(URI_LIST).is_none());
        assert_eq!(text.bytes("text/plain").unwrap(), b"hi");
    }
}
