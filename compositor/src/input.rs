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

use crate::{
    grabs::{MoveSurfaceGrab, ResizeEdge, ResizeSurfaceGrab},
    keybinds::{self, Action, ModKey},
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
                let location = self.clamp_to_outputs(current, current + event.delta());
                self.on_pointer_motion(location, event.time_msec());
                // Unaccelerated deltas for games and 3D apps (relative-pointer).
                let under = self.surface_under(location);
                pointer.relative_motion(
                    self,
                    under,
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                pointer.frame(self);
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
                    state.start_media_repeat(keycode, key);
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
                // Escape leaves the overview, unless the shell's search box
                // has the keyboard (it decides: clear the text, then close).
                if sym.raw() == keysyms::KEY_Escape
                    && state.overview.is_open()
                    && state.exclusive_layer().is_none()
                {
                    state.suppressed_keys.push(keycode);
                    return FilterResult::Intercept(Some(Action::CloseOverview));
                }
                match keybinds::action_for(mod_key, mods, sym) {
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

    fn run_action(&mut self, action: Action) {
        if self.lock.is_locked() && !matches!(action, Action::Media(_) | Action::SwitchVt(_)) {
            return;
        }
        match action {
            Action::Lock => self.lock_screen(),
            Action::Quit => self.loop_signal.stop(),
            Action::SpawnTerminal => spawn::spawn_terminal(&self.client_env()),
            Action::SpawnBrowser => spawn::spawn_browser(&self.client_env()),
            Action::CloseWindow => self.close_focused(),
            Action::Maximize => self.set_focused_placement(Placement::Maximized),
            Action::Restore => self.set_focused_placement(Placement::Floating),
            Action::SnapLeft => self.set_focused_placement(Placement::SnappedLeft),
            Action::SnapRight => self.set_focused_placement(Placement::SnappedRight),
            Action::ToggleFullscreen => self.toggle_focused_fullscreen(),
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
            Action::Launcher if self.screenshot.active => {}
            Action::Launcher => {
                let command = self.config.launcher.command.clone();
                if command.trim().is_empty() {
                    self.super_tapped();
                } else {
                    spawn::spawn_command_line(&command, &self.client_env());
                }
            }
            Action::CloseOverview => self.set_overview(false),
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
    pub fn fake_pointer(&mut self, location: Point<f64, Logical>, press: Option<bool>) {
        let time = self.start_time.elapsed().as_millis() as u32;
        self.on_pointer_motion(location, time);
        if let Some(press) = press {
            let state = if press {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            };
            self.on_pointer_button(BTN_LEFT, state, time);
        }
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
            self.bar_motion(location);
            self.check_hot_corner(location);
        }
        // We draw the cursor ourselves on real hardware.
        if self.backend.draws_cursor() {
            self.queue_redraw();
        }
    }

    fn on_pointer_button(&mut self, button: u32, button_state: ButtonState, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
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
        debug!(button, ?button_state, location = ?pointer.current_location(), "pointer button");

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
                    self.overview_click(location);
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
        if self.overview.is_open() && !over_layer {
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
