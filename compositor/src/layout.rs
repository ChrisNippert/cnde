//! Window layouts (ARCHITECTURE.md §4.4): floating and master-stack tiling
//! behind one `Layout` trait, chosen per workspace (Super+T).
//!
//! A layout only says where windows rest; `rest_rect` (workspaces.rs) asks
//! it, so the workspace strip, the overview and `rebuild_space` all agree
//! on tiles without knowing about them. When the tiles change (a window
//! opens, closes, swaps, the ratio changes) `relayout_workspace` moves the
//! live windows there with the same resize animation a maximize uses.
//!
//! Floating overrides on a tiling workspace: dialogs (windows with a
//! parent), fixed-size windows, minimized/maximized/snapped/fullscreen ones
//! and windows the user floated (`WindowState::keep_floating`) stay out of
//! the tiles and float above them.

use smithay::{
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        compositor,
        shell::xdg::{SurfaceCachedState, ToplevelStateSet},
    },
};
use tracing::debug;

use cnde_config::{TilingStyle, WindowLayout};

use crate::keybinds::Direction;

use crate::{
    decorations::{content_rect, titlebar_height},
    effects::{PendingSize, ResizeAnim},
    state::State,
    window::{Placement, with_state},
    workspaces::{Workspace, floating_rest},
};

/// Super+[ / ] step, and the range the ratio stays in.
pub const RATIO_STEP: f64 = 0.05;
pub const MIN_RATIO: f64 = 0.1;
pub const MAX_RATIO: f64 = 0.9;
/// Size of a window floated off the tiles that never had a floating size,
/// as a fraction of the usable area.
const FLOAT_FRACTION: f64 = 0.6;

/// Where a workspace's windows rest. Rects are whole window slots,
/// including any title bar cnde draws.
pub trait Layout {
    /// Where each window rests within `area` (a monitor's panel-free area),
    /// given where each would rest on its own (`own`), in tiling order.
    fn arrange(
        &self,
        area: Rectangle<i32, Logical>,
        own: &[Rectangle<i32, Logical>],
    ) -> Vec<Rectangle<i32, Logical>>;

    /// Whether it sizes windows itself: they're told they're tiled, and
    /// floating overrides (dialogs, …) stay out of it.
    fn tiles(&self) -> bool;
}

/// GNOME-style: windows stay wherever they were put.
pub struct Floating;

impl Layout for Floating {
    fn arrange(
        &self,
        _area: Rectangle<i32, Logical>,
        own: &[Rectangle<i32, Logical>],
    ) -> Vec<Rectangle<i32, Logical>> {
        own.to_vec()
    }

    fn tiles(&self) -> bool {
        false
    }
}

/// Hyprland's "master" layout: the first window on the left at `ratio` of
/// the width, the rest stacked top to bottom on the right.
pub struct MasterStack {
    pub ratio: f64,
    /// Between windows.
    pub gap: i32,
    /// Between windows and the area's edges.
    pub outer_gap: i32,
}

impl Layout for MasterStack {
    fn arrange(
        &self,
        area: Rectangle<i32, Logical>,
        own: &[Rectangle<i32, Logical>],
    ) -> Vec<Rectangle<i32, Logical>> {
        master_stack(area, own.len(), self.ratio, self.gap, self.outer_gap)
    }

    fn tiles(&self) -> bool {
        true
    }
}

/// Hyprland's "dwindle" layout: each window after the first splits the
/// previous one's space along its longer side, spiralling inwards. The
/// first split puts `ratio` of the space on the first window.
pub struct Dwindle {
    pub ratio: f64,
    /// Between windows.
    pub gap: i32,
    /// Between windows and the area's edges.
    pub outer_gap: i32,
}

impl Layout for Dwindle {
    fn arrange(
        &self,
        area: Rectangle<i32, Logical>,
        own: &[Rectangle<i32, Logical>],
    ) -> Vec<Rectangle<i32, Logical>> {
        dwindle(area, own.len(), self.ratio, self.gap, self.outer_gap)
    }

    fn tiles(&self) -> bool {
        true
    }
}

/// The dwindle rects for `count` windows in `area`, each new one having
/// split the one before it.
pub fn dwindle(
    area: Rectangle<i32, Logical>,
    count: usize,
    ratio: f64,
    gap: i32,
    outer_gap: i32,
) -> Vec<Rectangle<i32, Logical>> {
    let mut tree = SplitTree::default();
    for i in 0..count {
        tree.insert(i, i.checked_sub(1).as_ref());
    }
    tree.arrange(area, ratio, gap, outer_gap)
        .into_iter()
        .map(|(_, rect)| rect)
        .collect()
}

/// Dwindle's binary tree of splits: each leaf is a window, each split
/// halves its space along the longer side (decided when arranging, so it
/// follows the monitor it's shown on). The root split takes `ratio`.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitTree<W> {
    root: Option<Node<W>>,
}

impl<W> Default for SplitTree<W> {
    fn default() -> Self {
        Self { root: None }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Node<W> {
    Leaf(W),
    Split(Box<Node<W>>, Box<Node<W>>),
}

impl<W: PartialEq + Clone> Node<W> {
    fn contains(&self, w: &W) -> bool {
        match self {
            Node::Leaf(leaf) => leaf == w,
            Node::Split(a, b) => a.contains(w) || b.contains(w),
        }
    }

    fn leaves(&self, out: &mut Vec<W>) {
        match self {
            Node::Leaf(leaf) => out.push(leaf.clone()),
            Node::Split(a, b) => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    /// Split the leaf holding `target` (false if there's none).
    fn split(&mut self, target: &W, new: &W) -> bool {
        match self {
            Node::Leaf(leaf) if leaf == target => {
                let old = Node::Leaf(leaf.clone());
                *self = Node::Split(Box::new(old), Box::new(Node::Leaf(new.clone())));
                true
            }
            Node::Leaf(_) => false,
            Node::Split(a, b) => a.split(target, new) || b.split(target, new),
        }
    }

    /// Take out the leaf holding `w`; its sibling takes the split's place.
    /// `None` when `w` was this whole node.
    fn remove(self, w: &W) -> Option<Node<W>> {
        match self {
            Node::Leaf(leaf) => (leaf != *w).then_some(Node::Leaf(leaf)),
            Node::Split(a, b) => match (a.remove(w), b.remove(w)) {
                (Some(a), Some(b)) => Some(Node::Split(Box::new(a), Box::new(b))),
                (one, other) => one.or(other),
            },
        }
    }

    /// `a` and `b` trade leaves.
    fn trade(&mut self, a: &W, b: &W) {
        match self {
            Node::Leaf(leaf) if leaf == a => *leaf = b.clone(),
            Node::Leaf(leaf) if leaf == b => *leaf = a.clone(),
            Node::Leaf(_) => {}
            Node::Split(x, y) => {
                x.trade(a, b);
                y.trade(a, b);
            }
        }
    }

    fn place(
        &self,
        rect: Rectangle<i32, Logical>,
        ratio: f64,
        gap: i32,
        out: &mut Vec<(W, Rectangle<i32, Logical>)>,
    ) {
        let (a, b) = match self {
            Node::Leaf(leaf) => return out.push((leaf.clone(), rect)),
            Node::Split(a, b) => (a, b),
        };
        // Side by side when wider than tall, else one above the other.
        let wide = rect.size.w >= rect.size.h;
        let total = if wide { rect.size.w } else { rect.size.h };
        let first = (((total - gap) as f64 * ratio).round() as i32).max(1);
        let second = (total - first - gap).max(1);
        let (ra, rb) = if wide {
            (
                Rectangle::new(rect.loc, Size::from((first, rect.size.h))),
                Rectangle::new(
                    rect.loc + Point::from((first + gap, 0)),
                    Size::from((second, rect.size.h)),
                ),
            )
        } else {
            (
                Rectangle::new(rect.loc, Size::from((rect.size.w, first))),
                Rectangle::new(
                    rect.loc + Point::from((0, first + gap)),
                    Size::from((rect.size.w, second)),
                ),
            )
        };
        a.place(ra, 0.5, gap, out);
        b.place(rb, 0.5, gap, out);
    }
}

impl<W: PartialEq + Clone> SplitTree<W> {
    pub fn contains(&self, w: &W) -> bool {
        self.root.as_ref().is_some_and(|r| r.contains(w))
    }

    /// Its windows, in reading order of the splits.
    pub fn leaves(&self) -> Vec<W> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.leaves(&mut out);
        }
        out
    }

    /// Add `new` by splitting `target`'s space (the last window's if
    /// `target` isn't in the tree).
    pub fn insert(&mut self, new: W, target: Option<&W>) {
        let Some(root) = &mut self.root else {
            self.root = Some(Node::Leaf(new));
            return;
        };
        let target = target
            .filter(|t| root.contains(t))
            .cloned()
            .or_else(|| self.leaves().pop());
        if let (Some(root), Some(target)) = (&mut self.root, target) {
            root.split(&target, &new);
        }
    }

    pub fn remove(&mut self, w: &W) {
        self.root = self.root.take().and_then(|r| r.remove(w));
    }

    /// Swap two windows' places. If only `b` is in the tree, `a` splits
    /// its space instead (a window dropped onto a tile from elsewhere).
    pub fn swap(&mut self, a: &W, b: &W) {
        let Some(root) = &mut self.root else {
            return;
        };
        match (root.contains(a), root.contains(b)) {
            (true, true) => root.trade(a, b),
            (false, true) => {
                root.split(b, a);
            }
            _ => {}
        }
    }

    /// Make the tree hold exactly `windows`: drop the ones gone, and add
    /// new ones by splitting `target` (see [`insert`](Self::insert)).
    pub fn sync(&mut self, windows: &[W], target: Option<&W>) {
        for gone in self.leaves().into_iter().filter(|w| !windows.contains(w)) {
            self.remove(&gone);
        }
        for w in windows {
            if !self.contains(w) {
                self.insert(w.clone(), target);
            }
        }
    }

    /// Each window's rect in `area`, the root split taking `ratio`.
    pub fn arrange(
        &self,
        area: Rectangle<i32, Logical>,
        ratio: f64,
        gap: i32,
        outer_gap: i32,
    ) -> Vec<(W, Rectangle<i32, Logical>)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.place(
                inset(area, outer_gap),
                ratio.clamp(MIN_RATIO, MAX_RATIO),
                gap,
                &mut out,
            );
        }
        out
    }
}

/// `area` shrunk by `by` on every side.
fn inset(area: Rectangle<i32, Logical>, by: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        area.loc + Point::from((by, by)),
        Size::from(((area.size.w - 2 * by).max(1), (area.size.h - 2 * by).max(1))),
    )
}

/// Split `total` into `count` lengths with `gap` between them; the first
/// ones take the remainder, so they always add up exactly.
fn split(total: i32, count: usize, gap: i32) -> Vec<i32> {
    let n = count as i32;
    let free = (total - gap * (n - 1)).max(n);
    (0..n).map(|i| free / n + i32::from(i < free % n)).collect()
}

/// The master-stack rects for `count` windows in `area`.
pub fn master_stack(
    area: Rectangle<i32, Logical>,
    count: usize,
    ratio: f64,
    gap: i32,
    outer_gap: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    let inner = inset(area, outer_gap);
    if count == 1 {
        return vec![inner];
    }
    let ratio = ratio.clamp(MIN_RATIO, MAX_RATIO);
    let master_w = (((inner.size.w - gap) as f64 * ratio).round() as i32).max(1);
    let stack_x = inner.loc.x + master_w + gap;
    let stack_w = (inner.size.w - master_w - gap).max(1);
    let mut rects = vec![Rectangle::new(
        inner.loc,
        Size::from((master_w, inner.size.h)),
    )];
    let mut y = inner.loc.y;
    for h in split(inner.size.h, count - 1, gap) {
        rects.push(Rectangle::new(
            Point::from((stack_x, y)),
            Size::from((stack_w, h)),
        ));
        y += h + gap;
    }
    rects
}

/// Set or clear all four tiled states: CSD clients drop their shadows and
/// rounded corners, and stop asking to be resized from those edges.
pub fn set_tiled(states: &mut ToplevelStateSet, tiled: bool) {
    for state in [
        XdgState::TiledLeft,
        XdgState::TiledRight,
        XdgState::TiledTop,
        XdgState::TiledBottom,
    ] {
        if tiled {
            states.set(state);
        } else {
            states.unset(state);
        }
    }
}

/// Generic so the ordering can be unit-tested with plain numbers.
impl<W: PartialEq + Clone> Workspace<W> {
    /// Its windows in tiling order, main window first: the recorded order,
    /// then any not recorded yet in the order they joined.
    pub fn tile_order(&self) -> Vec<W> {
        let mut order: Vec<W> = self
            .tiles
            .iter()
            .filter(|w| self.windows.contains(w))
            .cloned()
            .collect();
        order.extend(
            self.windows
                .iter()
                .filter(|w| !self.tiles.contains(w))
                .cloned(),
        );
        order
    }

    /// Record the current tiling order, so restacking can't change it.
    pub fn sync_tiles(&mut self) {
        self.tiles = self.tile_order();
    }

    /// Swap two windows' places in the tiling order.
    pub fn swap_tiles(&mut self, a: &W, b: &W) {
        let mut order = self.tile_order();
        let i = order.iter().position(|w| w == a);
        let j = order.iter().position(|w| w == b);
        if let (Some(i), Some(j)) = (i, j) {
            order.swap(i, j);
        }
        self.tiles = order;
        self.splits.swap(a, b);
    }
}

impl State {
    /// The layout workspace `index` uses now.
    fn layout_mode(&self, index: usize) -> WindowLayout {
        self.workspaces
            .list
            .get(index)
            .and_then(|ws| ws.layout)
            .unwrap_or(self.config.tiling.layout)
    }

    fn master_ratio(&self, index: usize) -> f64 {
        self.workspaces
            .list
            .get(index)
            .and_then(|ws| ws.master_ratio)
            .unwrap_or(self.config.tiling.master_ratio)
    }

    pub fn layout_of(&self, index: usize) -> Box<dyn Layout> {
        match self.layout_mode(index) {
            WindowLayout::Floating => Box::new(Floating),
            WindowLayout::Tiling => {
                let (ratio, gap, outer_gap) = (
                    self.master_ratio(index),
                    self.config.tiling.gap as i32,
                    self.config.tiling.outer_gap as i32,
                );
                match self.config.tiling.style {
                    TilingStyle::Dwindle => Box::new(Dwindle {
                        ratio,
                        gap,
                        outer_gap,
                    }),
                    TilingStyle::Master => Box::new(MasterStack {
                        ratio,
                        gap,
                        outer_gap,
                    }),
                }
            }
        }
    }

    /// Whether `window` takes part in its workspace's layout, if that tiles.
    /// Dialogs and fixed-size windows float above the tiles.
    pub fn tileable(window: &Window) -> bool {
        let (placement, minimized) = with_state(window, |s| (s.placement, s.minimized));
        placement == Placement::Floating && !minimized && Self::could_tile(window)
    }

    /// Whether `window` would tile if it floated: not kept floating, not a
    /// dialog, not fixed-size.
    fn could_tile(window: &Window) -> bool {
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        if with_state(window, |s| s.keep_floating) || toplevel.parent().is_some() {
            return false;
        }
        let (min, max) = compositor::with_states(toplevel.wl_surface(), |states| {
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let data = guard.current();
            (data.min_size, data.max_size)
        });
        let fixed = min.w > 0 && min.h > 0 && min == max;
        !fixed
    }

    /// Whether `window`, not yet open, will join tiles: the workspace under
    /// the pointer (where it opens) tiles and it isn't a dialog.
    pub fn opens_tiled(&self, window: &Window) -> bool {
        window.toplevel().is_some_and(|t| t.parent().is_none())
            && self
                .output_under_pointer()
                .and_then(|o| self.workspace_on(&o))
                .is_some_and(|index| self.layout_of(index).tiles())
    }

    /// The windows workspace `index` tiles, main window first; empty when
    /// it floats.
    fn tiled_windows(&self, index: usize) -> Vec<Window> {
        if !self.layout_of(index).tiles() {
            return Vec::new();
        }
        self.workspaces.list[index]
            .tile_order()
            .into_iter()
            .filter(Self::tileable)
            .collect()
    }

    pub fn is_tiled(&self, window: &Window) -> bool {
        self.workspaces
            .index_of(window)
            .is_some_and(|index| self.layout_of(index).tiles())
            && Self::tileable(window)
    }

    /// Where `window`'s layout rests it on a monitor at `geo` with `area`
    /// free of panels; `None` if it isn't part of its workspace's layout.
    /// Its geometry, without the title bar cnde draws.
    pub(crate) fn layout_rest(
        &self,
        window: &Window,
        geo: Rectangle<i32, Logical>,
        area: Rectangle<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        let index = self.workspaces.index_of(window)?;
        let layout = self.layout_of(index);
        if !layout.tiles() {
            // Floating: every window where it is.
            let own = with_bar(floating_rest(window, geo, area), window);
            let rect = layout.arrange(area, &[own]).first().copied()?;
            return Some(content_rect(rect, titlebar_height(window)));
        }
        let rect = self
            .tile_rects(index, area)
            .into_iter()
            .find(|(w, _)| w == window)?
            .1;
        Some(content_rect(rect, titlebar_height(window)))
    }

    /// Where workspace `index`'s tiles rest within `area`.
    fn tile_rects(
        &self,
        index: usize,
        area: Rectangle<i32, Logical>,
    ) -> Vec<(Window, Rectangle<i32, Logical>)> {
        let windows = self.tiled_windows(index);
        let (ratio, gap, outer_gap) = self.tile_params(index);
        match self.config.tiling.style {
            TilingStyle::Dwindle => {
                // Normally already in step (relayout_workspace); a copy
                // keeps this read-only either way.
                let mut splits = self.workspaces.list[index].splits.clone();
                splits.sync(&windows, None);
                splits.arrange(area, ratio, gap, outer_gap)
            }
            TilingStyle::Master => {
                let rects = master_stack(area, windows.len(), ratio, gap, outer_gap);
                windows.into_iter().zip(rects).collect()
            }
        }
    }

    fn tile_params(&self, index: usize) -> (f64, i32, i32) {
        (
            self.master_ratio(index),
            self.config.tiling.gap as i32,
            self.config.tiling.outer_gap as i32,
        )
    }

    /// Bring workspace `index`'s splits in step with its tiles; new ones
    /// split the focused window, like Hyprland.
    fn sync_splits(&mut self, index: usize) {
        let windows = self.tiled_windows(index);
        let focused = self.focused_window().filter(|w| windows.contains(w));
        self.workspaces.list[index]
            .splits
            .sync(&windows, focused.as_ref());
    }

    /// The tile `window` rests in on the monitor showing it now.
    pub fn tile_rect_now(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let (outputs, _) = self.strip_outputs();
        let output = self.output_for(window)?;
        let slot = outputs.iter().position(|o| *o == output)?;
        let (geo, area) = self.slot_areas(&outputs, slot as isize)?;
        self.layout_rest(window, geo, area)
    }

    /// Workspace `index` is now visible at `slot` (an index into the
    /// strip's outputs), if it is.
    fn slot_of(&self, index: usize) -> Option<usize> {
        let (outputs, _) = self.strip_outputs();
        outputs
            .iter()
            .position(|o| self.workspace_on(o) == Some(index))
    }

    /// Move every window of workspace `index` to where its layout puts it,
    /// gliding there with `animate`. Hidden workspaces and a moving strip
    /// are left alone: `rest_rect` draws them, and `rebuild_space` sizes
    /// them when they land.
    pub fn relayout_workspace(&mut self, index: usize, animate: bool) {
        if self.workspaces.is_moving() || index >= self.workspaces.list.len() {
            return;
        }
        self.workspaces.list[index].sync_tiles();
        self.sync_splits(index);
        let Some(slot) = self.slot_of(index) else {
            return;
        };
        let (outputs, _) = self.strip_outputs();
        let tiles = self.layout_of(index).tiles();
        let windows = self.workspaces.list[index].windows.clone();
        for window in windows {
            let (placement, minimized) = with_state(&window, |s| (s.placement, s.minimized));
            if placement != Placement::Floating || minimized {
                continue;
            }
            if !(tiles && Self::tileable(&window)) {
                // Floated, or the workspace stopped tiling.
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|s| set_tiled(&mut s.states, false));
                    if toplevel.is_initial_configure_sent() {
                        toplevel.send_pending_configure();
                    }
                }
                continue;
            }
            let Some(rest) = self.rest_rect(&window, &outputs, slot as isize) else {
                continue;
            };
            // Coming back from minimized: it opens where its tile is.
            if self.space.element_location(&window).is_none() {
                self.space.map_element(window.clone(), rest.loc, false);
            }
            self.glide_to(&window, rest, true, animate);
        }
        self.queue_redraw();
        self.queue_pointer_refresh();
    }

    /// [`relayout_workspace`](Self::relayout_workspace) for every workspace
    /// on screen.
    pub fn relayout_visible(&mut self, animate: bool) {
        let (outputs, _) = self.strip_outputs();
        let mut visible: Vec<usize> = outputs
            .iter()
            .filter_map(|o| self.workspace_on(o))
            .collect();
        visible.dedup();
        for index in visible {
            self.relayout_workspace(index, animate);
        }
    }

    /// Tell `window` its new size and tiled state and move it to `rect`
    /// (its geometry, space coordinates). With `animate` it glides from
    /// where it's drawn now, crossfading from its old contents if its size
    /// changes, like a maximize.
    fn glide_to(
        &mut self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        tiled: bool,
        animate: bool,
    ) {
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        let old_size = window.geometry().size;
        let mapped = self.space.element_location(window);
        let resizing = rect.size != old_size;
        // Opening windows grow in place instead.
        let opening = with_state(window, |s| s.open_anim.is_some());
        let start = if animate && !opening && mapped.is_some_and(|loc| loc != rect.loc || resizing)
        {
            self.visual_rect(window)
        } else {
            None
        };
        let snapshot = if start.is_some() && resizing {
            self.snapshot(window)
        } else {
            None
        };
        toplevel.with_pending_state(|s| {
            set_tiled(&mut s.states, tiled);
            s.size = Some(rect.size);
        });
        let serial = if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure()
        } else {
            None
        };
        if mapped.is_none() {
            return;
        }
        self.move_window(window, rect.loc);
        if let Some(from) = start {
            with_state(window, |s| {
                s.resize_anim = Some(ResizeAnim::new(from, snapshot, serial));
            });
        } else if let Some(serial) = serial
            && resizing
        {
            with_state(window, |s| {
                s.pending_size = Some(PendingSize::new(rect.size, old_size, serial));
            });
        }
    }

    /// The workspace the layout keys act on: the focused window's, else the
    /// one under the pointer.
    fn current_workspace(&self) -> Option<usize> {
        self.focused_window()
            .and_then(|w| self.workspaces.index_of(&w))
            .or_else(|| {
                self.output_under_pointer()
                    .and_then(|o| self.workspace_on(&o))
            })
    }

    /// Super+T: switch the current workspace between floating and tiling.
    pub fn toggle_tiling(&mut self) {
        let Some(index) = self.current_workspace() else {
            return;
        };
        let was = self.layout_mode(index);
        let next = match was {
            WindowLayout::Floating => WindowLayout::Tiling,
            WindowLayout::Tiling => WindowLayout::Floating,
        };
        debug!(index, ?next, "workspace layout");
        // In the overview, the thumbnails slide to their new places too.
        self.overview_before_relayout();
        self.workspaces.list[index].layout = Some(next);
        self.layout_switched(index, was);
    }

    /// Workspace `index` was using `was` and may now use another layout:
    /// remember where windows floated before they tile, and put them back
    /// there when tiling stops.
    fn layout_switched(&mut self, index: usize, was: WindowLayout) {
        let now = self.layout_mode(index);
        if now == was {
            self.relayout_workspace(index, true);
            return;
        }
        let windows = self.workspaces.list[index].windows.clone();
        let (outputs, _) = self.strip_outputs();
        let output = self.slot_of(index).and_then(|slot| outputs.get(slot));
        let origin = output
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| g.loc);
        // Where windows that opened as tiles float: off screen, the size
        // the main monitor would give them.
        let usable = output.or(outputs.first()).and_then(|o| {
            let geo = self.space.output_geometry(o)?;
            let usable = self.usable_area(o)?;
            Some(Rectangle::new(usable.loc - geo.loc, usable.size))
        });
        let mut cascade = 0;
        for window in windows {
            // Maximized or snapped, it joins the tiles too, and floats where
            // it did before that when tiling stops.
            let restored = match now {
                WindowLayout::Tiling => self.unplace_to_tile(&window),
                WindowLayout::Floating => None,
            };
            if !Self::tileable(&window) {
                continue;
            }
            let rect = restored
                .or_else(|| self.space.element_geometry(&window))
                .zip(origin)
                .map(|(geo, origin)| Rectangle::new(geo.loc - origin, geo.size));
            match now {
                WindowLayout::Tiling => {
                    let rect = rect.unwrap_or_else(|| {
                        with_state(&window, |s| Rectangle::new(s.local, window.geometry().size))
                    });
                    with_state(&window, |s| s.untiled = Some(rect));
                }
                WindowLayout::Floating => {
                    let (untiled, kept) =
                        with_state(&window, |s| (s.untiled.take(), s.keep_floating));
                    // Opened as a tile, it never floated: rather than float
                    // in its tile (and look like nothing happened), it gets a
                    // fresh spot, centred and cascading like new windows.
                    let back = match untiled {
                        Some(back) => Some(back),
                        None if kept => rect,
                        None => usable.map(|area| {
                            cascade += 1;
                            floating_spot(area, cascade - 1)
                        }),
                    };
                    let Some(back) = back else {
                        continue;
                    };
                    with_state(&window, |s| s.local = back.loc);
                    match origin {
                        Some(origin) => self.glide_to(
                            &window,
                            Rectangle::new(origin + back.loc, back.size),
                            false,
                            true,
                        ),
                        // Not on screen: just the size; it's placed when shown.
                        None => {
                            if let Some(toplevel) = window.toplevel() {
                                toplevel.with_pending_state(|s| {
                                    set_tiled(&mut s.states, false);
                                    s.size = Some(back.size);
                                });
                                if toplevel.is_initial_configure_sent() {
                                    toplevel.send_pending_configure();
                                }
                            }
                        }
                    }
                }
            }
        }
        self.relayout_workspace(index, true);
    }

    /// Take a maximized or snapped `window` (that could tile) back to
    /// floating, unsent: the relayout that follows sizes it into its tile.
    /// Returns where it floated before, in space coordinates.
    fn unplace_to_tile(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let (placement, minimized) = with_state(window, |s| (s.placement, s.minimized));
        let snapped_or_maximized = matches!(
            placement,
            Placement::Maximized | Placement::SnappedLeft | Placement::SnappedRight
        );
        if !snapped_or_maximized || minimized || !Self::could_tile(window) {
            return None;
        }
        window.toplevel()?.with_pending_state(|s| {
            s.states.unset(XdgState::Maximized);
        });
        with_state(window, |s| {
            s.placement = Placement::Floating;
            s.restore
        })
    }

    /// `tiling` in the config changed (from `old`): workspaces that follow
    /// its layout switch, and tiles take the new ratio and gaps.
    pub fn tiling_config_changed(&mut self, old: &cnde_config::Tiling) {
        for index in 0..self.workspaces.list.len() {
            if self.workspaces.list[index].layout.is_none() {
                self.layout_switched(index, old.layout);
            }
        }
        self.relayout_visible(true);
    }

    /// Super+Enter: swap the focused window with the main one (the main
    /// one swaps with the first in the stack).
    pub fn swap_with_master(&mut self) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let Some(index) = self.workspaces.index_of(&window) else {
            return;
        };
        let tiles = self.tiled_windows(index);
        let other = match tiles.iter().position(|w| *w == window) {
            Some(0) => tiles.get(1),
            Some(_) => tiles.first(),
            None => None,
        };
        let Some(other) = other.cloned() else {
            return;
        };
        self.overview_before_relayout();
        self.workspaces.list[index].swap_tiles(&window, &other);
        self.relayout_workspace(index, true);
    }

    /// Whether the workspace the keys act on tiles.
    pub fn tiling_here(&self) -> bool {
        self.current_workspace()
            .is_some_and(|index| self.layout_of(index).tiles())
    }

    /// Super+arrow on a tiling workspace: focus the window next to the
    /// focused one that way, on any monitor.
    pub fn focus_direction(&mut self, direction: Direction) {
        let Some(window) = self.focused_window() else {
            return;
        };
        if let Some(next) = self.neighbour(&window, direction, false) {
            self.focus_window(Some(&next));
        }
    }

    /// Super+Shift+arrow on a tiling workspace: swap the focused tile with
    /// the tile next to it that way. With none there, left and right move
    /// it onto the next monitor's workspace (if it tiles), at the end.
    pub fn move_tile(&mut self, direction: Direction) {
        let Some(window) = self.focused_window() else {
            return;
        };
        if !self.is_tiled(&window) {
            return;
        }
        let Some(from) = self.workspaces.index_of(&window) else {
            return;
        };
        self.overview_before_relayout();
        if let Some(other) = self.neighbour(&window, direction, true) {
            let Some(dest) = self.workspaces.index_of(&other) else {
                return;
            };
            if dest != from {
                self.workspaces.remove_window(&window);
                self.workspaces.list[dest].windows.push(window.clone());
            }
            self.workspaces.list[dest].swap_tiles(&window, &other);
        } else {
            let step = match direction {
                Direction::Left => -1,
                Direction::Right => 1,
                Direction::Up | Direction::Down => return,
            };
            let Some(slot) = self.slot_of(from) else {
                return;
            };
            let (outputs, _) = self.strip_outputs();
            let Some(output) = outputs.get(slot.wrapping_add_signed(step)) else {
                return;
            };
            let Some(dest) = self.workspace_on(output) else {
                return;
            };
            if !self.layout_of(dest).tiles() {
                return;
            }
            self.workspaces.remove_window(&window);
            self.workspaces.list[dest].windows.push(window.clone());
            self.normalize_workspaces();
        }
        self.relayout_visible(true);
        self.focus_window(Some(&window));
    }

    /// The window on screen nearest `window` in `direction` (only tiles
    /// with `tiles_only`): among those beyond its edge that way, the one
    /// sharing the most of its side, then the closest.
    fn neighbour(&self, window: &Window, direction: Direction, tiles_only: bool) -> Option<Window> {
        let from = with_bar(self.space.element_geometry(window)?, window);
        // Tiles touch across a gap; a little slack for rounding.
        let slack = self.config.tiling.gap as i32 + 4;
        self.space
            .elements()
            .filter(|w| *w != window)
            .filter(|w| !tiles_only || self.is_tiled(w))
            .filter_map(|w| {
                let to = with_bar(self.space.element_geometry(w)?, w);
                let (distance, overlap) = match direction {
                    Direction::Left => (
                        from.loc.x - (to.loc.x + to.size.w),
                        span_overlap(from.loc.y, from.size.h, to.loc.y, to.size.h),
                    ),
                    Direction::Right => (
                        to.loc.x - (from.loc.x + from.size.w),
                        span_overlap(from.loc.y, from.size.h, to.loc.y, to.size.h),
                    ),
                    Direction::Up => (
                        from.loc.y - (to.loc.y + to.size.h),
                        span_overlap(from.loc.x, from.size.w, to.loc.x, to.size.w),
                    ),
                    Direction::Down => (
                        to.loc.y - (from.loc.y + from.size.h),
                        span_overlap(from.loc.x, from.size.w, to.loc.x, to.size.w),
                    ),
                };
                (distance >= -slack && overlap > 0).then(|| (w.clone(), overlap, distance))
            })
            .min_by_key(|(_, overlap, distance)| (distance.max(&0) / 64, -overlap))
            .map(|(w, _, _)| w)
    }

    /// Super+[ / ]: narrow or widen the current workspace's main window.
    pub fn adjust_master_ratio(&mut self, delta: f64) {
        let Some(index) = self.current_workspace() else {
            return;
        };
        if !self.layout_of(index).tiles() {
            return;
        }
        let ratio = (self.master_ratio(index) + delta).clamp(MIN_RATIO, MAX_RATIO);
        self.overview_before_relayout();
        self.workspaces.list[index].master_ratio = Some(ratio);
        self.relayout_workspace(index, true);
    }

    /// Super+Shift+T: float the focused tile above the others (back where
    /// it floated before tiling, or centred), or tile it again.
    pub fn toggle_focused_floating(&mut self) {
        let Some(window) = self.focused_window() else {
            return;
        };
        let Some(index) = self.workspaces.index_of(&window) else {
            return;
        };
        if !self.layout_of(index).tiles() || Self::placement_of(&window) != Placement::Floating {
            return;
        }
        if with_state(&window, |s| s.keep_floating) {
            with_state(&window, |s| s.keep_floating = false);
            self.relayout_workspace(index, true);
            return;
        }
        if !Self::tileable(&window) {
            return;
        }
        with_state(&window, |s| s.keep_floating = true);
        let output = self.output_for(&window);
        let geo = output.as_ref().and_then(|o| self.space.output_geometry(o));
        let area = output.as_ref().and_then(|o| self.usable_area(o));
        if let (Some(geo), Some(area)) = (geo, area) {
            let size = with_state(&window, |s| s.untiled.map(|r| r.size)).unwrap_or_else(|| {
                Size::from((
                    (area.size.w as f64 * FLOAT_FRACTION) as i32,
                    (area.size.h as f64 * FLOAT_FRACTION) as i32,
                ))
            });
            let rect = match with_state(&window, |s| s.untiled) {
                Some(untiled) => Rectangle::new(geo.loc + untiled.loc, size),
                None => Rectangle::new(
                    area.loc
                        + Point::from(((area.size.w - size.w) / 2, (area.size.h - size.h) / 2)),
                    size,
                ),
            };
            self.glide_to(&window, rect, false, true);
        }
        self.relayout_workspace(index, true);
    }

    /// An interactive move of a window on a tiling workspace is starting:
    /// lift it out of the layout so nothing pulls it back mid-drag. Returns
    /// the tile it came from, if it was tiled.
    pub fn lift_for_move(&mut self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let index = self.workspaces.index_of(window)?;
        if !self.layout_of(index).tiles() {
            return None;
        }
        let origin = if self.is_tiled(window) {
            self.space
                .element_geometry(window)
                .map(|g| with_bar(g, window))
        } else {
            None
        };
        with_state(window, |s| s.keep_floating = true);
        origin
    }

    /// A move ended with the pointer at `pointer`. Dropped on a tile (of
    /// any monitor's tiling workspace), the window takes its place and the
    /// two swap. Anywhere else it rejoins the tiles of the workspace it was
    /// dropped on (at the end, if that's another one). A window that wasn't
    /// a tile (`was_tile`) just stays where it was let go.
    pub fn drop_window(&mut self, window: &Window, pointer: Point<f64, Logical>, was_tile: bool) {
        // It may have been dragged onto another monitor's workspace.
        self.sync_workspace_membership();
        // A window floated on purpose stays floating.
        if !was_tile {
            self.relayout_visible(true);
            return;
        }
        with_state(window, |s| s.keep_floating = false);
        let tileable = Self::tileable(window);
        let target = self
            .tile_under(pointer, window)
            .filter(|_| tileable)
            .and_then(|t| Some((self.workspaces.index_of(&t)?, t)));
        if let Some((dest, target)) = target {
            if self.workspaces.index_of(window) != Some(dest) {
                self.workspaces.remove_window(window);
                self.workspaces.list[dest].windows.push(window.clone());
            }
            self.workspaces.list[dest].swap_tiles(window, &target);
        }
        self.relayout_visible(true);
    }

    /// The tiled window drawn at `pointer`, other than `except`; `None` if
    /// a floating window is on top there.
    fn tile_under(&self, pointer: Point<f64, Logical>, except: &Window) -> Option<Window> {
        self.space
            .elements()
            .rev()
            .filter(|w| *w != except)
            .find(|w| {
                self.space
                    .element_geometry(w)
                    .is_some_and(|g| with_bar(g, w).to_f64().contains(pointer))
            })
            .filter(|w| self.is_tiled(w))
            .cloned()
    }

    /// An interactive resize on a tiling workspace is starting: the window
    /// floats at its current size (a maximized one too, rather than
    /// dropping back into a tile) and the other tiles close up.
    pub fn float_for_resize(&mut self, window: &Window) {
        let tiling = self
            .workspaces
            .index_of(window)
            .is_some_and(|index| self.layout_of(index).tiles());
        if !tiling {
            return;
        }
        with_state(window, |s| s.keep_floating = true);
        self.relayout_visible(true);
    }

    /// Before a new window's first configure: on a tiling workspace, ask
    /// for its tile's size straight away, so its first frame already fits.
    pub fn size_new_tile(&mut self, surface: &WlSurface) {
        let Some(window) = self
            .unmapped
            .iter()
            .find(|w| {
                w.toplevel()
                    .is_some_and(|t| t.wl_surface() == surface && !t.is_initial_configure_sent())
            })
            .cloned()
        else {
            return;
        };
        if !Self::tileable(&window) {
            return;
        }
        if !self.workspaces.is_moving() {
            self.normalize_workspaces();
        }
        // It opens on the workspace under the pointer (map_new_window).
        let Some(output) = self.output_under_pointer() else {
            return;
        };
        let Some(index) = self.workspace_on(&output) else {
            return;
        };
        let layout = self.layout_of(index);
        if !layout.tiles() {
            return;
        }
        let (outputs, _) = self.strip_outputs();
        let Some(slot) = outputs.iter().position(|o| *o == output) else {
            return;
        };
        let Some((_, area)) = self.slot_areas(&outputs, slot as isize) else {
            return;
        };
        let rect = match self.config.tiling.style {
            // It splits the focused window's space.
            TilingStyle::Dwindle => {
                self.sync_splits(index);
                let focused = self.focused_window();
                let (ratio, gap, outer_gap) = self.tile_params(index);
                let splits = &mut self.workspaces.list[index].splits;
                splits.insert(window.clone(), focused.as_ref());
                splits
                    .arrange(area, ratio, gap, outer_gap)
                    .into_iter()
                    .find(|(w, _)| *w == window)
                    .map(|(_, rect)| rect)
            }
            // It joins at the end of the stack.
            TilingStyle::Master => {
                let count = self.tiled_windows(index).len() + 1;
                let own = vec![Rectangle::default(); count];
                layout.arrange(area, &own).last().copied()
            }
        };
        let Some(rect) = rect else {
            return;
        };
        let rect = content_rect(rect, titlebar_height(&window));
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|s| {
                set_tiled(&mut s.states, true);
                s.size = Some(rect.size);
            });
        }
    }
}

/// How much the spans `a..a+a_len` and `b..b+b_len` share.
fn span_overlap(a: i32, a_len: i32, b: i32, b_len: i32) -> i32 {
    (a + a_len).min(b + b_len) - a.max(b)
}

/// `rect` (a window's geometry) with the title bar cnde draws above it.
fn with_bar(rect: Rectangle<i32, Logical>, window: &Window) -> Rectangle<i32, Logical> {
    let bar = titlebar_height(window);
    Rectangle::new(
        rect.loc - Point::from((0, bar)),
        rect.size + Size::from((0, bar)),
    )
}

/// A floating spot in `area` for the `n`th window that never floated: two
/// thirds of the area, centred, each one stepped down and right of the last.
fn floating_spot(area: Rectangle<i32, Logical>, n: i32) -> Rectangle<i32, Logical> {
    let size = Size::from((area.size.w * 2 / 3, area.size.h * 2 / 3));
    let step = 32 * (n % 6);
    let x = area.loc.x + (area.size.w - size.w) / 2 - 80 + step;
    let y = area.loc.y + (area.size.h - size.h) / 2 - 80 + step;
    Rectangle::new((x.max(area.loc.x), y.max(area.loc.y)).into(), size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn new_windows_split_the_selected_one() {
        // Two side by side; selecting the left one, a third goes under it.
        let mut tree = SplitTree::default();
        tree.insert(1, None);
        tree.insert(2, Some(&1));
        tree.insert(3, Some(&1));
        let rects = tree.arrange(rect(0, 0, 1000, 600), 0.5, 0, 0);
        assert_eq!(
            rects,
            vec![
                (1, rect(0, 0, 500, 300)),
                (3, rect(0, 300, 500, 300)),
                (2, rect(500, 0, 500, 600)),
            ]
        );
    }

    #[test]
    fn closing_a_window_gives_its_space_to_its_sibling() {
        let mut tree = SplitTree::default();
        for (w, target) in [(1, None), (2, Some(1)), (3, Some(1))] {
            tree.insert(w, target.as_ref());
        }
        tree.remove(&3);
        let rects = tree.arrange(rect(0, 0, 1000, 600), 0.5, 0, 0);
        assert_eq!(
            rects,
            vec![(1, rect(0, 0, 500, 600)), (2, rect(500, 0, 500, 600))]
        );
        tree.remove(&1);
        assert_eq!(
            tree.arrange(rect(0, 0, 1000, 600), 0.5, 0, 0),
            vec![(2, rect(0, 0, 1000, 600))]
        );
    }

    #[test]
    fn swapping_trades_places_and_dropping_in_splits() {
        let mut tree = SplitTree::default();
        for (w, target) in [(1, None), (2, Some(1)), (3, Some(2))] {
            tree.insert(w, target.as_ref());
        }
        tree.swap(&1, &3);
        assert_eq!(tree.leaves(), vec![3, 2, 1]);
        tree.swap(&2, &3);
        assert_eq!(tree.leaves(), vec![2, 3, 1]);
        // 4 isn't in the tree: dropped onto 3, it splits 3's space.
        tree.swap(&4, &3);
        assert_eq!(tree.leaves(), vec![2, 3, 4, 1]);
        tree.sync(&[1, 4], None);
        assert_eq!(tree.leaves(), vec![4, 1]);
    }

    #[test]
    fn dwindle_halves_then_spirals() {
        let area = rect(0, 0, 1000, 600);
        assert_eq!(dwindle(area, 1, 0.5, 10, 0), vec![area]);
        // Two: side by side, split down the middle.
        assert_eq!(
            dwindle(area, 2, 0.5, 10, 0),
            vec![rect(0, 0, 495, 600), rect(505, 0, 495, 600)]
        );
        // Three: the second half (taller than wide) splits top and bottom.
        assert_eq!(
            dwindle(area, 3, 0.5, 10, 0),
            vec![
                rect(0, 0, 495, 600),
                rect(505, 0, 495, 295),
                rect(505, 305, 495, 295)
            ]
        );
        // Four: the last quarter (wider than tall) splits side by side.
        let four = dwindle(area, 4, 0.5, 10, 0);
        assert_eq!(four[2], rect(505, 305, 243, 295));
        assert_eq!(four[3], rect(758, 305, 242, 295));
    }

    #[test]
    fn dwindle_ratio_sizes_the_first_split_and_gaps_inset() {
        let rects = dwindle(rect(0, 0, 1000, 600), 2, 0.6, 0, 20);
        assert_eq!(rects, vec![rect(20, 20, 576, 560), rect(596, 20, 384, 560)]);
    }

    #[test]
    fn one_window_fills_the_area_inside_the_outer_gap() {
        let rects = master_stack(rect(0, 32, 1920, 1048), 1, 0.55, 8, 10);
        assert_eq!(rects, vec![rect(10, 42, 1900, 1028)]);
    }

    #[test]
    fn master_left_stack_right() {
        let rects = master_stack(rect(0, 0, 1000, 600), 3, 0.5, 10, 0);
        // (1000 - 10) * 0.5 = 495 for the master; the stack gets the rest.
        assert_eq!(rects[0], rect(0, 0, 495, 600));
        assert_eq!(rects[1], rect(505, 0, 495, 295));
        assert_eq!(rects[2], rect(505, 305, 495, 295));
    }

    #[test]
    fn stack_heights_add_up_exactly() {
        let rects = master_stack(rect(0, 0, 1000, 1000), 4, 0.5, 7, 0);
        let stack = &rects[1..];
        let total: i32 = stack.iter().map(|r| r.size.h).sum::<i32>() + 7 * 2;
        assert_eq!(total, 1000);
        // Each starts right after the previous one and its gap.
        for pair in stack.windows(2) {
            assert_eq!(pair[1].loc.y, pair[0].loc.y + pair[0].size.h + 7);
        }
        assert_eq!(stack[2].loc.y + stack[2].size.h, 1000);
    }

    #[test]
    fn ratio_is_clamped() {
        let rects = master_stack(rect(0, 0, 1000, 500), 2, 2.0, 0, 0);
        assert_eq!(rects[0].size.w, 900);
        assert_eq!(rects[1].size.w, 100);
    }

    #[test]
    fn tiny_areas_never_give_empty_rects() {
        let rects = master_stack(rect(0, 0, 10, 10), 6, 0.5, 8, 8);
        assert_eq!(rects.len(), 6);
        assert!(rects.iter().all(|r| r.size.w >= 1 && r.size.h >= 1));
    }

    #[test]
    fn floating_keeps_windows_where_they_are() {
        let own = [rect(5, 5, 100, 100), rect(300, 40, 640, 480)];
        assert_eq!(Floating.arrange(rect(0, 0, 1920, 1080), &own), own.to_vec());
        assert!(!Floating.tiles());
    }

    fn workspace(windows: &[u32]) -> Workspace<u32> {
        Workspace::new(1, windows.to_vec())
    }

    #[test]
    fn tile_order_ignores_stacking_once_recorded() {
        let mut ws = workspace(&[1, 2, 3]);
        ws.sync_tiles();
        // Focusing 1 raises it to the top of the stack.
        ws.windows = vec![2, 3, 1];
        assert_eq!(ws.tile_order(), vec![1, 2, 3]);
        // New windows join at the end; gone ones drop out.
        ws.windows = vec![3, 1, 4];
        assert_eq!(ws.tile_order(), vec![1, 3, 4]);
    }

    #[test]
    fn swapping_tiles() {
        let mut ws = workspace(&[1, 2, 3]);
        ws.swap_tiles(&3, &1);
        assert_eq!(ws.tile_order(), vec![3, 2, 1]);
    }
}
