// Adapted from smithay: smallvil/src/input.rs

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    desktop::Window,
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
};

use crate::{
    grabs::{MoveSurfaceGrab, ResizeEdge, ResizeSurfaceGrab},
    keybinds::{self, Action, ModKey},
    spawn,
    state::State,
    window::{Placement, with_state},
};

const BTN_LEFT: u32 = 0x110;
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
            InputEvent::PointerButton { event, .. } => {
                self.on_pointer_button(event.button_code(), event.state(), event.time_msec());
            }
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<I>(event),
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
                if key_state == KeyState::Released {
                    if let Some(i) = state.suppressed_keys.iter().position(|k| *k == keycode) {
                        state.suppressed_keys.swap_remove(i);
                        return FilterResult::Intercept(None);
                    }
                    return FilterResult::Forward;
                }
                let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() else {
                    return FilterResult::Forward;
                };
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
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Quit => self.loop_signal.stop(),
            Action::SpawnTerminal => spawn::spawn_terminal(&self.socket_name),
            Action::SpawnBrowser => spawn::spawn_browser(&self.socket_name),
            Action::CloseWindow => self.close_focused(),
            Action::Maximize => self.set_focused_placement(Placement::Maximized),
            Action::Restore => self.set_focused_placement(Placement::Floating),
            Action::SnapLeft => self.snap_focused(Placement::SnappedLeft),
            Action::SnapRight => self.snap_focused(Placement::SnappedRight),
            Action::ToggleFullscreen => self.toggle_focused_fullscreen(),
        }
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
    }

    fn on_pointer_button(&mut self, button: u32, button_state: ButtonState, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();

        if button_state == ButtonState::Pressed && !pointer.is_grabbed() {
            let location = pointer.current_location();
            let window = self.space.element_under(location).map(|(w, _)| w.clone());
            self.focus_window(window.as_ref());

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
            let restore_size =
                with_state(&window, |s| s.restore.map(|r| r.size)).unwrap_or(geo.size);
            let pointer_pos = start_data.location;
            let fraction_x = (pointer_pos.x - geo.loc.x as f64) / geo.size.w.max(1) as f64;
            self.set_placement(&window, Placement::Floating);
            let new_loc = Point::<f64, Logical>::from((
                pointer_pos.x - fraction_x * restore_size.w as f64,
                geo.loc.y as f64,
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
            self.set_placement(&window, Placement::Floating);
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
