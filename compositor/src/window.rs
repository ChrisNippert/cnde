//! Floating window management: placement, maximize/snap/fullscreen, focus,
//! and the per-window animation state (ARCHITECTURE.md §4.4).

use std::{cell::RefCell, time::Instant};

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    desktop::Window,
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER, Scale, Size},
};
use tracing::debug;

use crate::{
    animation::{Animation, presets},
    effects::{self, ClosingWindow, ResizeAnim},
    snapshot::{LastFrame, Snapshot},
    state::State,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    Floating,
    Maximized,
    SnappedLeft,
    SnappedRight,
    Fullscreen,
}

#[derive(Default)]
pub struct WindowState {
    pub placement: Placement,
    /// Floating geometry to return to when leaving maximize/snap/fullscreen.
    pub restore: Option<Rectangle<i32, Logical>>,
    pub open_anim: Option<Animation>,
    pub resize_anim: Option<ResizeAnim>,
    /// What we last drew, for close animations of killed clients.
    pub last_frame: Option<LastFrame>,
    /// Geometry size at the last commit, to notice client-driven resizes.
    pub last_size: Size<i32, Logical>,
}

pub fn with_state<T>(window: &Window, f: impl FnOnce(&mut WindowState) -> T) -> T {
    let data = window.user_data();
    data.insert_if_missing(|| RefCell::new(WindowState::default()));
    let cell = data
        .get::<RefCell<WindowState>>()
        .expect("WindowState was inserted above");
    f(&mut cell.borrow_mut())
}

impl State {
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(&self.unmapped)
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    /// The monitor `window` belongs to: the one containing the centre of its
    /// geometry, else the one it overlaps most. Why not `outputs_for_element`:
    /// that counts client-side shadows, so a window snapped to a monitor's
    /// edge "touches" its neighbour too, and its order is arbitrary.
    /// Unmapped windows belong to the monitor under the pointer (GNOME
    /// opens new windows where you're looking).
    pub fn output_for(&self, window: &Window) -> Option<Output> {
        let Some(geo) = self.space.element_geometry(window) else {
            return self.output_under_pointer();
        };
        let centre = geo.loc.to_f64() + geo.size.to_f64().downscale(2.0).to_point();
        if let Some(output) = self.space.output_under(centre).next() {
            return Some(output.clone());
        }
        self.space
            .outputs()
            .max_by_key(|output| {
                self.space
                    .output_geometry(output)
                    .and_then(|output_geo| output_geo.intersection(geo))
                    .map_or(0, |overlap| overlap.size.w * overlap.size.h)
            })
            .cloned()
    }

    fn output_under_pointer(&self) -> Option<Output> {
        let location = self.seat.get_pointer()?.current_location();
        self.space
            .output_under(location)
            .next()
            .or_else(|| self.space.outputs().next())
            .cloned()
    }

    /// The area a window in `placement` may occupy on its monitor: the whole
    /// monitor when fullscreen, otherwise what panels leave free.
    fn area_for(&self, window: &Window, placement: Placement) -> Option<Rectangle<i32, Logical>> {
        self.area_on(&self.output_for(window)?, placement)
    }

    fn area_on(&self, output: &Output, placement: Placement) -> Option<Rectangle<i32, Logical>> {
        match placement {
            Placement::Fullscreen => self.space.output_geometry(output),
            _ => self.usable_area(output),
        }
    }

    /// Where `window`'s geometry is drawn right now, mid-animation included.
    fn visual_rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        let live = self.space.element_geometry(window)?.to_f64();
        Some(with_state(window, |s| {
            effects::window_frame(s, live, Instant::now()).rect
        }))
    }

    fn snapshot(&mut self, window: &Window) -> Option<Snapshot> {
        let scale = Scale::from(self.output_for(window)?.current_scale().fractional_scale());
        Snapshot::capture(self.backend.renderer()?, window, scale)
    }

    /// Keep showing a destroyed window as a fading, shrinking snapshot. Must
    /// run before the window is unmapped, while its buffers still exist.
    pub fn start_close_animation(&mut self, window: &Window) {
        let Some(below) = self.space.elements().position(|w| w == window) else {
            return;
        };
        let last_frame = with_state(window, |s| s.last_frame.take());
        // Capture live only if the window still has contents. A killed
        // client's surfaces are gone, and a hidden window (null buffer) may
        // still report a size but has nothing to draw, so both fall back to
        // the last frame we drew.
        let alive = !window.geometry().is_empty()
            && window.toplevel().is_some_and(|t| {
                with_renderer_surface_state(t.wl_surface(), |s| s.buffer().is_some())
                    .unwrap_or(false)
            });
        let rect = if alive {
            self.visual_rect(window)
        } else {
            last_frame.as_ref().map(|frame| frame.rect)
        };
        let Some(rect) = rect else {
            return;
        };
        let snapshot = if alive { self.snapshot(window) } else { None }
            .or_else(|| Snapshot::from_last_frame(self.backend.renderer()?, last_frame?));
        debug!(captured = snapshot.is_some(), "window closing");
        if let Some(snapshot) = snapshot {
            self.closing.push(ClosingWindow::new(snapshot, rect, below));
            self.queue_redraw();
        }
    }

    /// Called on a toplevel's first buffer commit.
    pub fn map_new_window(&mut self, window: Window) {
        self.unmapped.retain(|w| w != &window);
        let placement = with_state(&window, |s| s.placement);
        let Some(area) = self.area_for(&window, placement) else {
            return;
        };

        let location = match placement {
            Placement::Floating => {
                let size = window.geometry().size;
                let x = area.loc.x + ((area.size.w - size.w) / 2).max(0);
                let y = area.loc.y + ((area.size.h - size.h) / 2).max(0);
                (x, y).into()
            }
            other => placement_rect(other, area).loc,
        };

        debug!(size = ?window.geometry().size, ?location, ?placement, "mapping window");
        with_state(&window, |s| {
            s.open_anim = Some(Animation::new(0.0, 1.0, presets::WINDOW_OPEN));
        });
        self.space.map_element(window.clone(), location, true);
        self.focus_window(Some(&window));
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// Called when a mapped toplevel attaches a null buffer or is destroyed.
    pub fn unmap_window(&mut self, window: &Window) {
        self.space.unmap_elem(window);
        let topmost = self.space.elements().last().cloned();
        self.focus_window(topmost.as_ref());
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// Whether the topmost window on `output` is fullscreen.
    pub fn fullscreen_on(&self, output: &Output) -> bool {
        self.space
            .elements_for_output(output)
            .last()
            .is_some_and(|w| Self::placement_of(w) == Placement::Fullscreen)
    }

    pub fn focused_window(&self) -> Option<Window> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        self.window_for_surface(&focus)
    }

    /// Raise and focus `window`, or clear focus with `None`.
    pub fn focus_window(&mut self, window: Option<&Window>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if let Some(window) = window {
            self.space.raise_element(window, true);
        }
        for other in self.space.elements() {
            if Some(other) != window {
                other.set_activated(false);
            }
            if let Some(toplevel) = other.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        let surface = match self.exclusive_layer() {
            // An open launcher keeps the keyboard until it closes.
            Some(layer) => Some(layer.wl_surface().clone()),
            None => window
                .and_then(|w| w.toplevel())
                .map(|t| t.wl_surface().clone()),
        };
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
    }

    pub fn close_focused(&mut self) {
        if let Some(toplevel) = self.focused_window().as_ref().and_then(|w| w.toplevel()) {
            toplevel.send_close();
        }
    }

    pub fn placement_of(window: &Window) -> Placement {
        with_state(window, |s| s.placement)
    }

    /// Move `window` into `target` placement: remember the floating geometry,
    /// tell the client its new size and states, and position it. With
    /// `animate`, the window glides from where it is now once the client has
    /// redrawn at the new size.
    pub fn set_placement(&mut self, window: &Window, target: Placement, animate: bool) {
        self.set_placement_on(window, target, animate, None);
    }

    /// [`set_placement`](Self::set_placement) on a specific monitor rather
    /// than the one the window is mostly on (drag-to-snap uses the monitor
    /// under the pointer, where the preview was shown).
    pub fn set_placement_on(
        &mut self,
        window: &Window,
        target: Placement,
        animate: bool,
        output: Option<Output>,
    ) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let current = Self::placement_of(window);
        debug!(from = ?current, to = ?target, "set_placement");
        if current == target && output.is_none() {
            return;
        }
        let area = match &output {
            Some(output) => self.area_on(output, target),
            None => self.area_for(window, target),
        };
        let Some(area) = area else {
            return;
        };
        let mapped_location = self.space.element_location(window);
        let animation_start = if animate && mapped_location.is_some() {
            self.visual_rect(window)
                .map(|from| (from, self.snapshot(window)))
        } else {
            None
        };

        let restore = with_state(window, |s| {
            if current == Placement::Floating
                && let Some(loc) = mapped_location
            {
                s.restore = Some(Rectangle::new(loc, window.geometry().size));
            }
            s.placement = target;
            s.restore
        });

        let rect = match target {
            Placement::Floating => restore,
            other => Some(placement_rect(other, area)),
        };

        toplevel.with_pending_state(|s| {
            for state in [
                XdgState::Maximized,
                XdgState::Fullscreen,
                XdgState::TiledLeft,
                XdgState::TiledRight,
                XdgState::TiledTop,
                XdgState::TiledBottom,
            ] {
                s.states.unset(state);
            }
            match target {
                Placement::Floating => {}
                Placement::Maximized => {
                    s.states.set(XdgState::Maximized);
                }
                Placement::Fullscreen => {
                    s.states.set(XdgState::Fullscreen);
                }
                // Tiled states tell CSD clients to drop rounded corners and shadows on those edges.
                Placement::SnappedLeft | Placement::SnappedRight => {
                    s.states.set(XdgState::TiledTop);
                    s.states.set(XdgState::TiledBottom);
                    s.states.set(if target == Placement::SnappedLeft {
                        XdgState::TiledLeft
                    } else {
                        XdgState::TiledRight
                    });
                }
            }
            s.size = rect.map(|r| r.size);
        });
        let serial = if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure()
        } else {
            None
        };

        if let (Some(rect), Some(_)) = (rect, mapped_location) {
            self.space.map_element(window.clone(), rect.loc, true);
        }
        if let Some((from, snapshot)) = animation_start {
            with_state(window, |s| {
                s.resize_anim = Some(ResizeAnim::new(from, snapshot, serial));
            });
        }
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// Also used for Super+Left/Right. Unlike GNOME, snapping a left-snapped
    /// window right goes straight to the right half instead of un-snapping;
    /// the floating geometry is still remembered for Super+Down.
    pub fn set_focused_placement(&mut self, target: Placement) {
        if let Some(window) = self.focused_window() {
            self.set_placement(&window, target, true);
        }
    }

    pub fn toggle_focused_fullscreen(&mut self) {
        if let Some(window) = self.focused_window() {
            let target = if Self::placement_of(&window) == Placement::Fullscreen {
                Placement::Floating
            } else {
                Placement::Fullscreen
            };
            self.set_placement(&window, target, true);
        }
    }

    /// Keep maximized/snapped/fullscreen windows filling their area after the
    /// output is resized.
    pub fn relayout_after_output_change(&mut self) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            let placement = Self::placement_of(&window);
            if placement == Placement::Floating {
                continue;
            }
            let (Some(area), Some(toplevel)) =
                (self.area_for(&window, placement), window.toplevel())
            else {
                continue;
            };
            let rect = placement_rect(placement, area);
            toplevel.with_pending_state(|s| s.size = Some(rect.size));
            toplevel.send_pending_configure();
            self.space.map_element(window.clone(), rect.loc, false);
        }
    }
}

/// The rectangle a non-floating placement occupies within `area`.
pub fn placement_rect(
    placement: Placement,
    area: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let half = area.size.w / 2;
    match placement {
        Placement::SnappedLeft => Rectangle::new(area.loc, Size::from((half, area.size.h))),
        Placement::SnappedRight => Rectangle::new(
            (area.loc.x + half, area.loc.y).into(),
            Size::from((area.size.w - half, area.size.h)),
        ),
        Placement::Floating | Placement::Maximized | Placement::Fullscreen => area,
    }
}
