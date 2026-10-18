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

use cnde_config::WindowLayout;

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
    let inner = Rectangle::new(
        area.loc + Point::from((outer_gap, outer_gap)),
        Size::from((
            (area.size.w - 2 * outer_gap).max(1),
            (area.size.h - 2 * outer_gap).max(1),
        )),
    );
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
            WindowLayout::Tiling => Box::new(MasterStack {
                ratio: self.master_ratio(index),
                gap: self.config.tiling.gap as i32,
                outer_gap: self.config.tiling.outer_gap as i32,
            }),
        }
    }

    /// Whether `window` takes part in its workspace's layout, if that tiles.
    /// Dialogs and fixed-size windows float above the tiles.
    pub fn tileable(window: &Window) -> bool {
        let Some(toplevel) = window.toplevel() else {
            return false;
        };
        let (placement, minimized, keep_floating) =
            with_state(window, |s| (s.placement, s.minimized, s.keep_floating));
        if placement != Placement::Floating || minimized || keep_floating {
            return false;
        }
        if toplevel.parent().is_some() {
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
            let own = with_bar(floating_rest(window, geo), window);
            let rect = layout.arrange(area, &[own]).first().copied()?;
            return Some(content_rect(rect, titlebar_height(window)));
        }
        let windows = self.tiled_windows(index);
        let i = windows.iter().position(|w| w == window)?;
        let own: Vec<_> = windows
            .iter()
            .map(|w| with_bar(floating_rest(w, geo), w))
            .collect();
        let rect = layout.arrange(area, &own).get(i).copied()?;
        Some(content_rect(rect, titlebar_height(window)))
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
        let origin = self
            .slot_of(index)
            .and_then(|slot| {
                let (outputs, _) = self.strip_outputs();
                self.space.output_geometry(outputs.get(slot)?)
            })
            .map(|g| g.loc);
        for window in windows {
            if !Self::tileable(&window) {
                continue;
            }
            let rect = self
                .space
                .element_geometry(&window)
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
                    let back = with_state(&window, |s| s.untiled.take()).or(rect);
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
        // It joins at the end of the stack.
        let count = self.tiled_windows(index).len() + 1;
        let own = vec![Rectangle::default(); count];
        let Some(rect) = layout.arrange(area, &own).last().copied() else {
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

/// `rect` (a window's geometry) with the title bar cnde draws above it.
fn with_bar(rect: Rectangle<i32, Logical>, window: &Window) -> Rectangle<i32, Logical> {
    let bar = titlebar_height(window);
    Rectangle::new(
        rect.loc - Point::from((0, bar)),
        rect.size + Size::from((0, bar)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
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
