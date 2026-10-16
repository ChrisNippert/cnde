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
    state::State,
    window::with_state,
};

pub type WorkspaceId = u64;

pub struct Workspace {
    #[allow(dead_code)] // identifies workspaces to ext-workspace clients (next)
    pub id: WorkspaceId,
    /// Bottom to top.
    pub windows: Vec<Window>,
}

pub struct Workspaces {
    pub list: Vec<Workspace>,
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
    carried: Option<Window>,
}

/// A three-finger swipe in progress.
pub struct Swipe {
    start: f64,
    /// Total horizontal finger travel so far.
    dx: f64,
    /// Recent (time, dx) samples, for release velocity.
    samples: Vec<(Instant, f64)>,
    position: f64,
}

impl Workspaces {
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

    fn push_empty(&mut self) {
        self.list.push(Workspace {
            id: self.next_id,
            windows: Vec::new(),
        });
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

    fn index_of(&self, window: &Window) -> Option<usize> {
        self.list.iter().position(|ws| ws.windows.contains(window))
    }

    pub fn remove_window(&mut self, window: &Window) {
        for ws in &mut self.list {
            ws.windows.retain(|w| w != window);
        }
    }

    /// Move `window` to the top of its workspace's stacking order.
    pub fn raise(&mut self, window: &Window) {
        if let Some(index) = self.index_of(window) {
            let windows = &mut self.list[index].windows;
            windows.retain(|w| w != window);
            windows.push(window.clone());
        }
    }

    fn last_valid(&self) -> usize {
        self.list.len().saturating_sub(1)
    }
}

impl Swipe {
    fn update(&mut self, dx: f64, now: Instant, min: f64, max: f64) {
        self.dx += dx;
        self.samples.push((now, self.dx));
        let cutoff = now - Duration::from_millis(100);
        self.samples.retain(|(t, _)| *t >= cutoff);
        // Natural: content follows the fingers, so fingers moving left
        // (negative dx) reveal the next workspace.
        let raw = self.start - self.dx / presets::SWIPE_DISTANCE;
        self.position = raw.clamp(min, max);
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
        -(last.1 - first.1) / presets::SWIPE_DISTANCE / dt
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

    /// Keep the strip tidy, GNOME-style: drop empty workspaces nobody is
    /// looking at, keep enough to fill every monitor, and always have one
    /// empty workspace at the end.
    fn normalize_workspaces(&mut self) {
        let (outputs, main) = self.strip_outputs();
        let right_of_main = outputs.len().saturating_sub(main + 1);
        let ws = &mut self.workspaces;
        let visible_end = ws.active + right_of_main; // inclusive
        let visible_start = ws.active.saturating_sub(main);

        // Remove hidden empty workspaces (from the back, so indices hold).
        let mut i = ws.list.len();
        while i > 0 {
            i -= 1;
            let visible = (visible_start..=visible_end).contains(&i);
            let is_last = i + 1 == ws.list.len();
            if ws.list[i].windows.is_empty() && !visible && !is_last {
                ws.list.remove(i);
                if i < ws.active {
                    ws.active -= 1;
                }
            }
        }
        // Fill every monitor, plus one trailing empty workspace.
        let visible_end = ws.active + right_of_main;
        while ws.list.len() <= visible_end {
            ws.push_empty();
        }
        if ws.list.last().is_some_and(|last| !last.windows.is_empty()) {
            ws.push_empty();
        }
        // More than one empty workspace past the visible range: trim.
        while ws.list.len() > visible_end + 2
            && ws.list[ws.list.len() - 1].windows.is_empty()
            && ws.list[ws.list.len() - 2].windows.is_empty()
        {
            ws.list.pop();
        }
    }

    /// Record every visible window's position relative to its monitor, so
    /// it can be drawn (and later re-placed) wherever its workspace goes.
    fn store_local_positions(&mut self) {
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
            swipe.update(dx, Instant::now(), 0.0, max);
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

    /// Map exactly the windows of visible workspaces into the space, at
    /// their monitor's origin plus their local position.
    fn rebuild_space(&mut self) {
        let mapped: Vec<Window> = self.space.elements().cloned().collect();
        for window in &mapped {
            self.space.unmap_elem(window);
        }
        let (outputs, _) = self.strip_outputs();
        for output in outputs {
            let (Some(index), Some(area)) = (
                self.workspace_on(&output),
                self.space.output_geometry(&output),
            ) else {
                continue;
            };
            let windows = self.workspaces.list[index].windows.clone();
            for window in windows {
                let local = with_state(&window, |s| s.local);
                // A workspace moving to a smaller monitor: keep windows reachable.
                let size = window.geometry().size;
                let max_x = (area.size.w - 64).max(0);
                let max_y = (area.size.h - 32).max(0);
                let local = Point::from((
                    local.x.clamp(-(size.w - 64).max(0), max_x),
                    local.y.clamp(0, max_y),
                ));
                self.space.map_element(window, area.loc + local, false);
            }
        }
        // Maximized/snapped windows refit their (possibly new) monitor.
        self.relayout_after_output_change();
    }

    /// What `output` should show while the strip is moving: each window of
    /// every workspace overlapping the view, bottom to top, with its rect.
    pub fn moving_layout(
        &self,
        output: &Output,
        now: Instant,
    ) -> Option<Vec<(Window, Rectangle<f64, Logical>)>> {
        let motion = self.workspaces.motion.as_ref()?;
        let (outputs, main) = self.strip_outputs();
        let i = outputs.iter().position(|o| o == output)?;
        let area = self.space.output_geometry(output)?;
        let view = self.workspaces.position(now) + i as f64 - main as f64;

        let mut layout = Vec::new();
        let first = view.floor() as isize;
        for index in [first, first + 1] {
            if index < 0 || index as usize >= self.workspaces.list.len() {
                continue;
            }
            let offset = (index as f64 - view) * area.size.w as f64;
            if offset.abs() >= area.size.w as f64 {
                continue;
            }
            for window in &self.workspaces.list[index as usize].windows {
                if motion.carried.as_ref() == Some(window) {
                    continue;
                }
                let local = with_state(window, |s| s.local).to_f64();
                let loc = area.loc.to_f64() + local + Point::from((offset, 0.0));
                layout.push((
                    window.clone(),
                    Rectangle::new(loc, window.geometry().size.to_f64()),
                ));
            }
        }
        // The carried window stays put, on top, on its own monitor.
        if let Some(window) = &motion.carried
            && self.output_for(window).as_ref() == Some(output)
        {
            let local = with_state(window, |s| s.local).to_f64();
            layout.push((
                window.clone(),
                Rectangle::new(area.loc.to_f64() + local, window.geometry().size.to_f64()),
            ));
        }
        Some(layout)
    }
}
