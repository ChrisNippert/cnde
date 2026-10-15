// Adapted from smithay: smallvil/src/handlers/compositor.rs

use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor, delegate_shm,
    reexports::wayland_server::{
        Client,
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
        },
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

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
                // A client resizing itself (e.g. finishing a maximize) can change
                // what's under the pointer.
                let size = window.geometry().size;
                if with_state(&window, |s| std::mem::replace(&mut s.last_size, size)) != size {
                    self.queue_pointer_refresh();
                }
            }
        }

        self.handle_toplevel_mapping(surface);
        xdg_shell::handle_commit(&mut self.popups, &self.space, &self.unmapped, surface);
        resize_grab::handle_commit(&mut self.space, surface);
        self.queue_redraw();
    }
}

impl State {
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
