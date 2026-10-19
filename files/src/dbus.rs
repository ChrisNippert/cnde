//! `org.freedesktop.FileManager1` on the session bus, so "Show in folder"
//! from other programs lands here, and a second cnde-files hands its
//! folder to the first instead of opening another window.
//!
//! The first instance also takes `org.cnde.Files` (the freedesktop name
//! may belong to another file manager); that name is the single-instance
//! lock.

use std::path::{Path, PathBuf};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::{debug, warn};
use zbus::{blocking::Connection, interface};

use crate::Event;

const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";
const OWN_NAME: &str = "org.cnde.Files";
const OWN_PATH: &str = "/org/cnde/Files";

/// What the window should do: open a folder, maybe with things selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub dir: PathBuf,
    /// Names in `dir` to select; the first is scrolled into view.
    pub select: Vec<String>,
    /// Show the info panel.
    pub props: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folders,
    Items,
    Properties,
}

/// The local path of a `file://` URI (anything else is no use here).
fn path_of(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An empty or "localhost" host, then the path.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    path.starts_with('/')
        .then(|| PathBuf::from(crate::percent_decode(path)))
}

/// Turn a request's URIs into what to open. Folders opens the first
/// folder (a file stands for its folder); items open the parent of the
/// first one with all its siblings in the list selected.
fn plan(kind: Kind, uris: &[String]) -> Option<Open> {
    let paths: Vec<PathBuf> = uris.iter().filter_map(|u| path_of(u)).collect();
    let first = paths.first()?;
    if kind == Kind::Folders {
        let dir = if first.is_dir() {
            first.clone()
        } else {
            first.parent()?.to_path_buf()
        };
        return Some(Open {
            dir,
            select: Vec::new(),
            props: false,
        });
    }
    let dir = first.parent()?.to_path_buf();
    let select = paths
        .iter()
        .filter(|p| p.parent() == Some(dir.as_path()))
        .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
        .collect();
    Some(Open {
        dir,
        select,
        props: kind == Kind::Properties,
    })
}

struct FileManager {
    tx: Sender<Event>,
}

impl FileManager {
    fn send(&self, kind: Kind, uris: &[String]) {
        match plan(kind, uris) {
            Some(open) => {
                let _ = self.tx.send(Event::Open(open));
            }
            None => debug!(?uris, "nothing to show"),
        }
    }
}

#[interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    fn show_folders(&self, uris: Vec<String>, _startup_id: String) {
        self.send(Kind::Folders, &uris);
    }

    fn show_items(&self, uris: Vec<String>, _startup_id: String) {
        self.send(Kind::Items, &uris);
    }

    fn show_item_properties(&self, uris: Vec<String>, _startup_id: String) {
        self.send(Kind::Properties, &uris);
    }
}

/// Our own name's interface: what a second launch calls.
struct Instance {
    tx: Sender<Event>,
}

#[interface(name = "org.cnde.Files")]
impl Instance {
    fn open(&self, uris: Vec<String>, _startup_id: String) {
        if let Some(open) = plan(Kind::Folders, &uris) {
            let _ = self.tx.send(Event::Open(open));
        }
    }
}

/// Hand `path` to a running cnde-files; true if there was one (the
/// caller then exits).
pub fn forward(path: Option<&Path>) -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    let uris: Vec<String> = path
        .map(|p| vec![crate::thumbs::uri_for(p)])
        .unwrap_or_default();
    let startup = std::env::var("XDG_ACTIVATION_TOKEN").unwrap_or_default();
    conn.call_method(
        Some(OWN_NAME),
        OWN_PATH,
        Some(OWN_NAME),
        "Open",
        &(uris, startup),
    )
    .is_ok()
}

/// Serve both interfaces on a thread of its own. Failing to get a name
/// (another file manager has FileManager1) only loses that name.
pub fn serve(tx: Sender<Event>) {
    let _ = std::thread::Builder::new()
        .name("dbus".into())
        .spawn(move || {
            let built = zbus::blocking::connection::Builder::session()
                .and_then(|b| b.serve_at(FILE_MANAGER_PATH, FileManager { tx: tx.clone() }))
                .and_then(|b| b.serve_at(OWN_PATH, Instance { tx }))
                .and_then(|b| b.build());
            let conn = match built {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(%err, "no session bus; \"show in folder\" won't reach us");
                    return;
                }
            };
            for name in [OWN_NAME, FILE_MANAGER] {
                if let Err(err) = conn.request_name(name) {
                    debug!(name, %err, "didn't get the bus name");
                }
            }
            // The connection answers calls on its own threads; keep it alive.
            loop {
                std::thread::park();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uris(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn uris_become_paths() {
        assert_eq!(
            path_of("file:///home/me/My%20Docs/a.txt"),
            Some(PathBuf::from("/home/me/My Docs/a.txt"))
        );
        assert_eq!(path_of("file://localhost/tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(path_of("http://example.com/a"), None);
        assert_eq!(path_of("file://remote/tmp"), None);
    }

    #[test]
    fn items_select_in_their_folder() {
        let open = plan(
            Kind::Items,
            &uris(&[
                "file:///home/me/a%20b.txt",
                "file:///home/me/c.txt",
                "file:///elsewhere/d.txt",
            ]),
        )
        .unwrap();
        assert_eq!(open.dir, Path::new("/home/me"));
        assert_eq!(open.select, ["a b.txt", "c.txt"]);
        assert!(!open.props);
        let open = plan(Kind::Properties, &uris(&["file:///home/me/x"])).unwrap();
        assert!(open.props);
        assert_eq!(open.select, ["x"]);
    }

    #[test]
    fn folders_open_the_folder() {
        let open = plan(Kind::Folders, &uris(&["file:///tmp"])).unwrap();
        assert_eq!(open.dir, Path::new("/tmp"));
        assert!(open.select.is_empty());
        // A file that isn't there stands for its folder.
        let open = plan(Kind::Folders, &uris(&["file:///tmp/nope-cnde.txt"])).unwrap();
        assert_eq!(open.dir, Path::new("/tmp"));
        assert!(plan(Kind::Folders, &[]).is_none());
    }

    /// Under `dbus-run-session -- cargo test -p cnde-files bus_round_trip -- --ignored`.
    #[test]
    #[ignore]
    fn bus_round_trip() {
        use smithay_client_toolkit::reexports::calloop::channel;
        let (ctx, crx) = channel::channel::<Event>();
        assert!(!forward(Some(Path::new("/tmp"))), "nobody serves yet");
        serve(ctx);
        let mut forwarded = false;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            forwarded = forward(Some(Path::new("/tmp")));
            if forwarded {
                break;
            }
        }
        assert!(forwarded);
        std::thread::sleep(std::time::Duration::from_millis(100));
        match crx.try_recv() {
            Ok(Event::Open(open)) => assert_eq!(open.dir, Path::new("/tmp")),
            _ => panic!("no request arrived"),
        }
    }
}
