// Adapted from smithay: smallvil/src/input.rs

use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent,
        GestureEndEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    desktop::Window,
    input::keyboard::keysyms,
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size},
};

use tracing::debug;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::RegionAttributes;
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};

use crate::{
    grabs::{MoveSurfaceGrab, ResizeEdge, ResizeSurfaceGrab, SnapPairResizeGrab, TileResizeGrab},
    keybinds::{self, Action, Direction, ModKey},
    spawn,
    state::State,
    window::{Placement, with_state},
};

pub(crate) const BTN_LEFT: u32 = 0x110;
/// Size of a restored window that never had a floating size, as a fraction
/// of the usable area.
const DEFAULT_FLOATING_FRACTION: f64 = 0.6;
const BTN_RIGHT: u32 = 0x111;

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        crate::perf::time("input", || self.handle_input_event(event));
    }

    fn handle_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Anything the user does; waking a dark screen with a click
        // shouldn't also press whatever is under the pointer.
        let woke = self.idle_activity();
        if woke
            && matches!(
                event,
                InputEvent::PointerButton { .. } | InputEvent::PointerAxis { .. }
            )
        {
            return;
        }
        match event {
            InputEvent::Keyboard { event, .. } => self.on_keyboard::<I>(event),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next().cloned() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(&output) else {
                    return;
                };
                let location =
                    event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.on_pointer_motion(location, event.time_msec());
            }
            InputEvent::PointerMotion { event, .. } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let current = pointer.current_location();
                let under = self.surface_under(current);
                let use_region = under.as_ref().is_none_or(|(s, _)| self.confine_region_applies(s));
                let constraint = active_constraint(&pointer, current, under.as_ref(), use_region);
                // Unaccelerated deltas for games and 3D apps (relative-pointer).
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                let location = self.clamp_to_outputs(current, current + event.delta());
                let stay = match &constraint {
                    // Locked: the game turns its camera from the deltas
                    // alone; the pointer itself doesn't move.
                    Some(Constraint::Locked) => true,
                    // Confined: not off the surface, nor out of its region.
                    Some(Constraint::Confined(region)) => {
                        let (surface, origin) = under.as_ref().expect("a constraint has a surface");
                        let new_under = self.surface_under(location);
                        new_under.as_ref().map(|(s, _)| s) != Some(surface)
                            || region.as_ref().filter(|_| use_region).is_some_and(|r| {
                                !r.contains((location - *origin).to_i32_round())
                            })
                    }
                    None => false,
                };
                if stay {
                    pointer.frame(self);
                    return;
                }
                self.on_pointer_motion(location, event.time_msec());
                self.activate_constraint(&pointer, location);
            }
            InputEvent::PointerButton { event, .. } => {
                self.on_pointer_button(event.button_code(), event.state(), event.time_msec());
            }
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(event),
            // Three-finger swipes: sideways drives the workspace strip, up
            // and down the overview. The direction is picked once the
            // fingers have moved a little.
            // TODO: forward other gestures to clients (pointer-gestures).
            InputEvent::GestureSwipeBegin { event, .. } => {
                if event.fingers() == 3 && !self.lock.is_locked() {
                    self.swipe_active = true;
                    self.swipe_sideways = None;
                    self.swipe_pending = (0.0, 0.0);
                }
            }
            InputEvent::GestureSwipeUpdate { event, .. } => {
                if self.swipe_active {
                    self.swipe_moved(event.delta_x(), event.delta_y());
                }
            }
            InputEvent::GestureSwipeEnd { event, .. } if self.swipe_active => {
                self.swipe_active = false;
                match self.swipe_sideways.take() {
                    Some(true) => self.swipe_end(event.cancelled()),
                    Some(false) => self.overview_swipe_end(event.cancelled()),
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn on_keyboard<I: InputBackend>(&mut self, event: I::KeyboardKeyEvent) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let keycode = event.key_code();
        let key_state = event.state();
        let mod_key = self.mod_key;

        let action = keyboard.input::<Option<Action>, _>(
            self,
            keycode,
            key_state,
            SERIAL_COUNTER.next_serial(),
            event.time_msec(),
            |state, mods, handle| {
                let is_mod = keybinds::is_mod_key(mod_key, handle.modified_sym());
                if key_state == KeyState::Released && is_mod && state.mod_tap_armed {
                    state.mod_tap_armed = false;
                    // Forward the release: clients saw the press.
                    state.mod_tapped = true;
                    return FilterResult::Forward;
                }
                if key_state == KeyState::Pressed {
                    state.mod_tap_armed = is_mod;
                }
                if key_state == KeyState::Released {
                    if state
                        .media_repeat
                        .as_ref()
                        .is_some_and(|(k, _)| *k == keycode)
                        && let Some((_, timer)) = state.media_repeat.take()
                    {
                        state.loop_handle.remove(timer);
                    }
                    if let Some(i) = state.suppressed_keys.iter().position(|k| *k == keycode) {
                        state.suppressed_keys.swap_remove(i);
                        return FilterResult::Intercept(None);
                    }
                    return FilterResult::Forward;
                }
                // VT switching comes from the keymap (Ctrl+Alt+Fn produces
                // XF86Switch_VT_n), so it's matched on the modified keysym.
                if let Some(action) = keybinds::vt_switch(handle.modified_sym()) {
                    state.suppressed_keys.push(keycode);
                    return FilterResult::Intercept(Some(action));
                }
                // Volume and brightness keys work whatever's focused, held
                // ones repeating.
                if let Some(key) = keybinds::media_key(handle.modified_sym()) {
                    state.suppressed_keys.push(keycode);
                    // Toggles don't repeat: a release held up past the
                    // delay (a busy frame, a wireless keyboard) would
                    // toggle twice and look like nothing happened.
                    if key.repeats() {
                        state.start_media_repeat(keycode, key);
                    }
                    return FilterResult::Intercept(Some(Action::Media(key)));
                }
                // Locked: every other key is the locker's (no bindings,
                // not even quitting, which would drop to a logged-in TTY).
                if state.lock.is_locked() {
                    state.mod_tap_armed = false;
                    return FilterResult::Forward;
                }
                let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() else {
                    return FilterResult::Forward;
                };
                // Escape always ends a screenshot, even when the shell's
                // tool never showed up to take the key.
                if sym.raw() == keysyms::KEY_Escape && state.screenshot.active {
                    state.suppressed_keys.push(keycode);
                    return FilterResult::Intercept(Some(Action::CancelScreenshot));
                }
                // Escape puts away a dock preview opened by a click.
                if sym.raw() == keysyms::KEY_Escape && state.preview_pinned() {
                    state.suppressed_keys.push(keycode);
                    state.dock_preview_dismiss();
                    return FilterResult::Intercept(None);
                }
                // Escape leaves the overview, unless the shell's search box
                // has the keyboard (it decides: clear the text, then close).
                if sym.raw() == keysyms::KEY_Escape
                    && state.overview.is_open()
                    && state.exclusive_layer().is_none()
                {
                    state.suppressed_keys.push(keycode);
                    return FilterResult::Intercept(Some(Action::CloseOverview));
                }
                // The switcher: Escape cancels, arrows step.
                if state.switcher.is_open() {
                    let step = match sym.raw() {
                        keysyms::KEY_Escape => Some(Action::CancelSwitcher),
                        keysyms::KEY_Left => Some(Action::Switcher { back: true }),
                        keysyms::KEY_Right => Some(Action::Switcher { back: false }),
                        _ => None,
                    };
                    if step.is_some() {
                        state.suppressed_keys.push(keycode);
                        return FilterResult::Intercept(step);
                    }
                }
                match keybinds::action_for(&state.shortcuts, mods, sym) {
                    Some(action) => {
                        state.suppressed_keys.push(keycode);
                        FilterResult::Intercept(Some(action))
                    }
                    None => FilterResult::Forward,
                }
            },
        );

        if let Some(Some(action)) = action {
            self.run_action(action);
        }
        // Letting go of the modifier that opened the switcher picks.
        if let Some(held) = self.switcher.held() {
            let mods = keyboard.modifier_state();
            let still = match held {
                crate::switcher::Held::Alt => mods.alt,
                crate::switcher::Held::Logo => mods.logo,
            };
            if !still {
                self.switcher_commit();
            }
        }
        if std::mem::take(&mut self.mod_tapped) {
            self.run_action(Action::Launcher);
        }
    }

    /// Send a held media key again, after a pause, until it's released.
    fn start_media_repeat(
        &mut self,
        keycode: smithay::input::keyboard::Keycode,
        key: cnde_ipc::MediaKey,
    ) {
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        if let Some((_, timer)) = self.media_repeat.take() {
            self.loop_handle.remove(timer);
        }
        let timer = self.loop_handle.insert_source(
            Timer::from_duration(std::time::Duration::from_millis(400)),
            move |_, _, state| {
                state.run_action(Action::Media(key));
                TimeoutAction::ToDuration(std::time::Duration::from_millis(90))
            },
        );
        if let Ok(timer) = timer {
            self.media_repeat = Some((keycode, timer));
        }
    }

    /// A three-finger swipe moved by (`dx`, `dy`).
    fn swipe_moved(&mut self, dx: f64, dy: f64) {
        /// Travel before a swipe commits to a direction.
        const DECIDE: f64 = 12.0;
        match self.swipe_sideways {
            Some(true) => self.swipe_update(dx),
            Some(false) => self.overview_swipe_update(dy),
            None => {
                let (px, py) = (self.swipe_pending.0 + dx, self.swipe_pending.1 + dy);
                self.swipe_pending = (px, py);
                if px.hypot(py) < DECIDE {
                    return;
                }
                let sideways = px.abs() > py.abs();
                self.swipe_sideways = Some(sideways);
                if sideways {
                    self.swipe_begin();
                    self.swipe_update(px);
                } else if py < 0.0 || self.overview.is_open() {
                    self.overview_swipe_begin();
                    self.overview_swipe_update(py);
                }
                // (Down with the overview closed: nothing to do.)
            }
        }
    }

    /// Debug aid (IPC `fake_key`): the binding for `sym` with the mod key
    /// held, run as if pressed.
    pub fn fake_key(&mut self, sym: u32, shift: bool, alt: bool) {
        let mut mods = smithay::input::keyboard::ModifiersState {
            shift,
            ..Default::default()
        };
        match self.mod_key {
            ModKey::Super => {
                mods.logo = true;
                mods.alt = alt;
            }
            ModKey::Alt => {
                mods.alt = true;
                mods.ctrl = alt;
            }
        }
        let sym = smithay::input::keyboard::Keysym::from(sym);
        // With the switcher up (nothing to release): Return picks, Escape cancels.
        if self.switcher.is_open() {
            match sym.raw() {
                keysyms::KEY_Return => self.switcher_commit(),
                keysyms::KEY_Escape => self.switcher_cancel(),
                _ => {}
            }
            if sym.raw() != keysyms::KEY_Tab {
                return;
            }
        }
        if let Some(action) = keybinds::action_for(&self.shortcuts, &mods, sym) {
            self.run_action(action);
        }
    }

    fn run_action(&mut self, action: Action) {
        if self.lock.is_locked() && !matches!(action, Action::Media(_) | Action::SwitchVt(_)) {
            return;
        }
        // Framing a screenshot: the tool owns the keyboard.
        if self.screenshot.active
            && !matches!(
                action,
                Action::Media(_) | Action::Quit | Action::SwitchVt(_) | Action::CancelScreenshot
            )
        {
            return;
        }
        match action {
            Action::Switcher { back } => {
                let held = self.seat.get_keyboard().and_then(|k| {
                    let mods = k.modifier_state();
                    if mods.alt {
                        Some(crate::switcher::Held::Alt)
                    } else if mods.logo {
                        Some(crate::switcher::Held::Logo)
                    } else {
                        None
                    }
                });
                self.switcher_step(back, held);
            }
            Action::CancelSwitcher => self.switcher_cancel(),
            Action::Lock => self.lock_screen(),
            Action::Quit => self.loop_signal.stop(),
            Action::SpawnTerminal => spawn::spawn_terminal(&self.client_env(), &self.config.apps.terminal),
            Action::SpawnBrowser => spawn::spawn_browser(&self.client_env()),
            Action::SpawnFiles => spawn::spawn_files(&self.client_env()),
            Action::SpawnSystemMonitor => {
                let command = self.config.dock.system_monitor.trim().to_string();
                if !command.is_empty() {
                    spawn::spawn_command_line(&command, &self.client_env());
                }
            }
            Action::Custom(index) => {
                if let Some(command) = self.shortcuts.command(index) {
                    spawn::spawn_command_line(command, &self.client_env());
                }
            }
            Action::ShowDesktop => self.toggle_show_desktop(),
            Action::CloseWindow => self.close_focused(),
            // On a tiling workspace the arrows move focus between tiles.
            Action::Maximize if self.tiling_here() => self.focus_direction(Direction::Up),
            Action::Restore if self.tiling_here() => self.focus_direction(Direction::Down),
            Action::SnapLeft if self.tiling_here() => self.focus_direction(Direction::Left),
            Action::SnapRight if self.tiling_here() => self.focus_direction(Direction::Right),
            Action::Maximize => self.set_focused_placement(Placement::Maximized),
            Action::Restore => self.set_focused_placement(Placement::Floating),
            Action::SnapLeft => self.set_focused_placement(Placement::SnappedLeft),
            Action::SnapRight => self.set_focused_placement(Placement::SnappedRight),
            Action::MoveTile(direction) => self.move_tile(direction),
            Action::ToggleFullscreen => self.toggle_focused_fullscreen(),
            Action::ToggleTiling => self.toggle_tiling(),
            Action::ToggleFloating => self.toggle_focused_floating(),
            Action::SwapMaster => self.swap_with_master(),
            Action::MasterRatio(step) => {
                self.adjust_master_ratio(f64::from(step) * crate::layout::RATIO_STEP)
            }
            Action::SwitchVt(vt) => self.switch_vt(vt),
            Action::Workspace { step, carry } => {
                let step = if self.config.input.keyboard.natural_workspace_keys {
                    -step
                } else {
                    step
                };
                self.step_workspace(step, carry)
            }
            Action::Media(key) => self.ipc.broadcast(&cnde_ipc::Event::MediaKey { key }),
            Action::Screenshot => self.start_screenshot(),
            Action::Launcher => {
                let command = self.config.launcher.command.clone();
                if command.trim().is_empty() {
                    self.super_tapped();
                } else {
                    spawn::spawn_command_line(&command, &self.client_env());
                }
            }
            Action::CloseOverview => self.set_overview(false),
            Action::CancelScreenshot => self.cancel_screenshot(),
            Action::AppGrid => {
                self.set_overview(true);
                self.ipc.broadcast(&cnde_ipc::Event::ToggleAppGrid);
            }
        }
    }

    /// Keep the pointer on screen: moves into another monitor are allowed,
    /// otherwise it stays on its current monitor, pinned to the edge.
    fn clamp_to_outputs(
        &self,
        current: Point<f64, Logical>,
        target: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        if self.space.output_under(target).next().is_some() {
            return target;
        }
        let geo = self
            .space
            .output_under(current)
            .next()
            .or_else(|| self.space.outputs().next())
            .and_then(|output| self.space.output_geometry(output));
        let Some(geo) = geo else {
            return current;
        };
        let geo = geo.to_f64();
        // Stay strictly inside: the right/bottom edges belong to the next pixel.
        let max_x = geo.loc.x + geo.size.w - 1.0;
        let max_y = geo.loc.y + geo.size.h - 1.0;
        Point::from((
            target.x.clamp(geo.loc.x, max_x),
            target.y.clamp(geo.loc.y, max_y),
        ))
    }

    fn mod_held(&self) -> bool {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let mods = keyboard.modifier_state();
        match self.mod_key {
            ModKey::Super => mods.logo,
            ModKey::Alt => mods.alt,
        }
    }

    /// Debug builds' `FakePointer` IPC request, for driving nested tests.
    pub fn fake_pointer(&mut self, location: Point<f64, Logical>, press: Option<bool>, right: bool) {
        let time = self.start_time.elapsed().as_millis() as u32;
        self.on_pointer_motion(location, time);
        if let Some(press) = press {
            let state = if press {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            };
            let button = if right { BTN_RIGHT } else { BTN_LEFT };
            self.on_pointer_button(button, state, time);
        }
    }

    /// The pointer arrived over a surface with a constraint waiting (a game
    /// asked to lock or confine it): apply it, if inside its region.
    fn activate_constraint(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<State>,
        location: Point<f64, Logical>,
    ) {
        let Some((surface, origin)) = self.surface_under(location) else {
            return;
        };
        if !self.keyboard_on_window(&surface) {
            return;
        }
        let use_region = self.confine_region_applies(&surface);
        with_pointer_constraint(&surface, pointer, |constraint| {
            if let Some(constraint) = constraint
                && !constraint.is_active()
            {
                let point = (location - origin).to_i32_round();
                if !use_region || constraint.region().is_none_or(|r| r.contains(point)) {
                    constraint.activate();
                }
            }
        });
    }

    /// Whether a lock or confinement on `surface` keeps to the region the
    /// client gave. Not for a fullscreen window: a game drawing at a lower
    /// resolution, scaled up to the monitor, can give its region in its own
    /// unscaled pixels (Halo's covered the top-left 1920x1080 of a 2560x1440
    /// screen), and a fullscreen game only ever means its whole window.
    pub(crate) fn confine_region_applies(&self, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        !self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
            .is_some_and(|w| Self::placement_of(w) == crate::window::Placement::Fullscreen)
    }

    /// Whether the keyboard is with `surface`'s window (a game's subsurface
    /// counts as its toplevel): only the focused window may lock or confine
    /// the pointer, not one merely under it. By surface, not client, since
    /// every X11 window shares xwayland-satellite's connection.
    pub(crate) fn keyboard_on_window(&self, surface: &WlSurface) -> bool {
        let root = |mut s: WlSurface| {
            while let Some(parent) = smithay::wayland::compositor::get_parent(&s) {
                s = parent;
            }
            s
        };
        self.seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|focus| root(focus) == root(surface.clone()))
    }

    /// Focus moved: let go of a lock or confinement held by the pointer's
    /// surface if its app no longer has the keyboard.
    pub(crate) fn release_unfocused_constraint(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(surface) = pointer.current_focus() else {
            return;
        };
        if self.keyboard_on_window(&surface) {
            return;
        }
        with_pointer_constraint(&surface, &pointer, |constraint| {
            if let Some(constraint) = constraint
                && constraint.is_active()
            {
                constraint.deactivate();
            }
        });
    }

    fn on_pointer_motion(&mut self, location: Point<f64, Logical>, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let under = self.surface_under(location);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        if !self.lock.is_locked() {
            self.preview_motion(location);
            self.overview_motion(location);
            self.bar_motion(location);
            self.check_hot_corner(location);
        }
        // We draw the cursor ourselves on real hardware; the overview's
        // close button follows the pointer.
        if self.backend.draws_cursor() || self.overview.is_open() {
            self.queue_redraw();
        }
    }

    fn on_pointer_button(&mut self, button: u32, button_state: ButtonState, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        // Individual workspaces: the monitor clicked is the one the
        // workspace keys act on.
        if button_state == ButtonState::Pressed
            && let Some(output) = self.output_under_pointer()
        {
            self.workspace_monitor = Some(output.name());
        }
        // Mod+click (e.g. Super+drag) is not a tap.
        self.mod_tap_armed = false;
        // Locked: clicks are only for the locker.
        if self.lock.is_locked() {
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state: button_state,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            return;
        }
        // The dock's window preview takes clicks on it.
        if self.preview_button(
            pointer.current_location(),
            button_state == ButtonState::Pressed,
            button == BTN_LEFT,
        ) {
            return;
        }
        // The window switcher swallows clicks; one on a thumbnail picks it.
        if self.switcher.is_open() {
            if button_state == ButtonState::Pressed {
                let location = pointer.current_location();
                self.switcher_click(location);
            }
            return;
        }
        // Framing a screenshot: only its overlay gets clicks. Releases always
        // reach it (a drag may end anywhere); a press with no overlay under
        // it, e.g. on a monitor whose overlay isn't mapped yet, is dropped.
        if self.screenshot.active {
            let location = pointer.current_location();
            if button_state == ButtonState::Released
                || self
                    .layer_surface_under(&[WlrLayer::Overlay], location)
                    .is_some()
            {
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
            }
            return;
        }
        debug!(button, ?button_state, location = ?pointer.current_location(), "pointer button");

        if button_state == ButtonState::Released && button == BTN_LEFT && self.overview_holding() {
            self.overview_release(pointer.current_location());
            return;
        }
        if button_state == ButtonState::Released
            && !pointer.is_grabbed()
            && self.bar_release(pointer.current_location())
        {
            return;
        }
        if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
            let location = pointer.current_location();
            // Clicking a panel/launcher above the windows focuses it if it
            // wants the keyboard, and never the window underneath.
            if let Some((layer, _, _)) =
                self.layer_surface_under(self.layers_above_windows(location), location)
            {
                if layer.can_receive_keyboard_focus()
                    && self.exclusive_layer().is_none()
                    && !self.lock.is_locked()
                    && let Some(keyboard) = self.seat.get_keyboard()
                {
                    keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
                }
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
                return;
            }
            if self.overview.is_open() {
                if button == BTN_LEFT {
                    self.overview_press(location);
                }
                return;
            }
            // cnde's title bars and resize borders.
            if !self.mod_held() && self.bar_press(button, location) {
                return;
            }
            let window = self.space.element_under(location).map(|(w, _)| w.clone());
            self.focus_window(window.as_ref());

            // Right-click on the bare wallpaper: the shell's desktop menu.
            if window.is_none()
                && button == BTN_RIGHT
                && !self.mod_held()
                && self
                    .layer_surface_under(&[WlrLayer::Bottom, WlrLayer::Background], location)
                    .is_none()
                && let Some(output) = self.space.output_under(location).next().cloned()
                && let Some(geometry) = self.space.output_geometry(&output)
            {
                let local = location - geometry.loc.to_f64();
                self.ipc.broadcast(&cnde_ipc::Event::DesktopMenu {
                    output: output.name(),
                    x: local.x.floor() as i32,
                    y: local.y.floor() as i32,
                    serial: serial.into(),
                });
            }

            // Mod+left-drag moves, mod+right-drag resizes; the click never reaches the client.
            if let Some(window) = window.filter(|_| self.mod_held()) {
                let start_data = GrabStartData {
                    focus: None,
                    button,
                    location,
                };
                match button {
                    BTN_LEFT => {
                        self.begin_move(window, start_data, serial);
                        return;
                    }
                    BTN_RIGHT => {
                        let edges = self.edges_nearest(&window, location);
                        self.begin_resize(window, start_data, serial, edges);
                        return;
                    }
                    _ => {}
                }
            }
        }

        let was_grabbed = pointer.is_grabbed();
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: button_state,
                serial,
                time,
            },
        );
        pointer.frame(self);
        // A press outside an open menu dismisses its popup grab; the same
        // click also focuses and raises what is under the pointer.
        if button_state == ButtonState::Pressed
            && was_grabbed
            && !pointer.is_grabbed()
            && !self.overview.is_open()
        {
            let location = pointer.current_location();
            if self
                .layer_surface_under(self.layers_above_windows(location), location)
                .is_none()
            {
                let window = self.space.element_under(location).map(|(w, _)| w.clone());
                self.focus_window(window.as_ref());
                // The grab swallowed the press; hand it to what is under the
                // pointer, as a click with no menu open would have gone. Not
                // for an app's own menus: clicking off one shouldn't also
                // click a link under it.
                if window.is_some()
                    && self.shell_popup_grab
                    && !self.mod_held()
                    && !self.bar_press(button, location)
                {
                    let under = self.surface_under(location);
                    pointer.motion(
                        self,
                        under,
                        &MotionEvent {
                            location,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: button_state,
                            serial: SERIAL_COUNTER.next_serial(),
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
        }
    }

    fn on_pointer_axis<I: InputBackend>(&mut self, event: I::PointerAxisEvent) {
        let source = event.source();
        let amount = |axis| {
            event
                .amount(axis)
                .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.)
        };

        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let value = amount(axis);
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                frame = frame.stop(axis);
            }
        }

        // The wheel slides the strip in the overview (down = next).
        let over_layer = self.seat.get_pointer().is_some_and(|p| {
            let pos = p.current_location();
            self.surface_under(pos).is_some()
        });
        if self.overview.is_open() && !self.screenshot.active && !over_layer {
            if let Some(v120) = event.amount_v120(Axis::Vertical) {
                self.overview_scroll(v120);
            }
            return;
        }

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Start an interactive move. Dragging a maximized or snapped window
    /// restores its floating size first, keeping the pointer at the same
    /// relative spot on the titlebar (GNOME behaviour).
    pub fn begin_move(&mut self, window: Window, start_data: GrabStartData<Self>, serial: Serial) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // On a tiling workspace the window leaves its tile while dragged;
        // where it's dropped decides what happens (see `drop_window`).
        let tile = self.lift_for_move(&window);
        if Self::placement_of(&window) != Placement::Floating {
            let Some(geo) = self.space.element_geometry(&window) else {
                return;
            };
            let pointer_pos = start_data.location;
            let area = self
                .space
                .output_under(pointer_pos)
                .next()
                .cloned()
                .and_then(|output| self.usable_area(&output))
                .unwrap_or(geo);
            // A window that opened maximized/fullscreen has no remembered
            // floating size; pick one rather than letting the client choose
            // a size we can't position against.
            let restore_size = match with_state(&window, |s| s.restore.map(|r| r.size)) {
                Some(size) => size,
                None => {
                    let size = Size::from((
                        (area.size.w as f64 * DEFAULT_FLOATING_FRACTION) as i32,
                        (area.size.h as f64 * DEFAULT_FLOATING_FRACTION) as i32,
                    ));
                    with_state(&window, |s| s.restore = Some(Rectangle::new(geo.loc, size)));
                    size
                }
            };
            // Keep the pointer at the same relative spot on the window, in
            // both directions, so the restored window lands under it.
            // A title bar cnde draws counts as part of the window here.
            let bar = crate::decorations::titlebar_height(&window) as f64;
            let fraction_x = (pointer_pos.x - geo.loc.x as f64) / geo.size.w.max(1) as f64;
            let fraction_y =
                (pointer_pos.y - (geo.loc.y as f64 - bar)) / (geo.size.h as f64 + bar).max(1.0);
            // No animation: the window must stick to the pointer.
            self.set_placement(&window, Placement::Floating, false);
            let new_loc = Point::<f64, Logical>::from((
                pointer_pos.x - fraction_x * restore_size.w as f64,
                // Never above the usable area, so the titlebar stays reachable.
                (pointer_pos.y - fraction_y * (restore_size.h as f64 + bar) + bar)
                    .max(area.loc.y as f64 + bar),
            ));
            self.space
                .map_element(window.clone(), new_loc.to_i32_round(), true);
        }
        let Some(initial_window_location) = self.space.element_location(&window) else {
            return;
        };
        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
            tile,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    pub fn begin_resize(
        &mut self,
        window: Window,
        start_data: GrabStartData<Self>,
        serial: Serial,
        edges: ResizeEdge,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        let Some(geo) = self.space.element_geometry(&window) else {
            return;
        };
        // A tile keeps its place: its edges move the splits instead.
        if self.is_tiled(&window)
            && let Some(tile) = self.tile_slot_now(&window)
        {
            let grab = TileResizeGrab::start(start_data, window, edges, tile);
            pointer.set_grab(self, grab, serial, Focus::Clear);
            return;
        }
        // The edge a snap pair shares moves for both windows.
        let shared = match Self::placement_of(&window) {
            Placement::SnappedLeft => ResizeEdge::RIGHT,
            Placement::SnappedRight => ResizeEdge::LEFT,
            _ => ResizeEdge::empty(),
        };
        if edges == shared
            && self.snap_partner(&window).is_some()
            && let Some(edge_x) = self.snap_edge_x(&window)
        {
            let grab = SnapPairResizeGrab::start(start_data, window, edge_x);
            pointer.set_grab(self, grab, serial, Focus::Clear);
            return;
        }
        // Resizing a maximized/snapped window turns it floating at its current size.
        if Self::placement_of(&window) != Placement::Floating {
            with_state(&window, |s| s.restore = Some(geo));
            self.set_placement(&window, Placement::Floating, false);
        }

        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });
        toplevel.send_pending_configure();

        let grab =
            ResizeSurfaceGrab::start(start_data, window, edges, Rectangle::new(geo.loc, geo.size));
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    /// For mod+right-drag: resize from whichever corner the pointer is closest to.
    fn edges_nearest(&self, window: &Window, location: Point<f64, Logical>) -> ResizeEdge {
        let Some(geo) = self.space.element_geometry(window) else {
            return ResizeEdge::BOTTOM_RIGHT;
        };
        let centre_x = geo.loc.x as f64 + geo.size.w as f64 / 2.0;
        let centre_y = geo.loc.y as f64 + geo.size.h as f64 / 2.0;
        let horizontal = if location.x < centre_x {
            ResizeEdge::LEFT
        } else {
            ResizeEdge::RIGHT
        };
        let vertical = if location.y < centre_y {
            ResizeEdge::TOP
        } else {
            ResizeEdge::BOTTOM
        };
        horizontal | vertical
    }
}

/// What a game asked of the pointer, where it is now.
enum Constraint {
    Locked,
    /// Inside the surface, and this region of it if any.
    Confined(Option<RegionAttributes>),
}

/// The active constraint on the surface under the pointer, if the pointer
/// is inside its region (outside it, the constraint doesn't apply).
fn active_constraint(
    pointer: &smithay::input::pointer::PointerHandle<State>,
    location: Point<f64, Logical>,
    under: Option<&(WlSurface, Point<f64, Logical>)>,
    // Whether the client's region counts (see `confine_region_applies`).
    use_region: bool,
) -> Option<Constraint> {
    let (surface, origin) = under?;
    with_pointer_constraint(surface, pointer, |constraint| {
        let constraint = constraint.filter(|c| c.is_active())?;
        let point = (location - *origin).to_i32_round();
        if use_region && !constraint.region().is_none_or(|r| r.contains(point)) {
            return None;
        }
        Some(match &*constraint {
            PointerConstraint::Locked(_) => Constraint::Locked,
            PointerConstraint::Confined(confined) => Constraint::Confined(confined.region().cloned()),
        })
    })
}
