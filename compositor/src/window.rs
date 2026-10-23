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
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Scale, Size},
};
use tracing::debug;

use crate::{
    animation::{Animation, presets},
    decorations::{content_rect, titlebar_height},
    effects::{self, ClosingWindow, PendingSize, ResizeAnim},
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
    /// Stable identity for IPC clients (the dock), never reused.
    pub id: u64,
    pub placement: Placement,
    /// Floating geometry to return to when leaving maximize/snap/fullscreen.
    pub restore: Option<Rectangle<i32, Logical>>,
    /// Maximized or snapped before going fullscreen: where leaving
    /// fullscreen returns to, instead of floating.
    pub before_fullscreen: Option<Placement>,
    pub open_anim: Option<Animation>,
    /// With `open_anim`: the dock icon (global coordinates) it grows out of,
    /// when restored from minimized.
    pub open_from: Option<Rectangle<f64, Logical>>,
    pub resize_anim: Option<ResizeAnim>,
    pub pending_size: Option<PendingSize>,
    /// Position relative to its monitor's origin; kept for hidden windows
    /// and used while the workspace strip moves (see workspaces.rs).
    pub local: Point<i32, Logical>,
    /// What we last drew, for close animations of killed clients.
    pub last_frame: Option<LastFrame>,
    /// Geometry size at the last commit, to notice client-driven resizes.
    pub last_size: Size<i32, Logical>,
    /// The title bar cnde draws for it, if it's one that lets us.
    pub titlebar: Option<crate::decorations::Titlebar>,
    pub titlebar_hover: Option<crate::decorations::Part>,
    /// Hidden until activated again (the dock, the overview's search).
    /// It stays in its workspace, just not in the space.
    pub minimized: bool,
    /// Asked to show something while out of sight; the dock lights its
    /// icon until the window is focused.
    pub urgent: bool,
    /// On a tiling workspace, kept out of the tiles: floated by the user
    /// (Super+Shift+T, dropped off the tiles, resized), or lifted while
    /// being dragged.
    pub keep_floating: bool,
    /// Already promoted to fullscreen for covering its monitor, so leaving
    /// fullscreen again sticks.
    pub auto_fullscreened: bool,
    /// Local position and size before its workspace started tiling, to
    /// return to when it stops.
    pub untiled: Option<Rectangle<i32, Logical>>,
}

pub fn with_state<T>(window: &Window, f: impl FnOnce(&mut WindowState) -> T) -> T {
    let data = window.user_data();
    data.insert_if_missing(|| {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        RefCell::new(WindowState {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            ..WindowState::default()
        })
    });
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

    pub(crate) fn output_under_pointer(&self) -> Option<Output> {
        let location = self.seat.get_pointer()?.current_location();
        self.space
            .output_under(location)
            .next()
            .or_else(|| self.space.outputs().next())
            .cloned()
    }

    /// The area a window in `placement` may occupy on its monitor: the whole
    /// monitor when fullscreen, otherwise what panels leave free.
    pub(crate) fn area_for(
        &self,
        window: &Window,
        placement: Placement,
    ) -> Option<Rectangle<i32, Logical>> {
        self.area_on(&self.output_for(window)?, placement)
    }

    fn area_on(&self, output: &Output, placement: Placement) -> Option<Rectangle<i32, Logical>> {
        match placement {
            Placement::Fullscreen => self.space.output_geometry(output),
            _ => self.usable_area(output),
        }
    }

    /// Where `window`'s geometry is drawn right now, mid-animation included.
    pub(crate) fn visual_rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        let live = self.space.element_geometry(window)?.to_f64();
        Some(with_state(window, |s| {
            effects::window_frame(s, live, Instant::now()).rect
        }))
    }

    pub(crate) fn snapshot(&mut self, window: &Window) -> Option<Snapshot> {
        let scale = Scale::from(self.output_for(window)?.current_scale().fractional_scale());
        Snapshot::capture(self.backend.renderer()?, window, scale)
    }

    /// Keep showing a destroyed window as a fading, shrinking snapshot. Must
    /// run before the window is unmapped, while its buffers still exist.
    pub fn start_close_animation(&mut self, window: &Window) {
        self.start_close_animation_to(window, None);
    }

    /// [`start_close_animation`](Self::start_close_animation), flying into
    /// the dock icon `icon` (global coordinates) if given.
    fn start_close_animation_to(&mut self, window: &Window, icon: Option<Rectangle<f64, Logical>>) {
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
        debug!(
            captured = snapshot.is_some(),
            ?rect,
            ?icon,
            "window closing"
        );
        if let Some(snapshot) = snapshot {
            self.closing
                .push(ClosingWindow::new(snapshot, rect, icon, below));
            self.queue_redraw();
        }
    }

    /// Smithay tells a window about every monitor its whole buffer touches,
    /// shadows, hidden frames and all. Clients (xwayland-satellite places an
    /// X11 window on the last monitor it entered) should only hear about the
    /// monitors the window itself is on, and a fullscreen or maximized one
    /// only about its own. Runs after `Space::refresh`, which sends the
    /// buffer-based enters; the leaves go out in the same flush.
    ///
    /// `Space` keeps its own record of which outputs it entered the window
    /// on and only re-sends an enter when that overlap changes. A bare
    /// `output_leave` would leave the record saying "entered", so a later
    /// move back onto that monitor (same overlap) was never announced. We
    /// remember what we trimmed per window and re-send the enter ourselves
    /// once the window belongs on that monitor again.
    pub fn trim_window_outputs(&mut self) {
        enum Act {
            Leave(Output),
            Enter(Output, Rectangle<i32, Logical>),
        }
        let mut acts = Vec::new();
        for window in self.space.elements() {
            let entered = self.space.outputs_for_element(window);
            let trimmed_before = with_trimmed(window, |t| {
                t.retain(|o| entered.contains(o));
                !t.is_empty()
            });
            if entered.len() < 2
                && !trimmed_before
                && !entered.iter().any(|o| !self.window_on(window, o))
            {
                continue;
            }
            let own = matches!(
                Self::placement_of(window),
                Placement::Fullscreen | Placement::Maximized
            )
            .then(|| self.output_for(window))
            .flatten();
            let bbox = self.space.element_bbox(window);
            let keeps: Vec<bool> = entered
                .iter()
                .map(|output| match &own {
                    Some(own) => own == output,
                    None => self.window_on(window, output),
                })
                .collect();
            // Never leave a window on no monitor at all: a game told it's
            // nowhere may never present (black screen, audio only).
            let none_kept = !keeps.contains(&true);
            for (i, (output, keep)) in entered.into_iter().zip(keeps).enumerate() {
                let keep = keep || (none_kept && i == 0);
                if !keep {
                    // Every time: a changed overlap makes `Space` re-enter.
                    with_trimmed(window, |t| {
                        if !t.contains(&output) {
                            t.push(output.clone());
                        }
                    });
                    acts.push((window.clone(), Act::Leave(output)));
                } else if with_trimmed(window, |t| {
                    let had = t.contains(&output);
                    t.retain(|o| *o != output);
                    had
                }) {
                    let overlap = bbox.zip(self.space.output_geometry(&output)).and_then(
                        |(bbox, out)| {
                            let mut overlap = out.intersection(bbox)?;
                            overlap.loc -= bbox.loc;
                            Some(overlap)
                        },
                    );
                    if let Some(overlap) = overlap {
                        acts.push((window.clone(), Act::Enter(output, overlap)));
                    }
                }
            }
        }
        for (window, act) in acts {
            match act {
                Act::Leave(output) => {
                    smithay::desktop::space::SpaceElement::output_leave(&window, &output)
                }
                Act::Enter(output, overlap) => {
                    smithay::desktop::space::SpaceElement::output_enter(&window, &output, overlap)
                }
            }
        }
    }

    fn window_on(&self, window: &Window, output: &Output) -> bool {
        match (
            self.space.element_geometry(window),
            self.space.output_geometry(output),
        ) {
            (Some(geo), Some(out)) => geo.overlaps(out),
            _ => true,
        }
    }

    /// A "borderless fullscreen" game: a floating window that fills (or
    /// overfills) its whole monitor and has no cnde title bar, without asking
    /// for fullscreen. Make it a real one: above the panels, only on that
    /// monitor. Only once per window, so leaving fullscreen sticks.
    pub fn promote_borderless(&mut self, window: &Window) {
        if with_state(window, |s| s.placement != Placement::Floating || s.auto_fullscreened)
            || titlebar_height(window) != 0
            || window.toplevel().is_none()
        {
            return;
        }
        // Only X11 windows: that's where games do "borderless fullscreen".
        // A Wayland app drawing its own title bar can open monitor-sized
        // and must stay a window.
        if !window.toplevel().is_some_and(|t| {
            crate::xwayland::is_x11(&self.display_handle, t.wl_surface())
        }) {
            return;
        }
        let Some(output) = self.output_for(window) else {
            return;
        };
        let Some(out_geo) = self.space.output_geometry(&output) else {
            return;
        };
        if !covers_output(window.geometry().size, out_geo.size) {
            return;
        }
        debug!(size = ?window.geometry().size, output = %output.name(), "borderless window covers its monitor; fullscreening");
        with_state(window, |s| s.auto_fullscreened = true);
        self.set_placement(window, Placement::Fullscreen, false);
    }

    /// Called on a toplevel's first buffer commit.
    pub fn map_new_window(&mut self, window: Window) {
        self.unmapped.retain(|w| w != &window);
        let placement = with_state(&window, |s| s.placement);
        let Some(area) = self.area_for(&window, placement) else {
            return;
        };

        let bar = titlebar_height(&window);
        let location = match placement {
            Placement::Floating => {
                let size = window.geometry().size;
                let x = area.loc.x + ((area.size.w - size.w) / 2).max(0);
                // Centred with its title bar, which stays on screen.
                let y = area.loc.y + bar + ((area.size.h - size.h - bar) / 2).max(0);
                (x, y).into()
            }
            other => content_rect(placement_rect(other, area), bar).loc,
        };

        debug!(size = ?window.geometry().size, ?location, ?placement, "mapping window");
        with_state(&window, |s| {
            s.open_anim = Some(Animation::new(0.0, 1.0, presets::WINDOW_OPEN));
        });
        self.space.map_element(window.clone(), location, true);
        self.launching = None;
        self.capture.window_mapped(&window);
        self.add_window_to_workspace(&window);
        // On a tiling workspace it grows in its tile and the others make room.
        self.relayout_visible(true);
        self.promote_borderless(&window);
        self.focus_window(Some(&window));
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// Called when a mapped toplevel attaches a null buffer or is destroyed.
    pub fn unmap_window(&mut self, window: &Window) {
        self.space.unmap_elem(window);
        self.capture.window_unmapped(window);
        self.overview_before_relayout();
        self.remove_window_from_workspaces(window);
        // The remaining tiles close the gap.
        self.relayout_visible(true);
        let topmost = self.space.elements().last().cloned();
        self.focus_window(topmost.as_ref());
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// Whether the topmost window on `output` is fullscreen. Only windows
    /// centred on the monitor count, so a window reaching over from the next
    /// one doesn't hide the fullscreen state.
    pub fn fullscreen_on(&self, output: &Output) -> bool {
        Self::fullscreen_in(&self.space, output)
    }

    /// `fullscreen_on` for callers holding the space but not all of `self`.
    pub fn fullscreen_in(space: &smithay::desktop::Space<Window>, output: &Output) -> bool {
        let Some(geo) = space.output_geometry(output) else {
            return false;
        };
        space
            .elements_for_output(output)
            .rfind(|w| {
                space
                    .element_geometry(w)
                    .is_some_and(|g| geo.contains(g.loc + g.size.downscale(2).to_point()))
            })
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
        // Locked: the keyboard stays with the locker.
        if self.lock.is_locked() {
            if let Some(window) = window {
                self.space.raise_element(window, true);
                self.workspaces.raise(window);
            }
            return;
        }
        if let Some(window) = window {
            self.space.raise_element(window, true);
            self.workspaces.raise(window);
            with_state(window, |s| s.urgent = false);
            self.mru.retain(|w| w != window && smithay::utils::IsAlive::alive(w));
            self.mru.insert(0, window.clone());
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
        self.release_unfocused_constraint();
        // Title bars show which window has focus.
        self.queue_redraw();
    }

    /// Where `window`'s dock icon is on screen (global coordinates), if the
    /// dock told us, it's on the monitor showing the window's workspace and
    /// fully in view (an autohidden dock isn't).
    pub(crate) fn dock_icon_rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        let id = with_state(window, |s| s.id);
        let (output_name, cell) = self.dock_icons.get(&id)?;
        let output = self.space.outputs().find(|o| o.name() == *output_name)?;
        let shown = self
            .workspaces
            .list
            .iter()
            .position(|ws| ws.windows.contains(window))
            == self.workspace_on(output);
        let output_geo = self.space.output_geometry(output)?.to_f64();
        let area = {
            let map = smithay::desktop::layer_map_for_output(output);
            map.layers()
                .find(|l| l.namespace() == cnde_ipc::dock::NAMESPACE)
                .and_then(|l| map.layer_geometry(l))?
        };
        let rect = Rectangle::new(
            Point::from((
                output_geo.loc.x + area.loc.x as f64 + cell[0],
                output_geo.loc.y + area.loc.y as f64 + cell[1],
            )),
            Size::from((cell[2], cell[3])),
        );
        (shown && output_geo.contains_rect(rect)).then_some(rect)
    }

    /// Hide `window` until it's activated again; it flies into its dock
    /// icon, or shrinks away like a closing one without it.
    pub fn minimize(&mut self, window: &Window) {
        if !self.space.elements().any(|w| w == window) {
            return;
        }
        let icon = self.dock_icon_rect(window);
        self.start_close_animation_to(window, icon);
        with_state(window, |s| s.minimized = true);
        self.space.unmap_elem(window);
        self.relayout_visible(true);
        let topmost = self.space.elements().last().cloned();
        self.focus_window(topmost.as_ref());
        self.queue_redraw();
        self.queue_pointer_refresh();
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
        // A video leaving fullscreen in a maximized browser stays maximized.
        let target = if current == Placement::Fullscreen && target == Placement::Floating {
            with_state(window, |s| s.before_fullscreen.take()).unwrap_or(target)
        } else {
            target
        };
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
            if target == Placement::Fullscreen && current != Placement::Fullscreen {
                s.before_fullscreen = (current != Placement::Floating).then_some(current);
            }
            s.placement = target;
            s.restore
        });

        // On a tiling workspace, back to its tile rather than where it floated.
        let tiled = target == Placement::Floating && self.is_tiled(window);
        let rect = match target {
            Placement::Floating if tiled => self.tile_rect_now(window).or(restore),
            Placement::Floating => restore,
            other => Some(content_rect(
                placement_rect(other, area),
                titlebar_height(window),
            )),
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
                Placement::Floating => crate::layout::set_tiled(&mut s.states, tiled),
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
        } else if let (Some(rect), Some(serial), Some(_)) = (rect, serial, mapped_location) {
            // The client's committed size; map_element above doesn't change it.
            let old_size = window.geometry().size;
            if rect.size != old_size {
                with_state(window, |s| {
                    s.pending_size = Some(PendingSize::new(rect.size, old_size, serial));
                });
            }
        }
        // A window joining or leaving the tiles moves the others.
        if mapped_location.is_some() {
            self.relayout_visible(animate);
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
        // Panels and docks first: they stretch to the new size (sending
        // their clients a configure), and maximized windows fit around
        // their new exclusive zones.
        for output in self.space.outputs() {
            smithay::desktop::layer_map_for_output(output).arrange();
        }
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
            let rect = content_rect(placement_rect(placement, area), titlebar_height(&window));
            toplevel.with_pending_state(|s| s.size = Some(rect.size));
            toplevel.send_pending_configure();
            self.move_window(&window, rect.loc);
        }
        // Tiles refit the new area too.
        self.relayout_visible(false);
    }

    /// Move a mapped window, keeping its place in the stack. Why not just
    /// `map_element`: that puts it on top, so re-fitting maximized windows
    /// after a setting changed buried the window being used.
    pub fn move_window(&mut self, window: &Window, loc: Point<i32, Logical>) {
        let above: Vec<Window> = self
            .space
            .elements()
            .skip_while(|w| *w != window)
            .skip(1)
            .cloned()
            .collect();
        self.space.map_element(window.clone(), loc, false);
        for other in &above {
            self.space.raise_element(other, false);
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

/// Whether a window of `window` size hides a whole monitor of `output` size.
fn covers_output(window: Size<i32, Logical>, output: Size<i32, Logical>) -> bool {
    output.w > 0 && output.h > 0 && window.w >= output.w && window.h >= output.h
}

#[cfg(test)]
mod borderless_tests {
    use super::covers_output;

    #[test]
    fn covering_sizes() {
        assert!(covers_output((2560, 1440).into(), (2560, 1440).into()));
        assert!(covers_output((2566, 1472).into(), (2560, 1440).into()));
        assert!(!covers_output((1920, 1080).into(), (2560, 1440).into()));
        assert!(!covers_output((2560, 1400).into(), (2560, 1440).into()));
        assert!(!covers_output((100, 100).into(), (0, 0).into()));
    }
}

/// Outputs we sent a window an `output_leave` for while `Space` still counts
/// it as entered (see `trim_window_outputs`).
#[derive(Default)]
struct Trimmed(std::cell::RefCell<Vec<Output>>);

fn with_trimmed<R>(window: &Window, f: impl FnOnce(&mut Vec<Output>) -> R) -> R {
    let data = window.user_data();
    data.insert_if_missing(Trimmed::default);
    f(&mut data.get::<Trimmed>().unwrap().0.borrow_mut())
}
