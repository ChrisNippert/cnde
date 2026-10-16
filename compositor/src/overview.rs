//! The overview (ARCHITECTURE.md §4.4, §5.2): tap Super and every monitor
//! zooms out of the workspace strip. The wallpaper frosts over, each
//! workspace becomes a frosted-glass card with its windows spread out so
//! none overlap, and neighbouring workspaces peek in at the sides. Click a
//! window to go to it, or an empty spot on a card to go to that workspace.
//!
//! It's the same strip, just drawn differently: the strip can keep moving
//! (Super+Alt+arrows, a swipe, the scroll wheel) while the overview is open.

use std::{collections::HashMap, time::Instant};

use smithay::{
    backend::renderer::{
        element::{
            Id, Kind,
            solid::{SolidColorBuffer, SolidColorRenderElement},
        },
        gles::{GlesPixelProgram, GlesRenderer, element::PixelShaderElement},
        utils::CommitCounter,
    },
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
};
use tracing::{debug, warn};

use crate::{
    animation::{Animation, presets},
    effects::lerp_rect,
    render::{OutputElement, Placed, Zoom},
    state::State,
    wallpaper::Wallpapers,
    workspaces::WorkspaceId,
};

/// Workspace card size, as a fraction of the monitor.
const ZOOM: f64 = 0.68;
/// Space between neighbouring cards, in logical pixels.
const GAP: f64 = 48.0;
/// Cards sit this fraction of the monitor height lower than centre, leaving
/// room for the search box above.
const SHIFT_DOWN: f64 = 0.05;
/// How much the frosted wallpaper behind the cards is darkened.
const DIM: f32 = 0.25;
/// Frosted-glass workspace tiles: a light veil with a faint edge
/// (premultiplied RGBA), and their corner radius in logical pixels.
const TILE_COLOR: [f32; 4] = [0.08, 0.08, 0.08, 0.08];
const TILE_BORDER: [f32; 4] = [0.12, 0.12, 0.12, 0.12];
const TILE_RADIUS: f64 = 18.0;
/// Space around and between spread-out windows on a card (full size).
const SPREAD_PADDING: f64 = 32.0;
const SPREAD_GAP: f64 = 28.0;
/// Pointer this close to the top-left corner opens the overview.
const HOT_CORNER: f64 = 1.0;
/// ...and must leave this far before the corner works again.
const HOT_CORNER_REARM: f64 = 24.0;
/// Clicks on the Activities button this soon after the hot corner fired
/// are ignored (GNOME does the same).
const HOT_CORNER_CLICK_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

pub struct Overview {
    /// Where it's heading (open or closed).
    open: bool,
    anim: Option<Animation>,
    dim: SolidColorBuffer,
    dim_alpha: f32,
    /// Stable element ids for workspace cards, so damage tracking can follow
    /// each card from frame to frame (square fallback without the shader).
    card_ids: HashMap<WorkspaceId, Id>,
    tile_shader: Option<GlesPixelProgram>,
    tiles: HashMap<WorkspaceId, Tile>,
    hot_corner_armed: bool,
    /// When the hot corner last opened the overview: a click on the panel's
    /// Activities button right after is the same gesture, not a toggle.
    hot_corner_at: Option<Instant>,
    /// Scroll wheel travel not yet turned into workspace steps (v120 units).
    scroll: f64,
    /// Had the keyboard before opening; gets it back unless a click picks
    /// another window.
    prev_focus: Option<Window>,
    /// Chosen on close, for when the shell's search box lets go of the
    /// keyboard (see `layer_destroyed`).
    focus_after: Option<Window>,
}

struct Tile {
    element: PixelShaderElement,
    area: Rectangle<i32, Logical>,
    alpha: f32,
}

/// What one monitor shows while the overview is (partly) open.
pub struct Scene {
    pub progress: f64,
    /// Bottom to top.
    pub windows: Vec<Placed>,
    pub cards: Vec<Card>,
}

pub struct Card {
    pub workspace: WorkspaceId,
    pub index: usize,
    /// Zoomed, in global coordinates.
    pub rect: Rectangle<f64, Logical>,
}

pub enum Hit {
    Window(Window, usize),
    Workspace(usize),
}

impl Overview {
    pub fn new() -> Self {
        Self {
            open: false,
            anim: None,
            dim: SolidColorBuffer::default(),
            dim_alpha: -1.0,
            card_ids: HashMap::new(),
            tile_shader: None,
            tiles: HashMap::new(),
            hot_corner_armed: true,
            hot_corner_at: None,
            scroll: 0.0,
            prev_focus: None,
            focus_after: None,
        }
    }

    pub fn take_focus_after(&mut self) -> Option<Window> {
        self.focus_after.take()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 0 = closed, 1 = fully open.
    pub fn progress(&self, now: Instant) -> f64 {
        match &self.anim {
            Some(anim) => anim.value_at(now),
            None if self.open => 1.0,
            None => 0.0,
        }
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.anim.as_ref().is_some_and(|a| !a.is_done(now))
    }

    fn set_open(&mut self, open: bool) {
        if open == self.open {
            return;
        }
        let now = Instant::now();
        let from = self.progress(now);
        self.open = open;
        self.scroll = 0.0;
        self.anim = Some(Animation::new(
            from,
            if open { 1.0 } else { 0.0 },
            presets::OVERVIEW,
        ));
    }
}

impl State {
    /// The Activities button (the shell asks over IPC). Ignored just after
    /// the hot corner fired: you hit the corner, then clicked it.
    pub fn activities_clicked(&mut self) {
        let just_opened = self
            .overview
            .hot_corner_at
            .is_some_and(|at| at.elapsed() < HOT_CORNER_CLICK_GRACE);
        if !just_opened {
            self.toggle_overview();
        }
    }

    pub fn toggle_overview(&mut self) {
        let open = !self.overview.is_open();
        self.set_overview(open);
    }

    pub fn set_overview(&mut self, open: bool) {
        if open == self.overview.is_open() {
            return;
        }
        debug!(open, "overview");
        self.overview.set_open(open);
        let event = match open {
            true => {
                let (outputs, main) = self.strip_outputs();
                let output = outputs.get(main).map(|o| o.name()).unwrap_or_default();
                cnde_ipc::Event::OverviewOpened { output }
            }
            false => cnde_ipc::Event::OverviewClosed,
        };
        self.ipc.broadcast(&event);
        if open {
            // Typing must not reach the window behind the overview.
            self.overview.prev_focus = self.focused_window();
            if self.exclusive_layer().is_none()
                && let Some(keyboard) = self.seat.get_keyboard()
            {
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
        } else {
            let previous = self
                .overview
                .prev_focus
                .take()
                .filter(|w| self.workspaces.list.iter().any(|ws| ws.windows.contains(w)));
            // A window picked by a click is already focused (or, while the
            // search box holds the keyboard, raised and waiting).
            let picked = self.overview.focus_after.take();
            let next = picked.or(previous).or_else(|| {
                self.workspaces.list[self.workspaces.active]
                    .windows
                    .last()
                    .cloned()
            });
            if self.exclusive_layer().is_some() {
                self.overview.focus_after = next;
            } else if self.focused_window().is_none() {
                self.focus_window(next.as_ref());
            }
        }
        // Windows stop (or start) taking pointer input.
        self.queue_pointer_refresh();
        self.queue_redraw();
    }

    /// Called every loop iteration: drop a finished animation.
    pub fn finish_overview_animation(&mut self, now: Instant) {
        if self.overview.anim.as_ref().is_some_and(|a| a.is_done(now)) {
            self.overview.anim = None;
        }
    }

    /// The windows (and, in the overview, workspace cards) `output` shows
    /// right now; `None` when nothing is moving and the space is accurate.
    ///
    /// On each monitor every card is that monitor's shape, whichever monitor
    /// its workspace normally sits on, and its windows spread out into a
    /// grid. Everything is interpolated by the open progress, so opening is
    /// one continuous zoom from the real layout.
    pub fn scene(&self, output: &Output, now: Instant) -> Option<Scene> {
        let progress = self.overview.progress(now);
        if progress <= 0.0 {
            return self.moving_layout(output, now).map(|windows| Scene {
                progress: 0.0,
                windows,
                cards: Vec::new(),
            });
        }

        let (outputs, main) = self.strip_outputs();
        let k = outputs.iter().position(|o| o == output)? as f64;
        let geo = self.space.output_geometry(output)?.to_f64();
        let scale = 1.0 + (ZOOM - 1.0) * progress;
        let card_size = Size::from((geo.size.w * scale, geo.size.h * scale));
        let centre = geo.loc + Point::from((geo.size.w / 2.0, geo.size.h / 2.0));
        let card_at = |slot: f64| {
            let cx = centre.x + (slot - k) * (card_size.w + GAP * progress);
            let cy = centre.y + geo.size.h * SHIFT_DOWN * progress;
            Rectangle::new(
                Point::from((cx - card_size.w / 2.0, cy - card_size.h / 2.0)),
                card_size,
            )
        };
        // The spread layout is worked out at full size, once per card.
        let full_card = Size::from((geo.size.w * ZOOM, geo.size.h * ZOOM));

        let position = self.workspaces.position(now);
        let carried = self
            .workspaces
            .motion
            .as_ref()
            .and_then(|m| m.carried.clone());
        let mut scene = Scene {
            progress,
            windows: Vec::new(),
            cards: Vec::new(),
        };
        for (index, workspace) in self.workspaces.list.iter().enumerate() {
            let slot = index as f64 - position + main as f64;
            let card = card_at(slot);
            if !card.overlaps(geo) {
                continue;
            }
            scene.cards.push(Card {
                workspace: workspace.id,
                index,
                rect: card,
            });
            let (from, t) = (slot.floor() as isize, slot - slot.floor());
            let Some(home) = self
                .slot_areas(&outputs, from)
                .zip(self.slot_areas(&outputs, from + 1))
                .map(|((a, _), (b, _))| lerp_rect(a.to_f64(), b.to_f64(), t))
            else {
                continue;
            };
            // Each window as a fraction of its own monitor, so it maps onto
            // a card of any shape.
            let mut windows = Vec::new();
            for window in &workspace.windows {
                if carried.as_ref() == Some(window) {
                    continue;
                }
                let (Some(a), Some(b)) = (
                    self.rest_rect(window, &outputs, from),
                    self.rest_rect(window, &outputs, from + 1),
                ) else {
                    continue;
                };
                let real = lerp_rect(a.to_f64(), b.to_f64(), t);
                windows.push((window.clone(), real, fraction_of(real, home)));
            }
            let sizes: Vec<Size<f64, Logical>> = windows
                .iter()
                .map(|(_, _, f)| Size::from((f.size.w * full_card.w, f.size.h * full_card.h)))
                .collect();
            let centres: Vec<Point<f64, Logical>> = windows
                .iter()
                .map(|(_, _, f)| f.loc + Point::from((f.size.w / 2.0, f.size.h / 2.0)))
                .collect();
            let spread = spread(&sizes, &centres, full_card, SPREAD_PADDING, SPREAD_GAP);
            for ((window, real, from), to) in windows.into_iter().zip(spread) {
                let to = Rectangle::new(
                    Point::from((to.loc.x / full_card.w, to.loc.y / full_card.h)),
                    Size::from((to.size.w / full_card.w, to.size.h / full_card.h)),
                );
                let fraction = lerp_rect(from, to, progress);
                let target = Rectangle::new(
                    card.loc
                        + Point::from((fraction.loc.x * card.size.w, fraction.loc.y * card.size.h)),
                    Size::from((fraction.size.w * card.size.w, fraction.size.h * card.size.h)),
                );
                scene.windows.push(Placed {
                    window,
                    rect: real,
                    zoom: Zoom::mapping(real, target),
                    workspace: index,
                });
            }
        }
        if let Some(window) = carried
            && let Some(rect) = self.space.element_geometry(&window)
        {
            let workspace = self.workspaces.motion.as_ref().map_or(0, |m| m.to);
            let rect = rect.to_f64();
            let card = card_at(k);
            let target = Rectangle::new(
                card.loc
                    + Point::from((
                        (rect.loc.x - geo.loc.x) * scale,
                        (rect.loc.y - geo.loc.y) * scale,
                    )),
                rect.size.upscale(scale),
            );
            scene.windows.push(Placed {
                window,
                rect,
                zoom: Zoom::mapping(rect, target),
                workspace,
            });
        }
        Some(scene)
    }

    /// What's under `pos` in the open overview.
    fn overview_hit(&self, pos: Point<f64, Logical>) -> Option<Hit> {
        let output = self.space.output_under(pos).next()?.clone();
        let scene = self.scene(&output, Instant::now())?;
        for placed in scene.windows.iter().rev() {
            if placed.zoom.apply(placed.rect).contains(pos) {
                return Some(Hit::Window(placed.window.clone(), placed.workspace));
            }
        }
        scene
            .cards
            .iter()
            .find(|card| card.rect.contains(pos))
            .map(|card| Hit::Workspace(card.index))
    }

    /// A click in the overview: go to the window or workspace under it.
    pub fn overview_click(&mut self, pos: Point<f64, Logical>) {
        let hit = self.overview_hit(pos);
        let (index, window) = match hit {
            Some(Hit::Window(window, index)) => (index, Some(window)),
            Some(Hit::Workspace(index)) => (index, None),
            None => {
                self.set_overview(false);
                return;
            }
        };
        // Bring that workspace onto the monitor that was clicked.
        let (outputs, main) = self.strip_outputs();
        let clicked = self
            .space
            .output_under(pos)
            .next()
            .and_then(|o| outputs.iter().position(|x| x == o))
            .unwrap_or(main);
        let target = index as isize + main as isize - clicked as isize;
        let current = self
            .workspaces
            .motion
            .as_ref()
            .map_or(self.workspaces.active, |m| m.to) as isize;
        if target != current {
            self.step_workspace(target - current, false);
        }
        let target = window.or_else(|| self.workspaces.list[index].windows.last().cloned());
        if let Some(target) = &target {
            self.focus_window(Some(target));
        }
        self.overview.focus_after = target;
        self.set_overview(false);
    }

    /// Mouse wheel in the overview slides the strip, one notch per workspace.
    pub fn overview_scroll(&mut self, v120: f64) {
        self.overview.scroll += v120;
        while self.overview.scroll.abs() >= 120.0 {
            let step = self.overview.scroll.signum();
            self.overview.scroll -= step * 120.0;
            self.step_workspace(step as isize, false);
        }
    }

    /// Top-left corner of the main monitor opens the overview.
    pub fn check_hot_corner(&mut self, pos: Point<f64, Logical>) {
        let (outputs, main) = self.strip_outputs();
        let Some(corner) = outputs
            .get(main)
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| g.loc.to_f64())
        else {
            return;
        };
        let (dx, dy) = (pos.x - corner.x, pos.y - corner.y);
        if dx < HOT_CORNER && dy < HOT_CORNER {
            let dragging = self.seat.get_pointer().is_some_and(|p| p.is_grabbed());
            if self.overview.hot_corner_armed && !dragging {
                self.overview.hot_corner_armed = false;
                self.overview.hot_corner_at = Some(Instant::now());
                self.toggle_overview();
            }
        } else if dx > HOT_CORNER_REARM || dy > HOT_CORNER_REARM {
            self.overview.hot_corner_armed = true;
        }
    }
}

/// The overview's backdrop for `output`, top first: a frosted-glass tile
/// per workspace card, a darkening layer, and the blurred wallpaper fading
/// in over the sharp one.
pub fn backdrop(
    overview: &mut Overview,
    wallpapers: &Wallpapers,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_geo: Rectangle<i32, Logical>,
    scene: Option<&Scene>,
) -> Vec<OutputElement> {
    let Some(scene) = scene.filter(|s| s.progress > 0.0) else {
        return Vec::new();
    };
    let scale = output.current_scale().fractional_scale();
    let progress = scene.progress as f32;
    let mut elements = Vec::new();

    if overview.tile_shader.is_none() {
        match renderer.compile_custom_pixel_shader(
            crate::shaders::ROUNDED_RECT,
            &crate::shaders::rounded_rect_uniforms(),
        ) {
            Ok(shader) => overview.tile_shader = Some(shader),
            Err(err) => warn!(?err, "can't compile the card shader; cards are square"),
        }
    }
    let live: Vec<WorkspaceId> = scene.cards.iter().map(|c| c.workspace).collect();
    overview.tiles.retain(|id, _| live.contains(id));
    for card in &scene.cards {
        let local = Rectangle::new(card.rect.loc - output_geo.loc.to_f64(), card.rect.size);
        let area = local.to_i32_round();
        let element = match &overview.tile_shader {
            Some(shader) => {
                let tile = overview
                    .tiles
                    .entry(card.workspace)
                    .or_insert_with(|| Tile {
                        element: crate::shaders::rounded_rect(
                            shader.clone(),
                            area,
                            TILE_RADIUS,
                            TILE_COLOR,
                            TILE_BORDER,
                            scale,
                            progress,
                        ),
                        area,
                        alpha: progress,
                    });
                // Same element (and id) while it only moves or resizes, so
                // damage tracking follows it; a new one when it fades.
                if tile.alpha != progress {
                    *tile = Tile {
                        element: crate::shaders::rounded_rect(
                            shader.clone(),
                            area,
                            TILE_RADIUS,
                            TILE_COLOR,
                            TILE_BORDER,
                            scale,
                            progress,
                        ),
                        area,
                        alpha: progress,
                    };
                } else if tile.area != area {
                    tile.element.resize(area, None);
                    tile.area = area;
                }
                OutputElement::Shader(tile.element.clone())
            }
            None => {
                let id = overview
                    .card_ids
                    .entry(card.workspace)
                    .or_insert_with(Id::new)
                    .clone();
                OutputElement::Solid(SolidColorRenderElement::new(
                    id,
                    area.to_physical_precise_round(scale),
                    CommitCounter::default(),
                    TILE_COLOR,
                    Kind::Unspecified,
                ))
            }
        };
        elements.push(element);
    }

    let alpha = DIM * progress;
    overview.dim.resize(output_geo.size);
    if alpha != overview.dim_alpha {
        overview.dim_alpha = alpha;
        overview.dim.update(output_geo.size, [0.0, 0.0, 0.0, alpha]);
    }
    elements.push(OutputElement::Solid(SolidColorRenderElement::from_buffer(
        &overview.dim,
        (0, 0),
        scale,
        1.0,
        Kind::Unspecified,
    )));
    if let Some(blurred) = wallpapers.blurred_element(output, progress) {
        elements.push(OutputElement::Wallpaper(blurred));
    }
    elements
}

/// Where `rect` sits in `area`, as fractions of `area`'s size.
fn fraction_of(
    rect: Rectangle<f64, Logical>,
    area: Rectangle<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let (w, h) = (area.size.w.max(1.0), area.size.h.max(1.0));
    Rectangle::new(
        Point::from(((rect.loc.x - area.loc.x) / w, (rect.loc.y - area.loc.y) / h)),
        Size::from((rect.size.w / w, rect.size.h / h)),
    )
}

/// Lay windows (`sizes`, with their current `centres` for ordering) out in
/// rows inside a `area`-sized box without overlapping, as large as fits but
/// never larger than they are. Rows are picked to make them largest; within
/// a row, windows keep their left-to-right order, so things stay roughly
/// where they were. Returns rects in box coordinates, in input order.
pub fn spread(
    sizes: &[Size<f64, Logical>],
    centres: &[Point<f64, Logical>],
    area: Size<f64, Logical>,
    padding: f64,
    gap: f64,
) -> Vec<Rectangle<f64, Logical>> {
    let n = sizes.len();
    if n == 0 {
        return Vec::new();
    }
    let avail = Size::<f64, Logical>::from((
        (area.w - 2.0 * padding).max(1.0),
        (area.h - 2.0 * padding).max(1.0),
    ));
    // Top to bottom, then rows left to right.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| centres[a].y.total_cmp(&centres[b].y));

    let rows_for = |count: usize| -> Vec<Vec<usize>> {
        let mut rows = Vec::new();
        let mut rest = order.as_slice();
        for r in 0..count {
            let take = rest.len().div_ceil(count - r);
            let (row, tail) = rest.split_at(take);
            let mut row = row.to_vec();
            row.sort_by(|&a, &b| centres[a].x.total_cmp(&centres[b].x));
            rows.push(row);
            rest = tail;
        }
        rows
    };
    let fit = |rows: &[Vec<usize>]| -> f64 {
        let widest = rows
            .iter()
            .map(|row| row.iter().map(|&i| sizes[i].w).sum::<f64>() + gap * (row.len() - 1) as f64)
            .fold(0.0, f64::max);
        let height: f64 = rows
            .iter()
            .map(|row| row.iter().map(|&i| sizes[i].h).fold(0.0, f64::max))
            .sum();
        // Gaps don't shrink with the windows, so take them off first.
        let row_gaps = gap * (rows.len() - 1) as f64;
        let col_gaps = rows
            .iter()
            .map(|r| gap * (r.len() - 1) as f64)
            .fold(0.0, f64::max);
        let sx = (avail.w - col_gaps).max(1.0) / (widest - col_gaps).max(1.0);
        let sy = (avail.h - row_gaps).max(1.0) / height.max(1.0);
        sx.min(sy).min(1.0)
    };
    let (rows, scale) = (1..=n)
        .map(|count| {
            let rows = rows_for(count);
            let scale = fit(&rows);
            (rows, scale)
        })
        // Strictly better only: ties keep fewer rows.
        .fold(
            None,
            |best: Option<(Vec<Vec<usize>>, f64)>, (rows, scale)| match best {
                Some((_, best_scale)) if best_scale >= scale - 1e-9 => best,
                _ => Some((rows, scale)),
            },
        )
        .unwrap_or_default();

    let row_heights: Vec<f64> = rows
        .iter()
        .map(|row| row.iter().map(|&i| sizes[i].h * scale).fold(0.0, f64::max))
        .collect();
    let total_h = row_heights.iter().sum::<f64>() + gap * (rows.len() - 1) as f64;
    let mut out = vec![Rectangle::default(); n];
    let mut y = (area.h - total_h) / 2.0;
    for (row, row_h) in rows.iter().zip(&row_heights) {
        let row_w =
            row.iter().map(|&i| sizes[i].w * scale).sum::<f64>() + gap * (row.len() - 1) as f64;
        let mut x = (area.w - row_w) / 2.0;
        for &i in row {
            let size = sizes[i].upscale(scale);
            out[i] = Rectangle::new(Point::from((x, y + (row_h - size.h) / 2.0)), size);
            x += size.w + gap;
        }
        y += row_h + gap;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(w: f64, h: f64) -> Size<f64, Logical> {
        Size::from((w, h))
    }

    fn overlap(a: Rectangle<f64, Logical>, b: Rectangle<f64, Logical>) -> bool {
        a.loc.x < b.loc.x + b.size.w
            && b.loc.x < a.loc.x + a.size.w
            && a.loc.y < b.loc.y + b.size.h
            && b.loc.y < a.loc.y + a.size.h
    }

    #[test]
    fn one_small_window_keeps_its_size_and_is_centred() {
        let out = spread(
            &[size(400.0, 300.0)],
            &[Point::from((0.5, 0.5))],
            size(1700.0, 980.0),
            24.0,
            24.0,
        );
        assert_eq!(out[0].size, size(400.0, 300.0));
        assert_eq!(out[0].loc, Point::from((650.0, 340.0)));
    }

    #[test]
    fn overlapping_windows_spread_apart_and_fit() {
        // Four big windows stacked on top of each other.
        let sizes = vec![size(1600.0, 900.0); 4];
        let centres: Vec<_> = (0..4)
            .map(|i| Point::from((0.5 + i as f64 * 0.01, 0.5)))
            .collect();
        let area = size(1700.0, 980.0);
        let out = spread(&sizes, &centres, area, 24.0, 24.0);
        for (i, a) in out.iter().enumerate() {
            assert!(a.loc.x >= 0.0 && a.loc.y >= 0.0);
            assert!(a.loc.x + a.size.w <= area.w + 1e-6 && a.loc.y + a.size.h <= area.h + 1e-6);
            // Aspect ratio kept.
            assert!((a.size.w / a.size.h - 16.0 / 9.0).abs() < 1e-6);
            for b in &out[i + 1..] {
                assert!(!overlap(*a, *b), "{a:?} overlaps {b:?}");
            }
        }
        // 2x2 beats 1x4 or 4x1 for 16:9 windows in a 16:9 box.
        assert!(out[0].size.w > 600.0);
    }

    #[test]
    fn left_to_right_order_is_kept() {
        let sizes = vec![size(800.0, 600.0); 2];
        let centres = vec![Point::from((0.8, 0.5)), Point::from((0.2, 0.5))];
        let out = spread(&sizes, &centres, size(1700.0, 980.0), 24.0, 24.0);
        assert!(out[1].loc.x < out[0].loc.x, "the left window stays left");
    }
}
