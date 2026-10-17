// Adapted from smithay: smallvil/src/handlers/mod.rs

mod compositor;
mod xdg_shell;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_cursor_shape, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat,
    delegate_viewporter, delegate_xdg_decoration,
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
        tracing::trace!(?image, "cursor image");
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

impl ClientDndGrabHandler for State {
    /// A drop lands on a window (a file into a chat, say): focus it, so you
    /// can type the message that goes with it.
    fn dropped(&mut self, target: Option<WlSurface>, validated: bool, seat: Seat<Self>) {
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        if target.is_none() || !validated {
            return;
        }
        let location = pointer.current_location();
        if let Some((window, _)) = self.space.element_under(location) {
            let window = window.clone();
            self.focus_window(Some(&window));
        }
    }
}
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

// Clients name their cursor (resize arrows, text beam, ...) instead of
// drawing it, so it matches the theme; GTK 4, Qt and cnde-settings use it.
impl smithay::wayland::tablet_manager::TabletSeatHandler for State {}
delegate_cursor_shape!(State);

/// Who draws title bars: see `decorations`.
impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.choose_decoration(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        self.choose_decoration(&toplevel, Some(mode));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.choose_decoration(&toplevel, None);
    }
}
delegate_xdg_decoration!(State);
