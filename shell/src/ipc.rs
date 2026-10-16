//! The shell end of the compositor's private socket (see `cnde-ipc`).
//! Events arrive on a reader thread and are handed to the main loop.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    thread,
};

use cnde_ipc::{Event, Request};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::{debug, warn};

pub struct Ipc {
    stream: Option<UnixStream>,
}

impl Ipc {
    /// Connect to `$CNDE_SOCKET`. Without it (another compositor, or an
    /// old cnde) the shell still runs; there's just no overview search.
    pub fn connect(events: Sender<Event>) -> Self {
        let Some(path) = std::env::var_os(cnde_ipc::SOCKET_ENV) else {
            debug!("no CNDE_SOCKET; overview search disabled");
            return Self { stream: None };
        };
        let stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(err) => {
                warn!(?path, %err, "can't connect to the compositor's IPC socket");
                return Self { stream: None };
            }
        };
        if let Ok(reader) = stream.try_clone() {
            let _ = thread::Builder::new().name("ipc".into()).spawn(move || {
                for line in BufReader::new(reader).split(b'\n').map_while(Result::ok) {
                    if let Some(event) = cnde_ipc::decode::<Event>(&line)
                        && events.send(event).is_err()
                    {
                        return;
                    }
                }
                debug!("IPC connection closed");
            });
        }
        Self {
            stream: Some(stream),
        }
    }

    pub fn send(&mut self, request: &Request) {
        if let Some(stream) = &mut self.stream
            && let Err(err) = stream.write_all(&cnde_ipc::encode(request))
        {
            warn!(%err, "IPC send failed");
            self.stream = None;
        }
    }
}
