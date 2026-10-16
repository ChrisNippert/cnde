//! The compositor end of the private IPC socket (see the `cnde-ipc` crate).
//! Non-blocking throughout: a stuck client is dropped, never waited on.

use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use cnde_ipc::{Event, Request};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use tracing::{debug, info, warn};

use crate::state::State;

#[derive(Default)]
pub struct IpcClients {
    next_id: u64,
    clients: Vec<(u64, UnixStream)>,
}

impl IpcClients {
    /// Send `event` to every client, dropping any that can't keep up.
    pub fn broadcast(&mut self, event: &Event) {
        let line = cnde_ipc::encode(event);
        self.clients
            .retain_mut(|(id, stream)| match stream.write_all(&line) {
                Ok(()) => true,
                Err(err) => {
                    debug!(id, %err, "dropping IPC client");
                    false
                }
            });
    }
}

/// Listen on the socket, and export its path for everything we launch.
/// Must run before any client is spawned.
pub fn listen(handle: &LoopHandle<'static, State>) -> Option<PathBuf> {
    let path = cnde_ipc::socket_path(std::process::id())?;
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            warn!(path = %path.display(), %err, "can't create the IPC socket; the shell's search won't work");
            return None;
        }
    };
    if let Err(err) = listener.set_nonblocking(true) {
        warn!(%err, "IPC socket");
        return None;
    }
    let accepting = handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => state.add_ipc_client(stream),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) => {
                        warn!(%err, "IPC accept failed");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    );
    if let Err(err) = accepting {
        warn!(?err, "can't listen for IPC clients");
        return None;
    }
    // Safety: called from main before other threads read the environment.
    unsafe { std::env::set_var(cnde_ipc::SOCKET_ENV, &path) };
    info!(path = %path.display(), "IPC socket ready");
    Some(path)
}

impl State {
    fn add_ipc_client(&mut self, stream: UnixStream) {
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let Ok(writer) = stream.try_clone() else {
            return;
        };
        let id = self.ipc.next_id;
        self.ipc.next_id += 1;
        self.ipc.clients.push((id, writer));
        debug!(id, "IPC client connected");

        let mut pending = Vec::new();
        let inserted = self.loop_handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, state| {
                let mut buf = [0u8; 4096];
                loop {
                    // Safety: we never drop or replace the stream ourselves.
                    match unsafe { stream.get_mut() }.read(&mut buf) {
                        Ok(0) => {
                            state.ipc.clients.retain(|(other, _)| *other != id);
                            return Ok(PostAction::Remove);
                        }
                        Ok(n) => pending.extend_from_slice(&buf[..n]),
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => {
                            state.ipc.clients.retain(|(other, _)| *other != id);
                            return Ok(PostAction::Remove);
                        }
                    }
                }
                while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    match cnde_ipc::decode::<Request>(&line[..line.len() - 1]) {
                        Some(request) => state.handle_ipc_request(request),
                        None => debug!(id, "ignoring unknown IPC message"),
                    }
                }
                // A client that never sends a newline doesn't get to grow
                // this forever.
                if pending.len() > 64 * 1024 {
                    pending.clear();
                }
                Ok(PostAction::Continue)
            },
        );
        if let Err(err) = inserted {
            warn!(?err, "can't read from IPC client");
        }
    }

    fn handle_ipc_request(&mut self, request: Request) {
        debug!(?request, "IPC request");
        match request {
            Request::SetOverview { open } => self.set_overview(open),
            Request::ToggleOverview => self.toggle_overview(),
        }
    }
}
