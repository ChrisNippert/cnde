//! The compositor end of the private IPC socket (see the `cnde-ipc` crate).
//! Non-blocking throughout: a stuck client is dropped, never waited on.

use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use cnde_ipc::{Event, Request, WindowInfo};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use tracing::{debug, info, warn};

use crate::state::State;

#[derive(Default)]
pub struct IpcClients {
    next_id: u64,
    clients: Vec<(u64, UnixStream)>,
    /// The window list as last sent; `None` forces a resend.
    windows: Option<Vec<WindowInfo>>,
}

impl IpcClients {
    pub fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }
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
        // Everyone gets the window list again; the newcomer needs it.
        self.ipc.windows = None;
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
            Request::ToggleOverview => self.activities_clicked(),
            Request::ShowAppGrid => {
                self.set_overview(true);
                self.ipc.broadcast(&Event::ToggleAppGrid);
            }
            Request::ActivateWindow { id } => self.activate_window(id),
            Request::AppGridShown { shown } => {
                self.overview.set_app_grid(shown);
                self.queue_redraw();
            }
            Request::Screenshot { target } => {
                if self.screenshot.active {
                    self.take_screenshot(target);
                }
            }
            Request::OpenScreenshot => self.start_screenshot(),
            Request::CloseWindow { id } => {
                let window = self
                    .workspaces
                    .list
                    .iter()
                    .flat_map(|ws| &ws.windows)
                    .find(|w| crate::window::with_state(w, |s| s.id) == id);
                if let Some(toplevel) = window.and_then(|w| w.toplevel()) {
                    toplevel.send_close();
                }
            }
            Request::Launching => {
                self.launching = Some(std::time::Instant::now());
                self.queue_redraw();
            }
            Request::CancelScreenshot => {
                if self.screenshot.active {
                    self.cancel_screenshot();
                }
            }
        }
    }

    /// Send the window list if it changed. Runs every loop iteration; cheap
    /// with a handful of windows, and free with no clients.
    pub fn sync_ipc_windows(&mut self) {
        if self.ipc.clients.is_empty() {
            return;
        }
        let focused = self.focused_window();
        let windows: Vec<WindowInfo> = self
            .workspaces
            .list
            .iter()
            .flat_map(|ws| &ws.windows)
            .map(|window| {
                let (title, app_id) = crate::capture::title_and_app_id(window);
                WindowInfo {
                    id: crate::window::with_state(window, |s| s.id),
                    app_id,
                    title,
                    focused: focused.as_ref() == Some(window),
                }
            })
            .collect();
        if self.ipc.windows.as_ref() != Some(&windows) {
            self.ipc.broadcast(&Event::Windows {
                windows: windows.clone(),
            });
            self.ipc.windows = Some(windows);
        }
    }

    /// The dock picked a window: show its workspace (on the main monitor,
    /// unless it's already on screen) and focus it.
    fn activate_window(&mut self, id: u64) {
        let Some((index, window)) = self.workspaces.list.iter().enumerate().find_map(|(i, ws)| {
            ws.windows
                .iter()
                .find(|w| crate::window::with_state(w, |s| s.id) == id)
                .map(|w| (i, w.clone()))
        }) else {
            return;
        };
        let minimized = crate::window::with_state(&window, |s| std::mem::take(&mut s.minimized));
        let visible = self.space.elements().any(|w| w == &window);
        if !visible {
            let current = self
                .workspaces
                .motion
                .as_ref()
                .map_or(self.workspaces.active, |m| m.to) as isize;
            let step = index as isize - current;
            if step != 0 {
                self.step_workspace(step, false);
            } else if minimized {
                // On screen already: put it back where it was.
                self.rebuild_space();
            }
        }
        self.focus_window(Some(&window));
        if self.overview.is_open() {
            self.overview.set_focus_after(window);
            self.set_overview(false);
        }
    }
}
