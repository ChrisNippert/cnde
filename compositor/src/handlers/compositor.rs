// Adapted from smithay: smallvil/src/handlers/compositor.rs

use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor, delegate_shm,
    desktop::Window,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_surface::WlSurface},
        },
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::get_dmabuf,
        shell::xdg::XdgToplevelSurfaceData,
        shm::{ShmHandler, ShmState},
    },
};

use super::xdg_shell;
use crate::{
    grabs::resize_grab,
    state::{ClientState, State},
    window::with_state,
};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor_state
    }

    /// Implicit sync: a client's dmabuf may still be rendering when it
    /// commits. Hold the commit until the GPU is done with it, or our own
    /// frame stalls behind the client's and misses vblank (games stutter).
    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let dmabuf = with_states(surface, |states| {
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                match attrs.pending().buffer.as_ref() {
                    Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).cloned().ok(),
                    _ => None,
                }
            });
            let Some(dmabuf) = dmabuf else { return };
            // Err means it's already ready: nothing to wait for.
            let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };
            let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
                let dh = state.display_handle.clone();
                state
                    .client_compositor_state(&client)
                    .blocker_cleared(state, &dh);
                Ok(())
            });
            if inserted.is_ok() {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // A window that only drew a new frame (same size, still mapped)
        // needs just the monitors it's on redrawn.
        let mut only_window = None;
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            self.capture.surface_committed(&root);
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
                // A client resizing itself (e.g. finishing a maximize) can change
                // what's under the pointer.
                let size = window.geometry().size;
                if with_state(&window, |s| std::mem::replace(&mut s.last_size, size)) != size {
                    self.queue_pointer_refresh();
                } else {
                    only_window = Some(window.clone());
                }
                // A pending maximize/snap animation starts once the client has
                // drawn at the size we asked for.
                let acked = window.toplevel().and_then(|t| {
                    with_states(t.wl_surface(), |states| {
                        states
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .and_then(|data| data.lock().ok()?.current_serial)
                    })
                });
                with_state(&window, |s| {
                    if let Some(resize) = &mut s.resize_anim {
                        resize.on_commit(acked);
                    }
                    if s.pending_size
                        .as_ref()
                        .is_some_and(|p| p.is_resolved(acked, size))
                    {
                        s.pending_size = None;
                    }
                });
            }
        }

        if !self.handle_layer_commit(surface) {
            self.handle_toplevel_mapping(surface);
        }
        self.size_new_tile(surface);
        xdg_shell::handle_commit(&mut self.popups, &self.space, &self.unmapped, surface);
        resize_grab::handle_commit(&mut self.space, surface);
        self.queue_commit_redraw(only_window);
    }
}

impl State {
    fn queue_commit_redraw(&mut self, window: Option<Window>) {
        // Animations, the overview and a grab draw windows away from their
        // own monitor, or over several: redraw them all then.
        let simple = window.filter(|_| {
            !self.overview.is_open()
                && self.workspaces.motion.is_none()
                && !self.seat.get_pointer().is_some_and(|p| p.is_grabbed())
        });
        let outputs = simple
            .map(|w| self.space.outputs_for_element(&w))
            .unwrap_or_default();
        if outputs.is_empty() {
            self.queue_redraw();
        }
        for output in &outputs {
            self.queue_redraw_output(output);
        }
    }

    /// A toplevel becomes visible on its first buffer and hidden again if it
    /// attaches a null buffer.
    fn handle_toplevel_mapping(&mut self, surface: &WlSurface) {
        let has_buffer =
            with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);

        if let Some(window) = self
            .unmapped
            .iter()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
        {
            if has_buffer {
                self.map_new_window(window);
            }
            return;
        }

        if has_buffer {
            return;
        }
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned();
        if let Some(window) = window {
            // Clients often hide a window (null buffer) before destroying it,
            // e.g. Electron apps on quit or closing to the tray. Animate that
            // like a close, from the last frame we drew.
            self.start_close_animation(&window);
            self.unmap_window(&window);
            self.unmapped.push(window);
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(State);
delegate_shm!(State);
