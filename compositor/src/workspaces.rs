//! The workspace strip (ARCHITECTURE.md §5.2): one global row of dynamic
//! workspaces, with monitors as side-by-side viewports onto consecutive ones.
//!
//! The space (`State::space`) holds only the windows of currently visible
//! workspaces, at their real positions. Hidden windows keep a position
//! relative to their monitor (`WindowState::local`). While the strip is
//! moving (key press or swipe), every monitor is drawn from this model at an
//! offset; the space is rebuilt once the motion settles.

use std::time::{Duration, Instant};

use smithay::{
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle},
};
use tracing::debug;

use crate::{
    animation::{Animation, Params, presets},
    effects::{PendingSize, lerp_rect},
    render::{Placed, Zoom},
    state::State,
    window::{Placement, placement_rect, with_state},
};

pub type WorkspaceId = u64;

/// Generic over the window type only so the strip logic can be unit-tested
/// with plain numbers; the compositor uses `Window`.
pub struct Workspace<W = Window> {
    /// Stable identity for ext-workspace clients across renumbering.
    pub id: WorkspaceId,
    /// Bottom to top.
    pub windows: Vec<W>,
    /// Floating or tiling (Super+T); `None` follows `tiling.layout`.
    pub layout: Option<cnde_config::WindowLayout>,
    /// Main window share set with Super+[ / ]; `None` uses the config's.
    pub master_ratio: Option<f64>,
    /// Tiling order, main window first; see layout.rs.
    pub tiles: Vec<W>,
}

impl<W> Workspace<W> {
    pub fn new(id: WorkspaceId, windows: Vec<W>) -> Self {
        Self {
            id,
            windows,
            layout: None,
            master_ratio: None,
            tiles: Vec::new(),
        }
    }
}

pub struct Workspaces<W = Window> {
    pub list: Vec<Workspace<W>>,
    /// Index of the workspace on the main monitor.
    pub active: usize,
    next_id: WorkspaceId,
    pub motion: Option<StripMotion>,
}

/// The strip sliding between two resting positions.
pub struct StripMotion {
    /// Strip position (in workspaces) when the current leg started.
    from: f64,
    /// Where it comes to rest.
    pub to: usize,
    /// `None` while a finger swipe drives the position directly.
    anim: Option<Animation>,
    pub swipe: Option<Swipe>,
    /// Super+Alt+Shift: this window rides along, fixed on screen.
    pub carried: Option<Window>,
}

/// A three-finger swipe in progress.
pub struct Swipe {
    start: f64,
    /// Total horizontal finger travel so far.
    dx: f64,
    /// Recent (time, dx) samples, for release velocity.
    samples: Vec<(Instant, f64)>,
    position: f64,
    /// Finger travel for one workspace (config).
    distance: f64,
}

impl<W: PartialEq + Clone> Workspaces<W> {
    pub fn new() -> Self {
        let mut workspaces = Self {
            list: Vec::new(),
            active: 0,
            next_id: 1,
            motion: None,
        };
        workspaces.push_empty();
        workspaces
    }

    /// Insert an empty workspace at the start of the strip, keeping the
    /// same workspace active.
    fn push_empty_front(&mut self) {
        self.list
            .insert(0, Workspace::new(self.next_id, Vec::new()));
        self.next_id += 1;
        self.active += 1;
    }

    fn push_empty(&mut self) {
        self.list.push(Workspace::new(self.next_id, Vec::new()));
        self.next_id += 1;
    }

    /// The strip's current position, in workspaces (fractional mid-motion).
    pub fn position(&self, now: Instant) -> f64 {
        match &self.motion {
            None => self.active as f64,
            Some(motion) => match (&motion.swipe, &motion.anim) {
                (Some(swipe), _) => swipe.position,
                (None, Some(anim)) => anim.value_at(now),
                (None, None) => motion.from,
            },
        }
    }

    pub fn is_moving(&self) -> bool {
        self.motion.is_some()
    }

    pub(crate) fn index_of(&self, window: &W) -> Option<usize> {
        self.list.iter().position(|ws| ws.windows.contains(window))
    }

    pub fn remove_window(&mut self, window: &W) {
        for ws in &mut self.list {
            ws.windows.retain(|w| w != window);
            ws.tiles.retain(|w| w != window);
        }
    }

    /// Move `window` to the top of its workspace's stacking order.
    pub fn raise(&mut self, window: &W) {
        if let Some(index) = self.index_of(window) {
            // Pin the tiling order first: it mustn't follow the stacking.
            self.list[index].sync_tiles();
            let windows = &mut self.list[index].windows;
            windows.retain(|w| w != window);
            windows.push(window.clone());
        }
    }

    /// Keep the strip tidy: drop empty workspaces nobody is looking at, keep
    /// enough to fill every monitor, and keep one spare empty workspace past
    /// each end (one more than GNOME's trailing one), so sliding either way
    /// from the edge brings in a fresh workspace instead of doing nothing.
    /// `main` monitors sit left of the main one and `right_of_main` right of it.
    pub fn normalize(&mut self, main: usize, right_of_main: usize) {
        let ws = self;
        let visible = |active: usize, i: usize| i + main >= active && i <= active + right_of_main;

        // Remove hidden empty workspaces, except the spares at each end
        // (back to front, so indices hold).
        let mut i = ws.list.len();
        while i > 0 {
            i -= 1;
            let is_end = i == 0 || i + 1 == ws.list.len();
            if ws.list[i].windows.is_empty() && !visible(ws.active, i) && !is_end {
                ws.list.remove(i);
                if i < ws.active {
                    ws.active -= 1;
                }
            }
        }

        // Front: every monitor left of main has a workspace, plus a spare
        // empty one beyond the leftmost.
        while ws.active < main + 1 || !ws.list[0].windows.is_empty() {
            ws.push_empty_front();
        }
        while ws.active > main + 1 && ws.list[0].windows.is_empty() && ws.list[1].windows.is_empty()
        {
            ws.list.remove(0);
            ws.active -= 1;
        }

        // Back: every monitor right of main, plus a spare empty one.
        let visible_end = ws.active + right_of_main;
        while ws.list.len() <= visible_end + 1 || !ws.list[ws.list.len() - 1].windows.is_empty() {
            ws.push_empty();
        }
        while ws.list.len() > visible_end + 2
            && ws.list[ws.list.len() - 1].windows.is_empty()
            && ws.list[ws.list.len() - 2].windows.is_empty()
        {
            ws.list.pop();
        }
    }

    fn last_valid(&self) -> usize {
        self.list.len().saturating_sub(1)
    }
}

impl Swipe {
    fn update(&mut self, dx: f64, now: Instant, last_valid: f64) {
        self.dx += dx;
        self.samples.push((now, self.dx));
        let cutoff = now - Duration::from_millis(100);
        self.samples.retain(|(t, _)| *t >= cutoff);
        // Natural: content follows the fingers, so fingers moving left
        // (negative dx) reveal the next workspace.
        let raw = self.start - self.dx / self.distance;
        // One swipe moves at most one workspace: the strip stops at the
        // neighbour rather than overshooting and snapping back on release.
        let anchor = self.start.round();
        self.position = raw.clamp(anchor - 1.0, anchor + 1.0).clamp(0.0, last_valid);
    }

    /// Workspaces per second at release.
    fn velocity(&self) -> f64 {
        let (Some(first), Some(last)) = (self.samples.first(), self.samples.last()) else {
            return 0.0;
        };
        let dt = last.0.saturating_duration_since(first.0).as_secs_f64();
        if dt < 0.005 {
            return 0.0;
        }
        -(last.1 - first.1) / self.distance / dt
    }
}

impl State {
    /// Monitors in strip order (left to right), and the main monitor's index.
    pub fn strip_outputs(&self) -> (Vec<Output>, usize) {
        let mut outputs: Vec<(Output, Point<i32, Logical>)> = self
            .space
            .outputs()
            .filter_map(|o| Some((o.clone(), self.space.output_geometry(o)?.loc)))
            .collect();
        // Side by side by x; stacked monitors fall back to reading order.
        outputs.sort_by_key(|(_, loc)| (loc.x, loc.y));
        let outputs: Vec<Output> = outputs.into_iter().map(|(o, _)| o).collect();
        let main = outputs
            .iter()
            .position(|o| self.config.output(&o.name()).is_some_and(|c| c.main))
            .unwrap_or(0);
        (outputs, main)
    }

    /// Index of the workspace showing on `output` when the strip is at rest.
    pub fn workspace_on(&self, output: &Output) -> Option<usize> {
        let (outputs, main) = self.strip_outputs();
        let i = outputs.iter().position(|o| o == output)?;
        let index = self.workspaces.active as isize + i as isize - main as isize;
        (index >= 0 && (index as usize) < self.workspaces.list.len()).then_some(index as usize)
    }

    /// Put a newly mapped window on the workspace showing where it opened.
    pub fn add_window_to_workspace(&mut self, window: &Window) {
        let index = self
            .output_for(window)
            .and_then(|o| self.workspace_on(&o))
            .unwrap_or(self.workspaces.active);
        self.workspaces.remove_window(window);
        self.workspaces.list[index].windows.push(window.clone());
        // Needed if it opens mid-motion, when everything is drawn from
        // local positions.
        if let (Some(loc), Some(origin)) = (
            self.space.element_location(window),
            self.output_for(window)
                .and_then(|o| self.space.output_geometry(&o))
                .map(|g| g.loc),
        ) {
            with_state(window, |s| s.local = loc - origin);
        }
        if !self.workspaces.is_moving() {
            self.normalize_workspaces();
        }
    }

    /// Monitors were added, removed or moved: re-derive which workspace each
    /// visible window is on and lay the strip out again.
    pub fn workspaces_after_output_change(&mut self) {
        if self.workspaces.is_moving() {
            return;
        }
        self.store_local_positions();
        self.sync_workspace_membership();
        self.normalize_workspaces();
        self.rebuild_space();
    }

    pub fn remove_window_from_workspaces(&mut self, window: &Window) {
        self.workspaces.remove_window(window);
        if !self.workspaces.is_moving() {
            self.normalize_workspaces();
        }
    }

    /// After windows move between monitors (dragging), make each visible
    /// window belong to the workspace on the monitor it's now on.
    pub fn sync_workspace_membership(&mut self) {
        if self.workspaces.is_moving() {
            return;
        }
        let visible: Vec<Window> = self.space.elements().cloned().collect();
        let mut changed = false;
        for window in visible {
            let Some(target) = self.output_for(&window).and_then(|o| self.workspace_on(&o)) else {
                continue;
            };
            if self.workspaces.index_of(&window) != Some(target) {
                self.workspaces.remove_window(&window);
                self.workspaces.list[target].windows.push(window);
                changed = true;
            }
        }
        if changed {
            self.normalize_workspaces();
        }
    }

    pub(crate) fn normalize_workspaces(&mut self) {
        let (outputs, main) = self.strip_outputs();
        let right_of_main = outputs.len().saturating_sub(main + 1);
        self.workspaces.normalize(main, right_of_main);
    }

    /// Record every visible window's position relative to its monitor, so
    /// it can be drawn (and later re-placed) wherever its workspace goes.
    pub(crate) fn store_local_positions(&mut self) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            let (Some(loc), Some(output)) = (
                self.space.element_location(&window),
                self.output_for(&window),
            ) else {
                continue;
            };
            let Some(origin) = self.space.output_geometry(&output).map(|g| g.loc) else {
                continue;
            };
            with_state(&window, |s| s.local = loc - origin);
        }
    }

    /// Super+Alt+Left/Right: slide the strip one workspace. `step` +1 moves
    /// content left (next workspace), -1 right. With `carry`, the focused
    /// window comes along.
    pub fn step_workspace(&mut self, step: isize, carry: bool) {
        let now = Instant::now();
        if !self.workspaces.is_moving() {
            // Make sure the spare workspaces at both ends exist.
            self.normalize_workspaces();
        }
        let current_target = self
            .workspaces
            .motion
            .as_ref()
            .map_or(self.workspaces.active, |m| m.to);
        let target = current_target as isize + step;
        if target < 0 || target as usize > self.workspaces.last_valid() {
            return;
        }
        let target = target as usize;
        let from = self.workspaces.position(now);

        let carried = if carry { self.focused_window() } else { None };
        if !self.workspaces.is_moving() {
            self.store_local_positions();
        }
        if let Some(window) = &carried {
            // It stays on its monitor, so it joins whichever workspace lands
            // there; drawn fixed on screen meanwhile.
            let (outputs, main) = self.strip_outputs();
            let monitor = self
                .output_for(window)
                .and_then(|o| outputs.iter().position(|x| x == &o))
                .unwrap_or(main);
            let dest = (target as isize + monitor as isize - main as isize).max(0) as usize;
            while self.workspaces.list.len() <= dest {
                self.workspaces.push_empty();
            }
            self.workspaces.remove_window(window);
            self.workspaces.list[dest].windows.push(window.clone());
        }

        let duration_scale = (target as f64 - from).abs().clamp(0.4, 1.5);
        let params = Params {
            duration: presets::WORKSPACE_SWITCH.duration.mul_f64(duration_scale),
            curve: presets::WORKSPACE_SWITCH.curve,
        };
        let carried = carried.or_else(|| {
            self.workspaces
                .motion
                .as_mut()
                .and_then(|m| m.carried.take())
        });
        self.workspaces.motion = Some(StripMotion {
            from,
            to: target,
            anim: Some(Animation::new(from, target as f64, params)),
            swipe: None,
            carried,
        });
        debug!(from, to = target, "workspace switch");
        self.queue_redraw();
    }

    pub fn swipe_begin(&mut self) {
        if !self.workspaces.is_moving() {
            self.normalize_workspaces();
        }
        let now = Instant::now();
        let start = self.workspaces.position(now);
        if !self.workspaces.is_moving() {
            self.store_local_positions();
        }
        let to = self
            .workspaces
            .motion
            .as_ref()
            .map_or(self.workspaces.active, |m| m.to);
        self.workspaces.motion = Some(StripMotion {
            from: start,
            to,
            anim: None,
            swipe: Some(Swipe {
                start,
                dx: 0.0,
                samples: vec![(now, 0.0)],
                position: start,
                distance: self.config.input.touchpad.workspace_swipe_distance,
            }),
            carried: None,
        });
    }

    pub fn swipe_update(&mut self, dx: f64) {
        let max = self.workspaces.last_valid() as f64;
        if let Some(swipe) = self
            .workspaces
            .motion
            .as_mut()
            .and_then(|m| m.swipe.as_mut())
        {
            swipe.update(dx, Instant::now(), max);
            self.queue_redraw();
        }
    }

    /// Fingers lifted: settle on the nearest workspace in the direction the
    /// swipe was heading (a flick goes to the next one even if short).
    pub fn swipe_end(&mut self, cancelled: bool) {
        let Some(motion) = &mut self.workspaces.motion else {
            return;
        };
        let Some(swipe) = motion.swipe.take() else {
            return;
        };
        let last = self.workspaces.list.len().saturating_sub(1) as f64;
        let projected = swipe.position + swipe.velocity() * presets::SWIPE_FLICK_SECONDS;
        let target = if cancelled {
            swipe.start.round()
        } else {
            projected
                .round()
                .clamp(swipe.start.round() - 1.0, swipe.start.round() + 1.0)
                .clamp(0.0, last)
        };
        let distance = (target - swipe.position).abs();
        let params = Params {
            duration: presets::WORKSPACE_SWITCH
                .duration
                .mul_f64(distance.clamp(0.3, 1.0)),
            curve: presets::WORKSPACE_SWITCH.curve,
        };
        motion.from = swipe.position;
        motion.to = target as usize;
        motion.anim = Some(Animation::new(swipe.position, target, params));
        self.queue_redraw();
    }

    /// Called every loop iteration: once a motion has settled, rebuild the
    /// space for the new resting position.
    pub fn finish_workspace_motion(&mut self, now: Instant) {
        let done =
            self.workspaces.motion.as_ref().is_some_and(|m| {
                m.swipe.is_none() && m.anim.as_ref().is_none_or(|a| a.is_done(now))
            });
        if !done {
            return;
        }
        let Some(motion) = self.workspaces.motion.take() else {
            return;
        };
        self.workspaces.active = motion.to;
        self.rebuild_space();
        self.normalize_workspaces();
        self.rebuild_space();

        // Keep focus if the focused window is still on screen; otherwise
        // the top window on the main monitor's workspace.
        let focused_visible = self
            .focused_window()
            .is_some_and(|w| self.space.elements().any(|e| e == &w));
        if let Some(carried) = &motion.carried {
            self.focus_window(Some(carried));
        } else if !focused_visible {
            let top = self.workspaces.list[self.workspaces.active]
                .windows
                .last()
                .cloned();
            self.focus_window(top.as_ref());
        }
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// Monitor geometry and panel-free area of strip slot `slot`, an index
    /// into `outputs`. Slots past either end are virtual off-screen
    /// continuations, the size of the nearest real monitor, so workspaces
    /// slide in from and out to somewhere sensible.
    pub(crate) fn slot_areas(
        &self,
        outputs: &[Output],
        slot: isize,
    ) -> Option<(Rectangle<i32, Logical>, Rectangle<i32, Logical>)> {
        let last = outputs.len() as isize - 1;
        let output = outputs.get(slot.clamp(0, last.max(0)) as usize)?;
        let geo = self.space.output_geometry(output)?;
        let area = self.usable_area(output).unwrap_or(geo);
        let beyond = if slot < 0 { slot } else { (slot - last).max(0) };
        let shift = Point::from((geo.size.w * beyond as i32, 0));
        Some((
            Rectangle::new(geo.loc + shift, geo.size),
            Rectangle::new(area.loc + shift, area.size),
        ))
    }

    /// Where `window` would rest if its workspace were shown on `slot`:
    /// floating windows where their workspace's layout puts them (floating:
    /// their local position, kept reachable on a smaller monitor; tiling:
    /// their tile), maximized/snapped/fullscreen ones fitted to that monitor.
    pub(crate) fn rest_rect(
        &self,
        window: &Window,
        outputs: &[Output],
        slot: isize,
    ) -> Option<Rectangle<i32, Logical>> {
        let (geo, area) = self.slot_areas(outputs, slot)?;
        Some(match Self::placement_of(window) {
            Placement::Floating => self
                .layout_rest(window, geo, area)
                .unwrap_or_else(|| floating_rest(window, geo)),
            Placement::Fullscreen => geo,
            placement => crate::decorations::content_rect(
                placement_rect(placement, area),
                crate::decorations::titlebar_height(window),
            ),
        })
    }

    /// Map exactly the windows of visible workspaces into the space, where
    /// `rest_rect` says, so the end of a slide is exactly where windows
    /// settle. Windows that must change size (maximized onto a different
    /// monitor) are configured and shown squeezed until they redraw.
    pub(crate) fn rebuild_space(&mut self) {
        let mapped: Vec<Window> = self.space.elements().cloned().collect();
        for window in &mapped {
            self.space.unmap_elem(window);
        }
        let (outputs, _) = self.strip_outputs();
        for (slot, output) in outputs.iter().enumerate() {
            let Some(index) = self.workspace_on(output) else {
                continue;
            };
            let windows = self.workspaces.list[index].windows.clone();
            for window in windows {
                if with_state(&window, |s| s.minimized) {
                    continue;
                }
                let Some(rest) = self.rest_rect(&window, &outputs, slot as isize) else {
                    continue;
                };
                self.space.map_element(window.clone(), rest.loc, false);
                let old_size = window.geometry().size;
                let placement = Self::placement_of(&window);
                // Tiles are sized by their layout, and may have joined or
                // left one (the overview moves windows between workspaces).
                let tiled = self.is_tiled(&window);
                let resize = (placement != Placement::Floating || tiled) && old_size != rest.size;
                let Some(toplevel) = window.toplevel() else {
                    continue;
                };
                toplevel.with_pending_state(|s| {
                    if placement == Placement::Floating {
                        crate::layout::set_tiled(&mut s.states, tiled);
                    }
                    if resize {
                        s.size = Some(rest.size);
                    }
                });
                if !toplevel.is_initial_configure_sent() {
                    continue;
                }
                if let Some(serial) = toplevel.send_pending_configure()
                    && resize
                {
                    with_state(&window, |s| {
                        s.pending_size = Some(PendingSize::new(rest.size, old_size, serial));
                    });
                }
            }
        }
        // Why: the space only works out which monitors each window touches
        // in refresh(). Freshly mapped windows count as on no monitor until
        // then, and this can run after the iteration's refresh, so they'd
        // vanish for one frame (the flash at the end of a slide).
        self.space.refresh();
    }

    /// What `output` should show while the strip is moving. Every window
    /// glides in global coordinates between where it would rest on the
    /// neighbouring slots, so it crosses monitor borders at one speed and
    /// morphs to fit monitors of different sizes; whatever overlaps
    /// `output` is drawn there. Bottom to top.
    pub fn moving_layout(&self, output: &Output, now: Instant) -> Option<Vec<Placed>> {
        let motion = self.workspaces.motion.as_ref()?;
        let (outputs, main) = self.strip_outputs();
        let output_geo = self.space.output_geometry(output)?.to_f64();
        let position = self.workspaces.position(now);

        let mut layout = Vec::new();
        let lowest = position.floor() as isize - main as isize - 1;
        let highest = position.ceil() as isize + (outputs.len() - main) as isize + 1;
        for index in lowest.max(0)..=highest.min(self.workspaces.list.len() as isize - 1) {
            // Fractional slot this workspace is passing through.
            let slot = index as f64 - position + main as f64;
            let (from, t) = (slot.floor() as isize, slot - slot.floor());
            for window in &self.workspaces.list[index as usize].windows {
                if motion.carried.as_ref() == Some(window) || with_state(window, |s| s.minimized) {
                    continue;
                }
                let (Some(a), Some(b)) = (
                    self.rest_rect(window, &outputs, from),
                    self.rest_rect(window, &outputs, from + 1),
                ) else {
                    continue;
                };
                let rect = lerp_rect(a.to_f64(), b.to_f64(), t);
                if rect.overlaps(output_geo) {
                    layout.push(Placed {
                        window: window.clone(),
                        rect,
                        zoom: Zoom::IDENTITY,
                        workspace: index as usize,
                    });
                }
            }
        }
        // The carried window stays where it is, on top.
        if let Some(window) = &motion.carried
            && let Some(rect) = self.space.element_geometry(window).map(|g| g.to_f64())
            && rect.overlaps(output_geo)
        {
            layout.push(Placed {
                window: window.clone(),
                rect,
                zoom: Zoom::IDENTITY,
                workspace: motion.to,
            });
        }
        Some(layout)
    }
}

/// Where a floating `window` rests on a monitor at `geo`: its local
/// position, kept reachable on a smaller monitor.
pub(crate) fn floating_rest(
    window: &Window,
    geo: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let size = window.geometry().size;
    let local = with_state(window, |s| s.local);
    let local = Point::from((
        local
            .x
            .clamp(-(size.w - 64).max(0), (geo.size.w - 64).max(0)),
        local.y.clamp(0, (geo.size.h - 32).max(0)),
    ));
    Rectangle::new(geo.loc + local, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A strip from occupancy: `true` = has a window (numbered by index).
    fn strip(occupied: &[bool], active: usize) -> Workspaces<u32> {
        let mut ws = Workspaces::<u32>::new();
        ws.list.clear();
        for (i, &full) in occupied.iter().enumerate() {
            ws.list.push(Workspace::new(
                i as u64,
                if full { vec![i as u32] } else { vec![] },
            ));
        }
        ws.active = active;
        ws
    }

    fn shape(ws: &Workspaces<u32>) -> (Vec<bool>, usize) {
        (
            ws.list.iter().map(|w| !w.windows.is_empty()).collect(),
            ws.active,
        )
    }

    fn swipe(start: f64) -> Swipe {
        Swipe {
            start,
            dx: 0.0,
            samples: vec![(Instant::now(), 0.0)],
            position: start,
            distance: 900.0,
        }
    }

    #[test]
    fn swipe_follows_fingers_naturally() {
        let mut s = swipe(2.0);
        s.update(-450.0, Instant::now(), 10.0); // fingers left, half a workspace
        assert!((s.position - 2.5).abs() < 1e-9);
    }

    #[test]
    fn swipe_stops_at_the_neighbour() {
        let mut s = swipe(2.0);
        s.update(-5000.0, Instant::now(), 10.0);
        assert_eq!(s.position, 3.0);
        s.update(10000.0, Instant::now(), 10.0);
        assert_eq!(s.position, 1.0);
    }

    #[test]
    fn swipe_respects_strip_ends() {
        let mut s = swipe(0.0);
        s.update(5000.0, Instant::now(), 10.0);
        assert_eq!(s.position, 0.0);
    }

    #[test]
    fn fresh_single_monitor_gets_spares_both_sides() {
        let mut ws = strip(&[true], 0);
        ws.normalize(0, 0);
        assert_eq!(shape(&ws), (vec![false, true, false], 1));
    }

    #[test]
    fn two_monitors_main_left() {
        // Main shows a window, the right monitor its own workspace.
        let mut ws = strip(&[true, true], 0);
        ws.normalize(0, 1);
        // spare | main | right | spare
        assert_eq!(shape(&ws), (vec![false, true, true, false], 1));
    }

    #[test]
    fn hidden_empties_are_removed_but_ends_kept() {
        // _ A _ _ B _ _ with main on A: the gaps go, spares stay.
        let mut ws = strip(&[false, true, false, false, true, false, false], 1);
        ws.normalize(0, 0);
        assert_eq!(shape(&ws), (vec![false, true, true, false], 1));
    }

    #[test]
    fn visible_empty_workspace_is_kept() {
        // Looking at an empty workspace between two full ones.
        let mut ws = strip(&[false, true, false, true, false], 2);
        ws.normalize(0, 0);
        assert_eq!(shape(&ws), (vec![false, true, false, true, false], 2));
    }

    #[test]
    fn monitor_left_of_main_gets_a_workspace() {
        let mut ws = strip(&[true], 0);
        ws.normalize(1, 0); // one monitor left of main
        // spare | left | main | spare
        assert_eq!(shape(&ws), (vec![false, false, true, false], 2));
    }
}
