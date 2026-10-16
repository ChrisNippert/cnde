// Adapted from smithay: smallvil/src/handlers/mod.rs

mod compositor;
mod xdg_shell;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_data_device, delegate_dmabuf, delegate_output, delegate_pointer_constraints,
    delegate_relative_pointer, delegate_seat, delegate_viewporter, delegate_xdg_decoration,
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Point},
    wayland::{
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::OutputHandler,
        pointer_constraints::{PointerConstraintsHandler, with_pointer_constraint},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
    },
};

use crate::state::State;

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        if self.backend.draws_cursor() {
            self.queue_redraw();
        }
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}
delegate_seat!(State);

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}
delegate_data_device!(State);

impl OutputHandler for State {}
delegate_output!(State);

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.backend.dmabuf_state()
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.backend.import_dmabuf(&dmabuf) {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}
delegate_dmabuf!(State);

// Surface cropping/scaling. Required by xwayland-satellite, and the basis
// for fractional scaling later.
delegate_viewporter!(State);

// Raw mouse deltas for games and 3D apps; events are sent from input.rs.
delegate_relative_pointer!(State);

/// Pointer lock (mouse-look) and confinement; enforced in input.rs.
impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Activate right away if the pointer is already over that surface;
        // otherwise on_pointer_motion does it when the pointer gets there.
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    /// Where a game wants the cursor when it releases a lock (e.g. back
    /// where the crosshair was), relative to its surface.
    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active =
            with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active()));
        let Some(window) = self.window_for_surface(surface).filter(|_| active) else {
            return;
        };
        let Some(window_loc) = self.space.element_location(&window) else {
            return;
        };
        // The surface origin, not the window geometry (CSD shadows).
        let origin = (window_loc - window.geometry().loc).to_f64();
        pointer.set_location(origin + location);
    }
}
delegate_pointer_constraints!(State);

/// We don't draw server-side titlebars yet (ARCHITECTURE.md §4.4), so every
/// client is told to decorate itself.
impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_client_side(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        set_client_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_client_side(&toplevel);
    }
}
delegate_xdg_decoration!(State);

fn set_client_side(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| state.decoration_mode = Some(DecorationMode::ClientSide));
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}
