// Adapted from smithay: smallvil/src/handlers/mod.rs

mod compositor;
mod xdg_shell;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat, delegate_viewporter,
    delegate_xdg_decoration,
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    wayland::{
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::OutputHandler,
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

    /// Only called for selections we own: the last screenshot.
    fn send_selection(
        &mut self,
        _target: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: smithay::input::Seat<Self>,
        _user_data: &(),
    ) {
        self.send_clipboard(&mime_type, fd);
    }
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
