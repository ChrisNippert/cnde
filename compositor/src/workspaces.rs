//! The workspace strip (ARCHITECTURE.md §5.2): one global row of dynamic
//! workspaces, with monitors as side-by-side viewports onto consecutive ones.
//!
//! The space (`State::space`) holds only the windows of currently visible
//! workspaces, at their real positions. Hidden windows keep a position
//! relative to their monitor (`WindowState::local`). While the strip is
//! moving (key press or swipe), every monitor is drawn from this model at an
//! offset; the space is rebuilt once the motion settles.
//!
//! With `workspaces.mode = "individual"` every monitor has a strip of its
//! own instead (a [`Lane`]): `Workspaces::list` then only stores the
//! workspaces, and each lane orders some of them by id. Switching moves the
//! lane of the monitor used last, `State::current_output`. The strip logic
//! (normalize, motion, swipes) is shared: a lane is normalized as a strip
//! with one monitor, and `State::strip_view` hands drawing code either the
//! global strip or one lane in the same shape.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, Scale, Size},
};
use tracing::debug;

use crate::{
    animation::{Animation, Params, presets},
    effects::{PendingSize, lerp_rect},
    render::{Placed, Zoom},
    snapshot::Snapshot,
    state::State,
    window::{Placement, window_placement_rect, with_state},
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
    /// The dwindle style's splits; see layout.rs.
    pub splits: crate::layout::SplitTree<W>,
}

impl<W> Workspace<W> {
    pub fn new(id: WorkspaceId, windows: Vec<W>) -> Self {
        Self {
            id,
            windows,
            layout: None,
            master_ratio: None,
            tiles: Vec::new(),
            splits: crate::layout::SplitTree::default(),
        }
    }
}

pub struct Workspaces<W = Window> {
    pub list: Vec<Workspace<W>>,
    /// Index of the workspace on the main monitor.
    pub active: usize,
    next_id: WorkspaceId,
    pub motion: Option<StripMotion>,
    /// Individual mode (and then `lanes` has the strips, by output name).
    pub individual: bool,
    pub lanes: HashMap<String, Lane>,
}

/// One monitor's strip in individual mode.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lane {
    /// Left to right; ids of workspaces in `Workspaces::list`.
    pub ids: Vec<WorkspaceId>,
    /// Position in `ids` of the workspace on the monitor.
    pub active: usize,
}

/// The strip sliding between two resting positions.
pub struct StripMotion {
    /// Individual mode: the monitor whose strip moves. `from`, `to` and the
    /// swipe then count positions in that lane.
    pub lane: Option<String>,
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
            individual: false,
            lanes: HashMap::new(),
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

    /// Strip position of `lane` (`None`: the global strip) at rest or in
    /// motion.
    pub fn position_in(&self, lane: Option<&str>, now: Instant) -> f64 {
        match &self.motion {
            Some(_) => self.position(now),
            None => self.rest_in(lane) as f64,
        }
    }

    /// Where `lane` (`None`: the global strip) is heading or resting.
    pub fn target_in(&self, lane: Option<&str>) -> usize {
        self.motion.as_ref().map_or(self.rest_in(lane), |m| m.to)
    }

    fn rest_in(&self, lane: Option<&str>) -> usize {
        lane.and_then(|name| self.lanes.get(name))
            .map_or(self.active, |l| l.active)
    }

    /// Number of workspaces in `lane` (`None`: the global strip).
    pub fn len_of(&self, lane: Option<&str>) -> usize {
        lane.and_then(|name| self.lanes.get(name))
            .map_or(self.list.len(), |l| l.ids.len())
    }

    /// Index into `list` of the workspace at `position` of `lane` (`None`:
    /// the global strip).
    pub fn index_at(&self, lane: Option<&str>, position: usize) -> Option<usize> {
        match lane.and_then(|name| self.lanes.get(name)) {
            None => (position < self.list.len()).then_some(position),
            Some(lane) => {
                let id = lane.ids.get(position)?;
                self.list.iter().position(|ws| ws.id == *id)
            }
        }
    }

    /// The lane holding `list[index]`, and its position there.
    pub fn lane_of(&self, index: usize) -> Option<(&str, usize)> {
        let id = self.list.get(index)?.id;
        self.lanes.iter().find_map(|(name, lane)| {
            let position = lane.ids.iter().position(|i| *i == id)?;
            Some((name.as_str(), position))
        })
    }

    /// A monitor's lane, with one empty workspace if it's new.
    pub fn ensure_lane(&mut self, name: &str) {
        if !self.lanes.contains_key(name) {
            self.push_empty();
            let id = self.list[self.list.len() - 1].id;
            self.lanes.insert(
                name.to_string(),
                Lane {
                    ids: vec![id],
                    active: 0,
                },
            );
        }
    }

    /// Tidy one lane exactly like the global strip with a single monitor.
    pub fn normalize_lane(&mut self, name: &str) {
        let Some(lane) = self.lanes.get(name) else {
            return;
        };
        let (ids, active) = (lane.ids.clone(), lane.active);
        // Where its workspaces sit in `list`, which stays as it was unless
        // the lane changes (indices held elsewhere, like the overview's
        // drag, then stay valid).
        let mut slots: Vec<(usize, WorkspaceId)> = ids
            .iter()
            .filter_map(|id| Some((self.list.iter().position(|ws| ws.id == *id)?, *id)))
            .collect();
        slots.sort();
        let mut taken: Vec<Workspace<W>> = Vec::new();
        for (i, _) in slots.iter().rev() {
            taken.push(self.list.remove(*i));
        }
        taken.sort_by_key(|ws| ids.iter().position(|id| *id == ws.id));
        let mut strip = Self {
            list: taken,
            active,
            next_id: self.next_id,
            motion: None,
            individual: false,
            lanes: HashMap::new(),
        };
        if strip.list.is_empty() {
            strip.push_empty();
        }
        strip.active = strip.active.min(strip.list.len() - 1);
        strip.normalize(0, 0);
        self.next_id = strip.next_id;
        let new_ids: Vec<WorkspaceId> = strip.list.iter().map(|ws| ws.id).collect();
        self.lanes.insert(
            name.to_string(),
            Lane {
                ids: new_ids.clone(),
                active: strip.active,
            },
        );
        // Same workspaces: back where they were.
        let mut sorted: Vec<WorkspaceId> = slots.iter().map(|(_, id)| *id).collect();
        sorted.sort();
        let mut now = new_ids.clone();
        now.sort();
        if sorted == now {
            let mut by_id: HashMap<WorkspaceId, Workspace<W>> =
                strip.list.into_iter().map(|ws| (ws.id, ws)).collect();
            for (i, id) in slots {
                if let Some(ws) = by_id.remove(&id) {
                    self.list.insert(i, ws);
                }
            }
        } else {
            self.list.extend(strip.list);
        }
    }

    /// A monitor went away: its workspaces (and windows) continue at the
    /// end of `into`'s lane.
    pub fn drop_lane(&mut self, name: &str, into: &str) {
        let Some(gone) = self.lanes.remove(name) else {
            return;
        };
        self.ensure_lane(into);
        if let Some(lane) = self.lanes.get_mut(into) {
            lane.ids.extend(gone.ids);
        }
    }

    /// Continuous to individual. `outputs` are in strip order with `main`
    /// the anchor. Each monitor keeps the workspace it shows; the main
    /// monitor's lane also takes all the others, in order: the ones before
    /// the visible block go before its workspace, the ones after, after it.
    /// Nothing moves on screen and no window changes workspace.
    pub fn become_individual(&mut self, outputs: &[String], main: usize) {
        let first = self.active as isize - main as isize;
        let ids: Vec<WorkspaceId> = self.list.iter().map(|ws| ws.id).collect();
        let at = |i: isize| usize::try_from(i).ok().and_then(|i| ids.get(i).copied());
        let end = first + outputs.len() as isize;
        let before: Vec<WorkspaceId> = (0..first).filter_map(at).collect();
        let after: Vec<WorkspaceId> = (end..ids.len() as isize).filter_map(at).collect();

        self.lanes.clear();
        for (i, name) in outputs.iter().enumerate() {
            let own = at(first + i as isize);
            let lane = if i == main {
                let mut lane = before.clone();
                let active = lane.len();
                lane.extend(own);
                lane.extend(after.iter().copied());
                Lane { ids: lane, active }
            } else {
                Lane {
                    ids: own.into_iter().collect(),
                    active: 0,
                }
            };
            self.lanes.insert(name.clone(), lane);
        }
        self.individual = true;
    }

    /// Individual to continuous: one strip of every lane's workspaces
    /// before its current one, then each monitor's current one in monitor
    /// order (so they sit side by side as the strip shows them), then the
    /// ones after.
    pub fn become_continuous(&mut self, outputs: &[String], main: usize) {
        let Some(anchor) = outputs.get(main).or(outputs.first()).cloned() else {
            return;
        };
        let stale: Vec<String> = self
            .lanes
            .keys()
            .filter(|name| !outputs.contains(name))
            .cloned()
            .collect();
        for name in stale {
            self.drop_lane(&name, &anchor);
        }
        for name in outputs {
            self.ensure_lane(name);
        }
        let (mut before, mut current, mut after) = (Vec::new(), Vec::new(), Vec::new());
        for name in outputs {
            let lane = &self.lanes[name];
            let split = lane.active.min(lane.ids.len().saturating_sub(1));
            before.extend_from_slice(&lane.ids[..split]);
            current.extend(lane.ids.get(split).copied());
            after.extend_from_slice(lane.ids.get(split + 1..).unwrap_or(&[]));
        }
        self.active = before.len() + main.min(current.len().saturating_sub(1));
        let order: Vec<WorkspaceId> = [before, current, after].concat();
        let mut old = std::mem::take(&mut self.list);
        for id in order {
            if let Some(i) = old.iter().position(|ws| ws.id == id) {
                self.list.push(old.remove(i));
            }
        }
        // Anything no lane listed (shouldn't happen) stays reachable.
        self.list.extend(old);
        self.lanes.clear();
        self.individual = false;
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
}

/// A strip as drawing code sees it: `outputs` are the monitors in strip
/// order with `main` the anchor, `order` the indices into
/// `Workspaces::list` of its workspaces left to right, and `position` the
/// (fractional) strip position of the workspace on the anchor.
pub struct StripView {
    pub outputs: Vec<Output>,
    pub main: usize,
    pub order: Vec<usize>,
    pub position: f64,
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
        if self.workspaces.individual {
            let name = output.name();
            let lane = self.workspaces.lanes.get(&name)?;
            return self.workspaces.index_at(Some(&name), lane.active);
        }
        let (outputs, main) = self.strip_outputs();
        let i = outputs.iter().position(|o| o == output)?;
        let index = self.workspaces.active as isize + i as isize - main as isize;
        (index >= 0 && (index as usize) < self.workspaces.list.len()).then_some(index as usize)
    }

    /// Individual mode: the monitor workspace keys and the overview's
    /// scroll act on: the last one clicked or focused, else the one under
    /// the pointer.
    pub fn current_output(&self) -> Option<Output> {
        self.workspace_monitor
            .as_ref()
            .and_then(|name| self.space.outputs().find(|o| o.name() == *name))
            .cloned()
            .or_else(|| self.output_under_pointer())
    }

    /// The lane that switching acts on right now (`None` in continuous mode).
    fn current_lane(&self) -> Option<String> {
        self.workspaces
            .individual
            .then(|| self.current_output().map(|o| o.name()))
            .flatten()
    }

    /// Index of the workspace "the current one" is on: the main monitor's,
    /// or the current monitor's in individual mode.
    pub fn current_workspace_index(&self) -> usize {
        let last = self.workspaces.list.len().saturating_sub(1);
        let rest = self
            .current_lane()
            .and_then(|name| {
                let at = self.workspaces.lanes.get(&name)?.active;
                self.workspaces.index_at(Some(&name), at)
            })
            .unwrap_or(self.workspaces.active);
        rest.min(last)
    }

    /// Slide so workspace `index` shows: on the main monitor (or `on`, in
    /// continuous mode), or in individual mode on the monitor whose strip
    /// it is.
    pub fn show_workspace(&mut self, index: usize, on: Option<&Output>) {
        if self.workspaces.individual {
            let Some((name, position)) = self
                .workspaces
                .lane_of(index)
                .map(|(name, position)| (name.to_string(), position))
            else {
                return;
            };
            let step = position as isize - self.workspaces.target_in(Some(&name)) as isize;
            self.workspace_monitor = Some(name);
            if step != 0 {
                self.step_workspace(step, false);
            }
            return;
        }
        let (outputs, main) = self.strip_outputs();
        let slot = on
            .and_then(|o| outputs.iter().position(|x| x == o))
            .unwrap_or(main);
        let target = index as isize + main as isize - slot as isize;
        let step = target - self.workspaces.target_in(None) as isize;
        if step != 0 {
            self.step_workspace(step, false);
        }
    }

    /// What drawing code walks to lay out `output`'s workspaces: in
    /// continuous mode the global strip, in individual mode just this
    /// monitor's.
    pub fn strip_view(&self, output: &Output, now: Instant) -> StripView {
        if !self.workspaces.individual {
            let (outputs, main) = self.strip_outputs();
            return StripView {
                outputs,
                main,
                order: (0..self.workspaces.list.len()).collect(),
                position: self.workspaces.position(now),
            };
        }
        let name = output.name();
        let order = (0..self.workspaces.len_of(Some(&name)))
            .filter_map(|p| self.workspaces.index_at(Some(&name), p))
            .collect();
        let moving = self
            .workspaces
            .motion
            .as_ref()
            .is_some_and(|m| m.lane.as_deref() == Some(name.as_str()));
        let position = if moving {
            self.workspaces.position(now)
        } else {
            self.workspaces.position_in(Some(&name), now)
        };
        StripView {
            outputs: vec![output.clone()],
            main: 0,
            order,
            position,
        }
    }

    /// Put a newly mapped window on the workspace showing where it opened.
    pub fn add_window_to_workspace(&mut self, window: &Window) {
        // Every monitor needs its workspace first: before any window opened,
        // the ones beside the main monitor may have none yet.
        if !self.workspaces.is_moving() {
            self.normalize_workspaces();
        }
        let index = self
            .output_for(window)
            .and_then(|o| self.workspace_on(&o))
            .unwrap_or(self.current_workspace_index());
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
        let names: Vec<String> = outputs.iter().map(|o| o.name()).collect();
        let individual = self.config.workspaces.mode == cnde_config::WorkspaceMode::Individual;
        let switched = !names.is_empty() && individual != self.workspaces.individual;
        if switched {
            self.switch_workspace_mode(&names, main, individual);
        }
        if !self.workspaces.individual {
            let right_of_main = outputs.len().saturating_sub(main + 1);
            self.workspaces.normalize(main, right_of_main);
        } else if !names.is_empty() {
            self.normalize_lanes(&names, main);
        }
        if switched {
            self.rebuild_space();
        }
    }

    fn normalize_lanes(&mut self, names: &[String], main: usize) {
        // A monitor that went away hands its workspaces to the main one; a
        // new one starts with an empty workspace.
        let gone: Vec<String> = (self.workspaces.lanes.keys())
            .filter(|name| !names.contains(name))
            .cloned()
            .collect();
        for name in gone {
            self.workspaces
                .drop_lane(&name, &names[main.min(names.len() - 1)]);
        }
        for name in names {
            self.workspaces.ensure_lane(name);
            self.workspaces.normalize_lane(name);
        }
    }

    /// `workspaces.mode` changed (or this is the first look at it): turn
    /// the strip into per-monitor strips or back. See `become_individual`
    /// and `become_continuous` for where the workspaces end up; every
    /// window keeps its workspace, so none is lost.
    fn switch_workspace_mode(&mut self, names: &[String], main: usize, individual: bool) {
        self.store_local_positions();
        // The motion's destination counts as where things are.
        if let Some(motion) = self.workspaces.motion.take() {
            match motion
                .lane
                .as_ref()
                .and_then(|n| self.workspaces.lanes.get_mut(n))
            {
                Some(lane) => lane.active = motion.to,
                None => self.workspaces.active = motion.to,
            }
        }
        if individual {
            self.workspaces.become_individual(names, main);
        } else {
            self.workspaces.become_continuous(names, main);
        }
        self.workspace_monitor = None;
        debug!(individual, "workspace mode switched");
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
        let carried = if carry { self.focused_window() } else { None };
        // Individual mode: only the strip of the monitor used last moves
        // (a carried window's own monitor), and one at a time.
        let lane = self
            .workspaces
            .individual
            .then(|| {
                carried
                    .as_ref()
                    .and_then(|w| self.output_for(w))
                    .or_else(|| self.current_output())
                    .map(|o| o.name())
            })
            .flatten();
        if let Some(name) = &lane {
            let elsewhere =
                (self.workspaces.motion.as_ref()).is_some_and(|m| m.lane.as_ref() != Some(name));
            if elsewhere {
                self.complete_motion();
            }
            self.workspace_monitor = Some(name.clone());
        }
        let lane = lane.as_deref();
        if !self.workspaces.is_moving() {
            // Make sure the spare workspaces at both ends exist.
            self.normalize_workspaces();
        }
        let current_target = self.workspaces.target_in(lane);
        let target = current_target as isize + step;
        if target < 0 || target as usize >= self.workspaces.len_of(lane) {
            return;
        }
        let target = target as usize;
        let from = self.workspaces.position_in(lane, now);

        if !self.workspaces.is_moving() {
            self.store_local_positions();
        }
        if let Some(window) = &carried {
            // It stays on its monitor, so it joins whichever workspace lands
            // there; drawn fixed on screen meanwhile.
            let dest = if lane.is_some() {
                self.workspaces.index_at(lane, target)
            } else {
                let (outputs, main) = self.strip_outputs();
                let monitor = self
                    .output_for(window)
                    .and_then(|o| outputs.iter().position(|x| x == &o))
                    .unwrap_or(main);
                let dest = (target as isize + monitor as isize - main as isize).max(0) as usize;
                while self.workspaces.list.len() <= dest {
                    self.workspaces.push_empty();
                }
                Some(dest)
            };
            if let Some(dest) = dest {
                self.workspaces.remove_window(window);
                self.workspaces.list[dest].windows.push(window.clone());
            }
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
        let lane = lane.map(str::to_string);
        self.workspaces.motion = Some(StripMotion {
            lane,
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
        // Individual mode: the monitor under the fingers' pointer.
        let lane = self
            .workspaces
            .individual
            .then(|| self.output_under_pointer().map(|o| o.name()))
            .flatten();
        if let Some(name) = &lane {
            let elsewhere =
                (self.workspaces.motion.as_ref()).is_some_and(|m| m.lane.as_ref() != Some(name));
            if elsewhere {
                self.complete_motion();
            }
            self.workspace_monitor = Some(name.clone());
        }
        if !self.workspaces.is_moving() {
            self.normalize_workspaces();
        }
        let now = Instant::now();
        let start = self.workspaces.position_in(lane.as_deref(), now);
        if !self.workspaces.is_moving() {
            self.store_local_positions();
        }
        let to = self.workspaces.target_in(lane.as_deref());
        self.workspaces.motion = Some(StripMotion {
            lane,
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
        let lane = self
            .workspaces
            .motion
            .as_ref()
            .and_then(|m| m.lane.as_deref());
        let max = self.workspaces.len_of(lane).saturating_sub(1) as f64;
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
        let lane = self.workspaces.motion.as_ref().and_then(|m| m.lane.clone());
        let last = self.workspaces.len_of(lane.as_deref()).saturating_sub(1) as f64;
        let Some(motion) = &mut self.workspaces.motion else {
            return;
        };
        let Some(swipe) = motion.swipe.take() else {
            return;
        };
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
        if done {
            self.complete_motion();
        }
    }

    /// End the motion in progress right now, at its destination.
    fn complete_motion(&mut self) {
        let Some(motion) = self.workspaces.motion.take() else {
            return;
        };
        match motion
            .lane
            .as_ref()
            .and_then(|name| self.workspaces.lanes.get_mut(name))
        {
            Some(lane) => lane.active = motion.to,
            None => self.workspaces.active = motion.to,
        }
        self.rebuild_space();
        self.normalize_workspaces();
        self.rebuild_space();

        // Keep focus if the focused window is still on screen; otherwise
        // the top window on the main (or current) monitor's workspace.
        let focused_visible = self
            .focused_window()
            .is_some_and(|w| self.space.elements().any(|e| e == &w));
        if let Some(carried) = &motion.carried {
            self.focus_window(Some(carried));
        } else if !focused_visible {
            let top = self.workspaces.list[self.current_workspace_index()]
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
                .unwrap_or_else(|| floating_rest(window, geo, area)),
            Placement::Fullscreen => geo,
            placement => crate::decorations::content_rect(
                window_placement_rect(window, placement, area),
                crate::decorations::titlebar_height(window),
            ),
        })
    }

    /// Map exactly the windows of visible workspaces into the space, where
    /// `rest_rect` says, so the end of a slide is exactly where windows
    /// settle. Windows that must change size (maximized onto a different
    /// monitor) are configured and shown squeezed until they redraw, then
    /// crossfade into their new contents.
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
                let send_tiled = self.tile_states(tiled);
                let Some(toplevel) = window.toplevel() else {
                    continue;
                };
                toplevel.with_pending_state(|s| {
                    if placement == Placement::Floating {
                        with_state(&window, |state| state.tiled = tiled);
                        crate::layout::set_tiled(&mut s.states, send_tiled);
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
                    // Once it redraws, crossfade from the squeezed old
                    // contents (where the slide left them) instead of
                    // swapping them in one frame.
                    let snapshot = self.refit_snapshot(&window, output);
                    with_state(&window, |s| {
                        s.pending_size = Some(
                            PendingSize::new(rest.size, old_size, serial).with_snapshot(snapshot),
                        );
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

    /// `window`'s contents as they are, to fade out once it redraws at the
    /// size of `output`; none with animations off.
    fn refit_snapshot(&mut self, window: &Window, output: &Output) -> Option<Snapshot> {
        if presets::WINDOW_RESIZE.scaled_duration().is_zero() {
            return None;
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        Snapshot::capture(self.backend.renderer()?, window, scale)
    }

    /// What `output` should show while the strip is moving. Every window
    /// glides in global coordinates between where it would rest on the
    /// neighbouring slots, so it crosses monitor borders at one speed and
    /// morphs to fit monitors of different sizes; whatever overlaps
    /// `output` is drawn there. Bottom to top.
    pub fn moving_layout(&self, output: &Output, now: Instant) -> Option<Vec<Placed>> {
        let motion = self.workspaces.motion.as_ref()?;
        // Another monitor's strip is moving: this one is at rest.
        if motion
            .lane
            .as_ref()
            .is_some_and(|name| *name != output.name())
        {
            return None;
        }
        let StripView {
            outputs,
            main,
            order,
            position,
        } = self.strip_view(output, now);
        let output_geo = self.space.output_geometry(output)?.to_f64();

        let mut layout = Vec::new();
        let lowest = position.floor() as isize - main as isize - 1;
        let highest = position.ceil() as isize + (outputs.len() - main) as isize + 1;
        for pos in lowest.max(0)..=highest.min(order.len() as isize - 1) {
            let index = order[pos as usize];
            // Fractional slot this workspace is passing through.
            let slot = pos as f64 - position + main as f64;
            let (from, t) = (slot.floor() as isize, slot - slot.floor());
            for window in &self.workspaces.list[index].windows {
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
                        workspace: index,
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
                workspace: order.get(motion.to).copied().unwrap_or(0),
            });
        }
        Some(layout)
    }
}

/// Where a floating `window` rests on a monitor at `geo`: its local
/// position, kept reachable on a smaller monitor and with its title bar
/// clear of the panels around `area` (dropped under the dock, it was
/// stuck there).
pub(crate) fn floating_rest(
    window: &Window,
    geo: Rectangle<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let size = window.geometry().size;
    let local = with_state(window, |s| s.local);
    let bar = crate::decorations::titlebar_height(window);
    Rectangle::new(geo.loc + reachable(local, size, bar, geo, area), size)
}

/// How much of a window's top edge must stay grabbable.
const GRAB: i32 = 32;

/// `local` (a window's position on a monitor at `geo`) moved so at least
/// 64px of its width and its title bar (`bar` tall, or the top GRAB px
/// without one) lie within `area`.
fn reachable(
    local: Point<i32, Logical>,
    size: Size<i32, Logical>,
    bar: i32,
    geo: Rectangle<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    let top = area.loc.y - geo.loc.y + bar;
    let bottom = area.loc.y - geo.loc.y + area.size.h - bar.max(GRAB) + bar;
    let left = area.loc.x - geo.loc.x;
    let right = left + area.size.w;
    Point::from((
        local
            .x
            .clamp(left - (size.w - 64).max(0), (right - 64).max(left)),
        local.y.min(bottom).max(top),
    ))
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
        ws.next_id = occupied.len() as u64;
        ws
    }

    #[test]
    fn dropped_windows_keep_their_title_bar_reachable() {
        let geo = Rectangle::new((1920, 0).into(), (1920, 1080).into());
        // A 32px top bar and a 72px dock.
        let area = Rectangle::new((1920, 32).into(), (1920, 976).into());
        let size = Size::from((800, 600));
        let at = |x, y| reachable(Point::from((x, y)), size, 36, geo, area);
        // Dropped under the dock: the title bar comes back above it.
        // Title bar top at 1008 - 36, just above the dock.
        assert_eq!(at(400, 1060), Point::from((400, 1008)));
        // Under the top bar: just below it.
        assert_eq!(at(400, 10), Point::from((400, 32 + 36)));
        // Mostly off a side: 64px stay on screen.
        assert_eq!(at(-2000, 300).x, -(800 - 64));
        assert_eq!(at(5000, 300).x, 1920 - 64);
        // Already reachable: left alone.
        assert_eq!(at(400, 300), Point::from((400, 300)));
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

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Window numbers of each workspace, in `lane` order.
    fn lane_windows(ws: &Workspaces<u32>, lane: &str) -> Vec<Vec<u32>> {
        (0..ws.len_of(Some(lane)))
            .map(|p| ws.list[ws.index_at(Some(lane), p).unwrap()].windows.clone())
            .collect()
    }

    fn all_windows(ws: &Workspaces<u32>) -> Vec<u32> {
        let mut all: Vec<u32> = ws.list.iter().flat_map(|w| w.windows.clone()).collect();
        all.sort();
        all
    }

    #[test]
    fn going_individual_keeps_each_monitors_workspace() {
        // 0 1 [2 3] 4 5 with two monitors, main left, showing 2 and 3.
        let mut ws = strip(&[true; 6], 2);
        ws.become_individual(&names(&["L", "R"]), 0);
        assert!(ws.individual);
        // Main takes the hidden ones around its own; the other has just its.
        assert_eq!(
            lane_windows(&ws, "L"),
            vec![vec![0], vec![1], vec![2], vec![4], vec![5]]
        );
        assert_eq!(ws.lanes["L"].active, 2);
        assert_eq!(lane_windows(&ws, "R"), vec![vec![3]]);
        assert_eq!(all_windows(&ws), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn lanes_normalize_independently() {
        let mut ws = strip(&[true; 4], 1);
        ws.become_individual(&names(&["L", "R"]), 0);
        ws.normalize_lane("L");
        ws.normalize_lane("R");
        // R: its window with a spare either side.
        assert_eq!(lane_windows(&ws, "R"), vec![vec![], vec![2], vec![]]);
        assert_eq!(ws.lanes["R"].active, 1);
        assert_eq!(all_windows(&ws), vec![0, 1, 2, 3]);
    }

    #[test]
    fn normalizing_a_settled_lane_keeps_list_order() {
        let mut ws = strip(&[true; 4], 1);
        ws.become_individual(&names(&["L", "R"]), 0);
        ws.normalize_lane("R");
        ws.normalize_lane("L");
        let ids: Vec<_> = ws.list.iter().map(|w| w.id).collect();
        ws.normalize_lane("R");
        ws.normalize_lane("L");
        assert_eq!(ws.list.iter().map(|w| w.id).collect::<Vec<_>>(), ids);
    }

    #[test]
    fn lane_motion_counts_lane_positions() {
        let mut ws = strip(&[true; 4], 1);
        ws.become_individual(&names(&["L", "R"]), 0);
        ws.normalize_lane("R");
        assert_eq!(ws.len_of(Some("R")), 3);
        assert_eq!(ws.target_in(Some("R")), 1);
        let index = ws.index_at(Some("R"), 2).unwrap();
        assert_eq!(ws.lane_of(index), Some(("R", 2)));
    }

    #[test]
    fn unplugged_monitor_hands_over_its_windows() {
        let mut ws = strip(&[true; 4], 1);
        ws.become_individual(&names(&["L", "R"]), 0);
        ws.drop_lane("R", "L");
        assert!(!ws.lanes.contains_key("R"));
        assert_eq!(all_windows(&ws), vec![0, 1, 2, 3]);
        let on_l: Vec<u32> = lane_windows(&ws, "L").concat();
        assert_eq!(on_l.len(), 4);
        // Its own workspace is last, after the main monitor's.
        assert_eq!(on_l.last(), Some(&2));
    }

    #[test]
    fn going_continuous_puts_current_workspaces_side_by_side() {
        let mut ws = strip(&[true; 4], 1);
        ws.become_individual(&names(&["L", "R"]), 0);
        // L looks at 0 now; R at 2.
        ws.lanes.get_mut("L").unwrap().active = 0;
        ws.become_continuous(&names(&["L", "R"]), 0);
        assert!(!ws.individual && ws.lanes.is_empty());
        let order: Vec<u32> = ws.list.iter().flat_map(|w| w.windows.clone()).collect();
        assert_eq!(order[ws.active], 0);
        assert_eq!(order[ws.active + 1], 2);
        assert_eq!(all_windows(&ws), vec![0, 1, 2, 3]);
    }

    #[test]
    fn mode_round_trip_loses_nothing() {
        let mut ws = strip(&[true, false, true, true, false, true], 2);
        ws.become_individual(&names(&["A", "B", "C"]), 1);
        for n in ["A", "B", "C"] {
            ws.normalize_lane(n);
        }
        ws.become_continuous(&names(&["A", "B", "C"]), 1);
        ws.normalize(1, 1);
        assert_eq!(all_windows(&ws), vec![0, 2, 3, 5]);
    }
}
