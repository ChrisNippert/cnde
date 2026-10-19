// Adapted from smithay: smallvil/src/handlers/mod.rs

mod compositor;
mod xdg_shell;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_cursor_shape, delegate_data_device, delegate_dmabuf, delegate_fractional_scale,
    delegate_output, delegate_pointer_constraints, delegate_relative_pointer, delegate_seat,
    delegate_viewporter, delegate_xdg_decoration,
    input::{
        Seat, SeatHandler, SeatState,
        pointer::{CursorImageStatus, PointerHandle},
    },
    utils::{Logical, Point},
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
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

// Surface cropping/scaling. Required by xwayland-satellite, and with
// fractional scale it lets clients draw at the exact pixel size.
delegate_viewporter!(State);

// Tells each client the scale of the monitor it's on (1.25, not just 1 or 2),
// so it can draw sharp instead of being resampled by us.
impl smithay::wayland::fractional_scale::FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        if let Some(output) = self.output_for_root_surface(&root) {
            set_preferred_scale(&surface, &output);
        }
    }
}
delegate_fractional_scale!(State);

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Applies at once if the pointer is already over the surface (and
        // inside its region); otherwise when it gets there (see input.rs).
        if pointer.current_focus().as_ref() != Some(surface) {
            return;
        }
        let location = pointer.current_location();
        let origin = self.surface_under(location).filter(|(s, _)| s == surface);
        with_pointer_constraint(surface, pointer, |constraint| {
            if let (Some(constraint), Some((_, origin))) = (constraint, origin) {
                let point = (location - origin).to_i32_round();
                if constraint.region().is_none_or(|r| r.contains(point)) {
                    constraint.activate();
                }
            }
        });
    }

    /// Where a game wants the pointer left when it unlocks it.
    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active = with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active()));
        if !active {
            return;
        }
        if let Some((_, origin)) = self
            .surface_under(pointer.current_location())
            .filter(|(s, _)| s == surface)
        {
            pointer.set_location(origin + location);
        }
    }
}
delegate_pointer_constraints!(State);
delegate_relative_pointer!(State);

pub(crate) fn set_preferred_scale(surface: &WlSurface, output: &smithay::output::Output) {
    let scale = output.current_scale().fractional_scale();
    smithay::wayland::compositor::with_states(surface, |states| {
        smithay::wayland::fractional_scale::with_fractional_scale(states, |f| {
            f.set_preferred_scale(scale)
        });
    });
}

impl State {
    /// The monitor a toplevel, panel or lock surface is on.
    pub(crate) fn output_for_root_surface(
        &self,
        root: &WlSurface,
    ) -> Option<smithay::output::Output> {
        if let Some(window) = self.window_for_surface(root) {
            return self.output_for(&window);
        }
        self.space
            .outputs()
            .find(|o| {
                smithay::desktop::layer_map_for_output(o)
                    .layer_for_surface(root, smithay::desktop::WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned()
            .or_else(|| self.output_under_pointer())
    }

    /// After monitors change scale or move: re-tell every window and panel.
    pub(crate) fn refresh_fractional_scales(&self) {
        use smithay::wayland::compositor::with_surface_tree_downward;
        let mut roots = Vec::new();
        for window in self.space.elements() {
            if let (Some(t), Some(output)) = (window.toplevel(), self.output_for(window)) {
                roots.push((t.wl_surface().clone(), output));
            }
        }
        for output in self.space.outputs() {
            for layer in smithay::desktop::layer_map_for_output(output).layers() {
                roots.push((layer.wl_surface().clone(), output.clone()));
            }
        }
        for (root, output) in roots {
            with_surface_tree_downward(
                &root,
                (),
                |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                |surface, _, _| set_preferred_scale(surface, &output),
                |_, _, _| true,
            );
        }
    }
}

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
