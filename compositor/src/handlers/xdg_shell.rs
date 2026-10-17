// Adapted from smithay: smallvil/src/handlers/xdg_shell.rs

use smithay::{
    delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space,
        Window, find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::{
        Seat,
        pointer::{Focus, GrabStartData as PointerGrabStartData},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Resource,
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    },
};
use tracing::{debug, warn};

use crate::{state::State, window::Placement};

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Not placed until its first buffer commit (see compositor.rs).
        self.unmapped.push(Window::new_wayland_window(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unmapped
            .retain(|w| w.toplevel().is_some_and(|t| t != &surface));
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.start_close_animation(&window);
            self.unmap_window(&window);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!(?err, "failed to track popup");
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.popup_closed(surface.wl_surface());
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        debug!("client requested interactive move");
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Some(start_data) = check_grab(&seat, surface.wl_surface(), serial) else {
            return;
        };
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.begin_move(window, start_data, serial);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(seat) = Seat::from_resource(&seat) else {
            return;
        };
        let Some(start_data) = check_grab(&seat, surface.wl_surface(), serial) else {
            return;
        };
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.begin_resize(window, start_data, serial, edges.into());
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.window_for_surface(surface.wl_surface()) {
            self.minimize(&window);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.placement_request(&surface, Placement::Maximized);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.placement_request(&surface, Placement::Floating);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        self.placement_request(&surface, Placement::Fullscreen);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.placement_request(&surface, Placement::Floating);
    }

    /// Menus and dropdowns ask for a grab: keyboard and pointer go to the
    /// popup chain, and a click anywhere else dismisses it.
    // Adapted from smithay: anvil/src/shell/xdg.rs
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        if self.window_for_surface(&root).is_none() && !self.is_layer_surface(&root) {
            return;
        }
        // Locked: nothing hidden behind the locker may take the keyboard.
        if self.lock.is_locked() {
            return;
        }
        let mut grab = match self.popups.grab_popup(root, kind, &seat, serial) {
            Ok(grab) => grab,
            Err(err) => {
                debug!(?err, "popup grab refused");
                return;
            }
        };

        // Only grant the grab if it stems from the input that's currently
        // grabbing (the click that opened the menu), not a stale serial.
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}
delegate_xdg_shell!(State);

impl State {
    fn placement_request(&mut self, surface: &ToplevelSurface, target: Placement) {
        debug!(?target, "client requested placement");
        match self.window_for_surface(surface.wl_surface()) {
            Some(window) => self.set_placement(&window, target, true),
            // The protocol requires a configure in reply even if we change nothing.
            None => {
                if surface.is_initial_configure_sent() {
                    surface.send_configure();
                }
            }
        }
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.window_for_surface(&root) else {
            return;
        };
        // Keep menus on the monitor their window is on.
        let Some(output) = self.output_for(&window) else {
            return;
        };
        let (Some(output_geo), Some(window_geo)) = (
            self.space.output_geometry(&output),
            self.space.element_geometry(&window),
        ) else {
            return;
        };

        // The positioner target is relative to the parent's geometry.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

fn check_grab(
    seat: &Seat<State>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<State>> {
    let pointer = seat.get_pointer()?;
    // Only honour moves/resizes started by a real click on this client.
    if !pointer.has_grab(serial) {
        return None;
    }
    let start_data = pointer.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }
    Some(start_data)
}

/// Should be called on `WlSurface::commit`: sends initial configures for
/// toplevels and popups.
pub fn handle_commit(
    popups: &mut PopupManager,
    space: &Space<Window>,
    unmapped: &[Window],
    surface: &WlSurface,
) {
    if let Some(toplevel) = space
        .elements()
        .chain(unmapped)
        .filter_map(|w| w.toplevel())
        .find(|t| t.wl_surface() == surface)
        && !toplevel.is_initial_configure_sent()
    {
        toplevel.send_configure();
    }

    popups.commit(surface);
    if let Some(PopupKind::Xdg(xdg)) = popups.find_popup(surface)
        && !xdg.is_initial_configure_sent()
        && let Err(err) = xdg.send_configure()
    {
        warn!(?err, "initial popup configure failed");
    }
}
