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

use cnde_config::{TilingStyle, TilingToggle, WindowLayout};

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
/// follows the monitor it's shown on). Each split keeps its own ratio
/// (dragged tile edges); the root takes the workspace's main ratio instead.
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
    /// The first child's share of the space, then the children.
    Split(f64, Box<Node<W>>, Box<Node<W>>),
}

/// A tile edge being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn horizontal(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    /// Whether it's the far side of its tile (right or bottom).
    fn far(self) -> bool {
        matches!(self, Edge::Right | Edge::Bottom)
    }
}

/// Which split a dragged edge moved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dragged {
    /// The root split: its ratio is the workspace's main ratio.
    Root(f64),
    Inner,
}

/// The smallest a tile gets dragged to along `total`: 10% or 120 px, though
/// never more than half.
fn min_tile(total: i32) -> i32 {
    ((total as f64 * 0.1) as i32).max(120).min(total / 2)
}

/// `rect` cut in two by a split of `ratio`: whether side by side, then the
/// two halves.
fn halves(
    rect: Rectangle<i32, Logical>,
    ratio: f64,
    gap: i32,
) -> (bool, Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
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
    (wide, ra, rb)
}

impl<W: PartialEq + Clone> Node<W> {
    fn contains(&self, w: &W) -> bool {
        match self {
            Node::Leaf(leaf) => leaf == w,
            Node::Split(_, a, b) => a.contains(w) || b.contains(w),
        }
    }

    fn leaves(&self, out: &mut Vec<W>) {
        match self {
            Node::Leaf(leaf) => out.push(leaf.clone()),
            Node::Split(_, a, b) => {
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
                *self = Node::Split(0.5, Box::new(old), Box::new(Node::Leaf(new.clone())));
                true
            }
            Node::Leaf(_) => false,
            Node::Split(_, a, b) => a.split(target, new) || b.split(target, new),
        }
    }

    /// Take out the leaf holding `w`; its sibling takes the split's place.
    /// `None` when `w` was this whole node.
    fn remove(self, w: &W) -> Option<Node<W>> {
        match self {
            Node::Leaf(leaf) => (leaf != *w).then_some(Node::Leaf(leaf)),
            Node::Split(ratio, a, b) => match (a.remove(w), b.remove(w)) {
                (Some(a), Some(b)) => Some(Node::Split(ratio, Box::new(a), Box::new(b))),
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
            Node::Split(_, x, y) => {
                x.trade(a, b);
                y.trade(a, b);
            }
        }
    }

    /// `root` overrides this split's ratio (the root's main ratio).
    fn place(
        &self,
        rect: Rectangle<i32, Logical>,
        root: Option<f64>,
        gap: i32,
        out: &mut Vec<(W, Rectangle<i32, Logical>)>,
    ) {
        let (ratio, a, b) = match self {
            Node::Leaf(leaf) => return out.push((leaf.clone(), rect)),
            Node::Split(ratio, a, b) => (root.unwrap_or(*ratio), a, b),
        };
        let (_, ra, rb) = halves(rect, ratio.clamp(MIN_RATIO, MAX_RATIO), gap);
        a.place(ra, None, gap, out);
        b.place(rb, None, gap, out);
    }

    /// Move `w`'s `edge` to `pos` by changing the nearest split above it
    /// that has that edge as its boundary (none if the edge is the area's).
    fn drag(
        &mut self,
        w: &W,
        rect: Rectangle<i32, Logical>,
        root: Option<f64>,
        gap: i32,
        edge: Edge,
        pos: i32,
    ) -> Option<Dragged> {
        let Node::Split(ratio, a, b) = self else {
            return None;
        };
        let current = root.unwrap_or(*ratio).clamp(MIN_RATIO, MAX_RATIO);
        let (wide, ra, rb) = halves(rect, current, gap);
        let in_first = a.contains(w);
        let found = if in_first {
            a.drag(w, ra, None, gap, edge, pos)
        } else {
            b.drag(w, rb, None, gap, edge, pos)
        };
        if found.is_some() {
            return found;
        }
        // The boundary is the first child's far edge or the second's near one.
        if wide != edge.horizontal() || in_first != edge.far() {
            return None;
        }
        let (start, total) = if wide {
            (rect.loc.x, rect.size.w)
        } else {
            (rect.loc.y, rect.size.h)
        };
        let share = total - gap;
        let first = if in_first {
            pos - start
        } else {
            pos - gap - start
        };
        let min = min_tile(share);
        let new = (first.clamp(min, (share - min).max(min)) as f64 / share.max(1) as f64)
            .clamp(MIN_RATIO, MAX_RATIO);
        if root.is_some() {
            return Some(Dragged::Root(new));
        }
        *ratio = new;
        Some(Dragged::Inner)
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
            root.place(inset(area, outer_gap), Some(ratio), gap, &mut out);
        }
        out
    }

    /// Drag `w`'s `edge` to `pos` (see [`arrange`](Self::arrange) for the
    /// other arguments). A root split isn't changed here: it's reported for
    /// the caller to store as the main ratio.
    #[allow(clippy::too_many_arguments)]
    pub fn drag_edge(
        &mut self,
        w: &W,
        area: Rectangle<i32, Logical>,
        ratio: f64,
        gap: i32,
        outer_gap: i32,
        edge: Edge,
        pos: i32,
    ) -> Option<Dragged> {
        self.root
            .as_mut()?
            .drag(w, inset(area, outer_gap), Some(ratio), gap, edge, pos)
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

/// The main ratio that puts the main/stack boundary at `pos` (the main
/// window's right edge if `main`, else the stack's left edge).
pub fn master_ratio_at(
    area: Rectangle<i32, Logical>,
    gap: i32,
    outer_gap: i32,
    main: bool,
    pos: i32,
) -> f64 {
    let inner = inset(area, outer_gap);
    let share = inner.size.w - gap;
    let width = pos - inner.loc.x - if main { 0 } else { gap };
    let min = min_tile(share);
    (width.clamp(min, (share - min).max(min)) as f64 / share.max(1) as f64)
        .clamp(MIN_RATIO, MAX_RATIO)
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

/// The layout a workspace uses: with `toggle = "all"` the config's, for
/// every workspace; otherwise its own (Super+T) or else the config's.
fn resolve_mode(
    toggle: TilingToggle,
    own: Option<WindowLayout>,
    config: WindowLayout,
) -> WindowLayout {
    match toggle {
        TilingToggle::All => config,
        TilingToggle::Workspace => own.unwrap_or(config),
    }
}

fn flipped(mode: WindowLayout) -> WindowLayout {
    match mode {
        WindowLayout::Floating => WindowLayout::Tiling,
        WindowLayout::Tiling => WindowLayout::Floating,
    }
}

impl State {
    /// The layout workspace `index` uses now.
    fn layout_mode(&self, index: usize) -> WindowLayout {
        let own = self.workspaces.list.get(index).and_then(|ws| ws.layout);
        resolve_mode(self.config.tiling.toggle, own, self.config.tiling.layout)
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
        // Tiles don't maximize: one that arrives maximized (moved here from a
        // floating workspace) joins them.
        if self.layout_of(index).tiles() {
            for window in self.workspaces.list[index].windows.clone() {
                if Self::placement_of(&window) == Placement::Maximized {
                    self.unplace_to_tile(&window);
                }
            }
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

    /// Super+T: switch between floating and tiling: every workspace
    /// (saved to the config so Settings agrees) or just the current one.
    pub fn toggle_tiling(&mut self) {
        if self.config.tiling.toggle == TilingToggle::All {
            let was: Vec<_> = (0..self.workspaces.list.len())
                .map(|i| self.layout_mode(i))
                .collect();
            let next = flipped(self.config.tiling.layout);
            debug!(?next, "global layout");
            self.overview_before_relayout();
            self.config.tiling.layout = next;
            for (index, was) in was.into_iter().enumerate() {
                self.workspaces.list[index].layout = None;
                self.layout_switched(index, was);
            }
            self.save_tiling_layout(next);
            return;
        }
        let Some(index) = self.current_workspace() else {
            return;
        };
        let was = self.layout_mode(index);
        let next = flipped(was);
        debug!(index, ?next, "workspace layout");
        // In the overview, the thumbnails slide to their new places too.
        self.overview_before_relayout();
        self.workspaces.list[index].layout = Some(next);
        self.layout_switched(index, was);
    }

    /// Workspace `index` was using `was` and may now use another layout.
    /// Starting to tile, each window takes the tile nearest where it
    /// floats; stopping, each floats where its tile was, grown over the
    /// gaps.
    fn layout_switched(&mut self, index: usize, was: WindowLayout) {
        let now = self.layout_mode(index);
        if now != was {
            let (outputs, _) = self.strip_outputs();
            let slot = self.slot_of(index);
            let origin = slot
                .and_then(|slot| outputs.get(slot))
                .and_then(|o| self.space.output_geometry(o))
                .map(|g| g.loc);
            // Its monitor, or the main one while it isn't shown.
            if let Some((geo, area)) = self.slot_areas(&outputs, slot.unwrap_or(0) as isize) {
                match now {
                    WindowLayout::Tiling => self.tile_nearest(index, geo, area, origin),
                    WindowLayout::Floating => self.untile_in_place(index, geo, area, origin),
                }
            }
        }
        self.relayout_workspace(index, true);
    }

    /// Order workspace `index`'s tiles so each window gets the one nearest
    /// where it floats now (`origin`: its monitor's, if it's shown).
    fn tile_nearest(
        &mut self,
        index: usize,
        geo: Rectangle<i32, Logical>,
        area: Rectangle<i32, Logical>,
        origin: Option<Point<i32, Logical>>,
    ) {
        let mut windows = Vec::new();
        let mut rects = Vec::new();
        for window in self.workspaces.list[index].windows.clone() {
            // Maximized or snapped, it joins the tiles too, from where it
            // floated before that.
            let restored = self.unplace_to_tile(&window);
            if !Self::tileable(&window) {
                continue;
            }
            let local = restored
                .or_else(|| self.space.element_geometry(&window))
                .zip(origin)
                .map(|(rect, origin)| Rectangle::new(rect.loc - origin, rect.size))
                .unwrap_or_else(|| {
                    with_state(&window, |s| Rectangle::new(s.local, window.geometry().size))
                });
            // For a window floated off the tiles later (see `untiled`).
            with_state(&window, |s| s.untiled = Some(local));
            rects.push(Rectangle::new(geo.loc + local.loc, local.size));
            windows.push(window);
        }
        let (ratio, gap, outer_gap) = self.tile_params(index);
        let tiles = match self.config.tiling.style {
            TilingStyle::Dwindle => dwindle(area, windows.len(), ratio, gap, outer_gap),
            TilingStyle::Master => master_stack(area, windows.len(), ratio, gap, outer_gap),
        };
        let order: Vec<Window> = nearest_tiles(&rects, &tiles)
            .into_iter()
            .map(|i| windows[i].clone())
            .collect();
        let workspace = &mut self.workspaces.list[index];
        // The dwindle tiles above are each a split of the one before.
        workspace.splits = SplitTree::default();
        for (i, window) in order.iter().enumerate() {
            let before = i.checked_sub(1).map(|i| &order[i]);
            workspace.splits.insert(window.clone(), before);
        }
        workspace.tiles = order;
    }

    /// Float workspace `index`'s tiled windows where their tiles are, as if
    /// there were no gaps.
    fn untile_in_place(
        &mut self,
        index: usize,
        geo: Rectangle<i32, Logical>,
        area: Rectangle<i32, Logical>,
        origin: Option<Point<i32, Logical>>,
    ) {
        let windows: Vec<Window> = self.workspaces.list[index]
            .tile_order()
            .into_iter()
            .filter(Self::tileable)
            .collect();
        let (ratio, _, _) = self.tile_params(index);
        let slots: Vec<(Window, Rectangle<i32, Logical>)> = match self.config.tiling.style {
            TilingStyle::Dwindle => {
                let mut splits = self.workspaces.list[index].splits.clone();
                splits.sync(&windows, None);
                splits.arrange(area, ratio, 0, 0)
            }
            TilingStyle::Master => {
                let rects = master_stack(area, windows.len(), ratio, 0, 0);
                windows.into_iter().zip(rects).collect()
            }
        };
        for (window, slot) in slots {
            let rect = content_rect(slot, titlebar_height(&window));
            let back = Rectangle::new(rect.loc - geo.loc, rect.size);
            with_state(&window, |s| {
                s.untiled = None;
                s.local = back.loc;
            });
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
        let all = self.config.tiling.toggle == TilingToggle::All;
        for index in 0..self.workspaces.list.len() {
            let own = self.workspaces.list[index].layout;
            // One global mode: Super+T overrides give way to the setting.
            if all {
                self.workspaces.list[index].layout = None;
            }
            if all || own.is_none() {
                let was = resolve_mode(old.toggle, own, old.layout);
                self.layout_switched(index, was);
            }
        }
        self.relayout_visible(true);
    }

    /// Write `tiling.layout` to config.toml, keeping the rest of the file.
    /// The reload this triggers finds nothing changed, as the live config
    /// already says so.
    fn save_tiling_layout(&self, mode: WindowLayout) {
        let Some(path) = &self.config_path else {
            return;
        };
        let value = match mode {
            WindowLayout::Floating => "floating",
            WindowLayout::Tiling => "tiling",
        };
        let result = cnde_config::ConfigFile::open(path).and_then(|mut file| {
            file.set("tiling.layout", value);
            file.save()
        });
        if let Err(err) = result {
            tracing::warn!(%err, "couldn't save the tiling mode to the config");
        }
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

    /// The monitor area (panels excluded) `window`'s tiles are laid out in.
    pub fn tile_area_now(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let (outputs, _) = self.strip_outputs();
        let output = self.output_for(window)?;
        let slot = outputs.iter().position(|o| *o == output)?;
        self.slot_areas(&outputs, slot as isize).map(|(_, area)| area)
    }

    /// The tile `window` rests in, whole (title bar included), where the
    /// pointer sees it.
    pub fn tile_slot_now(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let index = self.workspaces.index_of(window)?;
        let area = self.tile_area_now(window)?;
        self.tile_rects(index, area)
            .into_iter()
            .find(|(w, _)| w == window)
            .map(|(_, rect)| rect)
    }

    /// An interactive resize of a tile moves `window`'s `edge` to `pos`:
    /// the split on that side follows and the neighbours take up the
    /// difference. Edges on the screen's border, and the stack's inner
    /// edges (always equal), do nothing.
    pub fn drag_tile_edge(&mut self, window: &Window, edge: Edge, pos: i32) {
        let Some(index) = self.workspaces.index_of(window) else {
            return;
        };
        let Some(area) = self.tile_area_now(window) else {
            return;
        };
        self.sync_splits(index);
        let (ratio, gap, outer_gap) = self.tile_params(index);
        let new = match self.config.tiling.style {
            TilingStyle::Dwindle => self.workspaces.list[index]
                .splits
                .drag_edge(window, area, ratio, gap, outer_gap, edge, pos)
                .map(|dragged| match dragged {
                    Dragged::Root(ratio) => Some(ratio),
                    Dragged::Inner => None,
                }),
            TilingStyle::Master => {
                let windows = self.tiled_windows(index);
                let first = windows.first() == Some(window);
                let boundary = if first { Edge::Right } else { Edge::Left };
                (windows.len() > 1 && edge == boundary)
                    .then(|| Some(master_ratio_at(area, gap, outer_gap, first, pos)))
            }
        };
        let Some(new) = new else {
            return;
        };
        if let Some(ratio) = new {
            self.workspaces.list[index].master_ratio = Some(ratio);
        }
        self.relayout_workspace(index, false);
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

/// Which window takes each of `tiles`, as indices into `windows`: the
/// closest window and tile (by their centres) pair up first, then the
/// closest of the rest, so each window moves as little as it can.
fn nearest_tiles(
    windows: &[Rectangle<i32, Logical>],
    tiles: &[Rectangle<i32, Logical>],
) -> Vec<usize> {
    let centre = |r: &Rectangle<i32, Logical>| {
        (
            r.loc.x as i64 * 2 + r.size.w as i64,
            r.loc.y as i64 * 2 + r.size.h as i64,
        )
    };
    let mut pairs: Vec<(i64, usize, usize)> = Vec::new();
    for (w, window) in windows.iter().enumerate() {
        for (t, tile) in tiles.iter().enumerate() {
            let ((wx, wy), (tx, ty)) = (centre(window), centre(tile));
            pairs.push(((wx - tx).pow(2) + (wy - ty).pow(2), w, t));
        }
    }
    pairs.sort();
    let mut taken = vec![None; tiles.len()];
    let mut placed = vec![false; windows.len()];
    for (_, w, t) in pairs {
        if !placed[w] && taken[t].is_none() {
            placed[w] = true;
            taken[t] = Some(w);
        }
    }
    taken.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_take_their_nearest_tiles() {
        let tiles = master_stack(rect(0, 0, 1000, 600), 3, 0.5, 0, 0);
        // Bottom right, left, top right: each lands where it already was.
        let windows = [rect(700, 400, 200, 150), rect(50, 100, 300, 300), rect(600, 0, 300, 200)];
        assert_eq!(nearest_tiles(&windows, &tiles), vec![1, 2, 0]);
        // One window, still a tile.
        assert_eq!(nearest_tiles(&windows[..1], &tiles[..1]), vec![0]);
    }

    #[test]
    fn mode_resolution() {
        use TilingToggle::{All, Workspace};
        use WindowLayout::{Floating, Tiling};
        // Global: the config wins over any workspace override.
        assert_eq!(resolve_mode(All, Some(Tiling), Floating), Floating);
        assert_eq!(resolve_mode(All, None, Tiling), Tiling);
        // Per workspace: the override wins, else the config.
        assert_eq!(resolve_mode(Workspace, Some(Tiling), Floating), Tiling);
        assert_eq!(resolve_mode(Workspace, None, Tiling), Tiling);
        assert_eq!(flipped(Tiling), Floating);
        assert_eq!(flipped(Floating), Tiling);
    }

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

    fn three() -> SplitTree<u32> {
        // 1 | (2 over 3), in a 1010x600 area with a 10 px gap.
        let mut tree = SplitTree::default();
        tree.insert(1, None);
        tree.insert(2, Some(&1));
        tree.insert(3, Some(&2));
        tree
    }

    #[test]
    fn dragging_an_edge_moves_the_split_on_that_side() {
        let area = rect(0, 0, 1010, 600);
        let mut tree = three();
        // 1's right edge is the root split: reported, not stored.
        let dragged = tree.drag_edge(&1, area, 0.5, 10, 0, Edge::Right, 600);
        assert_eq!(dragged, Some(Dragged::Root(0.6)));
        let rects = tree.arrange(area, 0.6, 10, 0);
        assert_eq!(rects[0], (1, rect(0, 0, 600, 600)));
        assert_eq!(rects[1].1, rect(610, 0, 400, 295));
        // 2's bottom edge is the inner split, 3's top edge the same one.
        assert_eq!(
            tree.drag_edge(&2, area, 0.6, 10, 0, Edge::Bottom, 400),
            Some(Dragged::Inner)
        );
        let rects = tree.arrange(area, 0.6, 10, 0);
        assert_eq!(rects[1].1, rect(610, 0, 400, 400));
        assert_eq!(rects[2].1, rect(610, 410, 400, 190));
        assert_eq!(
            tree.drag_edge(&3, area, 0.6, 10, 0, Edge::Top, 300),
            Some(Dragged::Inner)
        );
        assert_eq!(tree.arrange(area, 0.6, 10, 0)[1].1.size.h, 290);
        // 2 and 3 meet the root split with their left edges.
        assert_eq!(
            tree.drag_edge(&3, area, 0.6, 10, 0, Edge::Left, 410),
            Some(Dragged::Root(0.4))
        );
    }

    #[test]
    fn edges_on_the_screen_border_do_nothing() {
        let area = rect(0, 0, 1010, 600);
        let mut tree = three();
        let before = tree.clone();
        for (w, edge) in [
            (1, Edge::Left),
            (1, Edge::Top),
            (1, Edge::Bottom),
            (2, Edge::Right),
            (2, Edge::Top),
            (3, Edge::Right),
            (3, Edge::Bottom),
        ] {
            assert_eq!(tree.drag_edge(&w, area, 0.5, 10, 0, edge, 300), None);
        }
        assert_eq!(tree, before);
    }

    #[test]
    fn dragged_tiles_keep_a_minimum_size() {
        let area = rect(0, 0, 1010, 600);
        let mut tree = three();
        assert_eq!(
            tree.drag_edge(&1, area, 0.5, 10, 0, Edge::Right, 5),
            Some(Dragged::Root(0.12))
        );
        assert_eq!(
            tree.drag_edge(&1, area, 0.5, 10, 0, Edge::Right, 5000),
            Some(Dragged::Root(0.88))
        );
        tree.drag_edge(&2, area, 0.5, 10, 0, Edge::Bottom, 0);
        assert_eq!(tree.arrange(area, 0.5, 10, 0)[1].1.size.h, 120);
        tree.drag_edge(&2, area, 0.5, 10, 0, Edge::Bottom, 9999);
        assert_eq!(tree.arrange(area, 0.5, 10, 0)[2].1.size.h, 120);
    }

    #[test]
    fn ratios_survive_inserting_removing_and_swapping() {
        let area = rect(0, 0, 1010, 600);
        let mut tree = three();
        tree.drag_edge(&2, area, 0.5, 10, 0, Edge::Bottom, 400);
        assert_eq!(tree.arrange(area, 0.5, 10, 0)[1].1.size.h, 400);
        // Trading places keeps the proportions of the places.
        tree.swap(&2, &3);
        assert_eq!(tree.arrange(area, 0.5, 10, 0)[1].1.size.h, 400);
        // A new window splits a leaf: the old splits stay as they were.
        tree.insert(4, Some(&3));
        let rects = tree.arrange(area, 0.5, 10, 0);
        assert_eq!(rects[0].1.size.w, 500);
        assert_eq!(rects[1].1.size.h, 400);
        // Closing a window gives its space to its sibling, ratios intact.
        tree.remove(&4);
        assert_eq!(tree.arrange(area, 0.5, 10, 0)[1].1.size.h, 400);
    }

    #[test]
    fn dragging_the_main_edge_sets_the_master_ratio() {
        let area = rect(0, 0, 1010, 600);
        assert_eq!(master_ratio_at(area, 10, 0, true, 600), 0.6);
        // The stack's left edge sits a gap further along.
        assert_eq!(master_ratio_at(area, 10, 0, false, 610), 0.6);
        assert_eq!(master_ratio_at(area, 10, 0, true, 0), 0.12);
    }
}
