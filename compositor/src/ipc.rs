//! The compositor end of the private IPC socket (see the `cnde-ipc` crate).
//! Non-blocking throughout: a stuck client is dropped, never waited on.

use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use cnde_ipc::{Event, OutputEdges, Request, WindowInfo};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use tracing::{debug, info, warn};

use crate::state::State;

#[derive(Default)]
pub struct IpcClients {
    next_id: u64,
    clients: Vec<(u64, UnixStream)>,
    /// The window list as last sent; `None` forces a resend.
    windows: Option<Vec<WindowInfo>>,
    /// The edges as last sent; `None` forces a resend.
    edges: Option<Vec<OutputEdges>>,
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
        self.ipc.edges = None;
        debug!(id, "IPC client connected");

        let mut pending = Vec::new();
        let inserted = self.loop_handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, state| {
                let mut buf = [0u8; 4096];
                // One-off clients (the shell's Lock and Log Out) write a
                // request and hang up at once: what they sent still counts.
                let mut closed = false;
                loop {
                    // Safety: we never drop or replace the stream ourselves.
                    match unsafe { stream.get_mut() }.read(&mut buf) {
                        Ok(0) => {
                            closed = true;
                            break;
                        }
                        Ok(n) => pending.extend_from_slice(&buf[..n]),
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => {
                            closed = true;
                            break;
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
                if closed {
                    state.ipc.clients.retain(|(other, _)| *other != id);
                    return Ok(PostAction::Remove);
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
        // Locked: nothing the shell asks for may show or change anything.
        if self.lock.is_locked()
            && !matches!(
                request,
                Request::FakePointer { .. }
                    | Request::FakeMediaKey { .. }
                    | Request::FakeKey { .. }
            )
        {
            return;
        }
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
            Request::LockScreen => self.lock_screen(),
            Request::LogOut => self.loop_signal.stop(),
            Request::Launching => {
                self.launching = Some(std::time::Instant::now());
                self.queue_redraw();
            }
            Request::FakePointer { x, y, press, right } if cfg!(debug_assertions) => {
                self.fake_pointer((x, y).into(), press, right)
            }
            Request::FakePointer { .. } => {}
            Request::FakeMediaKey { key } if cfg!(debug_assertions) => {
                self.ipc.broadcast(&Event::MediaKey { key })
            }
            Request::FakeMediaKey { .. } => {}
            Request::FakeKey { sym, shift, alt } if cfg!(debug_assertions) => {
                self.fake_key(sym, shift, alt)
            }
            Request::FakeKey { .. } => {}
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

    /// Send which monitor edges windows touch, if that changed.
    pub fn sync_ipc_edges(&mut self) {
        if self.ipc.clients.is_empty() {
            return;
        }
        let open = self.overview.is_open() || self.lock.is_locked();
        let edges: Vec<OutputEdges> = self
            .space
            .outputs()
            .map(|output| {
                let mut edges = OutputEdges {
                    output: output.name(),
                    top_light: self.wallpapers.top_light(&output.name()),
                    // Kept while the overview's open, so nothing flashes.
                    wallpaper_tint: self
                        .wallpapers
                        .tint(&output.name())
                        .filter(|_| self.config.appearance.adaptive()),
                    ..Default::default()
                };
                let (Some(geo), false) = (self.space.output_geometry(output), open) else {
                    return edges;
                };
                let touching = self.touching_edges(output, geo);
                edges.top = touching.top.is_some();
                edges.bottom = touching.bottom.is_some();
                edges.left = touching.left;
                edges.right = touching.right;
                let sample = |w: &Option<smithay::desktop::Window>| {
                    w.as_ref().and_then(|w| self.tints.get(w))
                };
                edges.top_tint = sample(&touching.top).map(|s| s.top);
                edges.bottom_tint = sample(&touching.bottom).map(|s| s.bottom);
                edges
            })
            .collect();
        if self.ipc.edges.as_ref() != Some(&edges) {
            self.ipc.broadcast(&Event::Edges {
                outputs: edges.clone(),
            });
            self.ipc.edges = Some(edges);
        }
    }

    /// Which edges of `output`'s free area windows are against, and the
    /// frontmost window at the top and bottom ones.
    pub fn touching_edges(
        &self,
        output: &smithay::output::Output,
        geo: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
    ) -> Touching {
        let zone = smithay::desktop::layer_map_for_output(output).non_exclusive_zone();
        let zone = smithay::utils::Rectangle::new(zone.loc + geo.loc, zone.size);
        let mut touching = Touching::default();
        // Bottom to top, so the last one found is in front.
        for window in self.space.elements_for_output(output) {
            let Some(mut rect) = self.space.element_geometry(window) else {
                continue;
            };
            if crate::window::with_state(window, |s| s.minimized) {
                continue;
            }
            // A new tile's committed size lags its configure (it
            // maps at the client's own, maybe full-screen, size), which
            // would flash the bars solid; judge tiles by their slot.
            if self.is_tiled(window)
                && !self.workspaces.is_moving()
                && let Some(slot) = self.tile_rect_now(window)
            {
                rect = slot;
            }
            // A server-drawn titlebar sits above the geometry.
            let top = rect.loc.y - crate::decorations::titlebar_height(window);
            let overlaps_x =
                rect.loc.x < zone.loc.x + zone.size.w && rect.loc.x + rect.size.w > zone.loc.x;
            let overlaps_y =
                top < zone.loc.y + zone.size.h && rect.loc.y + rect.size.h > zone.loc.y;
            if overlaps_x && top <= zone.loc.y {
                touching.top = Some(window.clone());
            }
            if overlaps_x && rect.loc.y + rect.size.h >= zone.loc.y + zone.size.h {
                touching.bottom = Some(window.clone());
            }
            touching.left |= overlaps_y && rect.loc.x <= zone.loc.x;
            touching.right |= overlaps_y && rect.loc.x + rect.size.w >= zone.loc.x + zone.size.w;
        }
        touching
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
            } else if minimized && self.is_tiled(&window) {
                // Back into its tile; the others make room.
                self.relayout_visible(true);
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

/// See `State::touching_edges`.
#[derive(Default)]
pub struct Touching {
    pub top: Option<smithay::desktop::Window>,
    pub bottom: Option<smithay::desktop::Window>,
    pub left: bool,
    pub right: bool,
}
