//! One Settings window: the first instance owns `org.cnde.Settings`, and a
//! second launch hands it the page it was asked for and exits.

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::{debug, warn};
use zbus::{blocking::Connection, interface};

use crate::pages::Page;

const OWN_NAME: &str = "org.cnde.Settings";
const OWN_PATH: &str = "/org/cnde/Settings";

/// What the window should do: switch page (`None`: stay) and come forward
/// with this activation token (maybe empty).
pub struct Show {
    pub page: Option<Page>,
    pub token: String,
}

struct Instance {
    tx: Sender<Show>,
}

#[interface(name = "org.cnde.Settings")]
impl Instance {
    fn show(&self, page: String, startup_id: String) {
        let page = (!page.is_empty())
            .then(|| {
                let found = Page::from_name(&page);
                if found.is_none() {
                    warn!(page, "no such page");
                }
                found
            })
            .flatten();
        let _ = self.tx.send(Show {
            page,
            token: startup_id,
        });
    }
}

/// Hand `page` to a running Settings; true if there was one (the caller
/// then exits).
pub fn forward(page: Option<&str>) -> bool {
    let Ok(conn) = Connection::session() else {
        return false;
    };
    let startup = std::env::var("XDG_ACTIVATION_TOKEN").unwrap_or_default();
    conn.call_method(
        Some(OWN_NAME),
        OWN_PATH,
        Some(OWN_NAME),
        "Show",
        &(page.unwrap_or_default(), startup),
    )
    .is_ok()
}

/// Take the name and serve `Show` on a thread of its own. Without a bus
/// there's just no single-instance behaviour.
pub fn serve(tx: Sender<Show>) {
    let _ = std::thread::Builder::new()
        .name("dbus".into())
        .spawn(move || {
            let built = zbus::blocking::connection::Builder::session()
                .and_then(|b| b.name(OWN_NAME))
                .and_then(|b| b.serve_at(OWN_PATH, Instance { tx }))
                .and_then(|b| b.build());
            let _conn = match built {
                Ok(conn) => conn,
                Err(err) => {
                    debug!(%err, "not serving {OWN_NAME}");
                    return;
                }
            };
            // The connection answers calls on its own threads; keep it alive.
            loop {
                std::thread::park();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a session bus of its own: run under `dbus-run-session`.
    #[test]
    #[ignore]
    fn bus_round_trip() {
        use smithay_client_toolkit::reexports::calloop::channel;
        let (tx, rx) = channel::channel::<Show>();
        assert!(!forward(Some("sound")), "nobody serves yet");
        serve(tx);
        let mut forwarded = false;
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            forwarded = forward(Some("sound"));
            if forwarded {
                break;
            }
        }
        assert!(forwarded);
        std::thread::sleep(std::time::Duration::from_millis(100));
        match rx.try_recv() {
            Ok(show) => assert_eq!(show.page, Some(Page::Sound)),
            _ => panic!("no request arrived"),
        }
    }
}
