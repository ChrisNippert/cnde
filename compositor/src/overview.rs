//! The overview (ARCHITECTURE.md §4.4, §5.2): tap Super and every monitor
//! zooms out of the workspace strip. Each workspace becomes a card on the
//! dimmed wallpaper, with its neighbours peeking in at the sides; click a
//! window to go to it, or an empty spot on a card to go to that workspace.
//!
//! It's the same strip, just drawn smaller: windows are laid out exactly as
//! in a workspace slide (`State::rest_rect`), then scaled about each
//! monitor's centre. So the strip can keep moving (Super+Alt+arrows, a
//! swipe, the scroll wheel) while the overview is open.

use std::{collections::HashMap, time::Instant};

use smithay::{
    backend::renderer::{
        Renderer,
        element::{
            Id, Kind,
            solid::{SolidColorBuffer, SolidColorRenderElement},
            texture::TextureRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Scale, Size, Transform},
};
use tracing::debug;

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
/// How dark the wallpaper behind the cards gets.
const DIM: f32 = 0.45;
/// Pointer this close to the top-left corner opens the overview.
const HOT_CORNER: f64 = 1.0;
/// ...and must leave this far before the corner works again.
const HOT_CORNER_REARM: f64 = 24.0;

pub struct Overview {
    /// Where it's heading (open or closed).
    open: bool,
    anim: Option<Animation>,
    dim: SolidColorBuffer,
    dim_alpha: f32,
    /// Stable element ids for workspace cards, so damage tracking can follow
    /// each card from frame to frame.
    card_ids: HashMap<WorkspaceId, Id>,
    hot_corner_armed: bool,
    /// Scroll wheel travel not yet turned into workspace steps (v120 units).
    scroll: f64,
    /// Had the keyboard before opening; gets it back unless a click picks
    /// another window.
    prev_focus: Option<Window>,
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
            hot_corner_armed: true,
            scroll: 0.0,
            prev_focus: None,
        }
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
        if open {
            // Typing must not reach the window behind the overview.
            self.overview.prev_focus = self.focused_window();
            if self.exclusive_layer().is_none()
                && let Some(keyboard) = self.seat.get_keyboard()
            {
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
        } else if self.focused_window().is_none() && self.exclusive_layer().is_none() {
            let previous = self
                .overview
                .prev_focus
                .take()
                .filter(|w| self.workspaces.list.iter().any(|ws| ws.windows.contains(w)));
            let top = previous.or_else(|| {
                self.workspaces.list[self.workspaces.active]
                    .windows
                    .last()
                    .cloned()
            });
            self.focus_window(top.as_ref());
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
        let centre = geo.loc + Point::from((geo.size.w / 2.0, geo.size.h / 2.0));
        let zoom_for = |slot: f64| Zoom {
            scale,
            offset: Point::from((
                centre.x * (1.0 - scale) + GAP * progress * (slot - k),
                centre.y * (1.0 - scale) + geo.size.h * SHIFT_DOWN * progress,
            )),
        };

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
            let (from, t) = (slot.floor() as isize, slot - slot.floor());
            let zoom = zoom_for(slot);
            let Some(card) = self
                .slot_areas(&outputs, from)
                .zip(self.slot_areas(&outputs, from + 1))
                .map(|((a, _), (b, _))| lerp_rect(a.to_f64(), b.to_f64(), t))
            else {
                continue;
            };
            let card = zoom.apply(card);
            if !card.overlaps(geo) {
                continue;
            }
            scene.cards.push(Card {
                workspace: workspace.id,
                index,
                rect: card,
            });
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
                scene.windows.push(Placed {
                    window: window.clone(),
                    rect: lerp_rect(a.to_f64(), b.to_f64(), t),
                    zoom,
                    workspace: index,
                });
            }
        }
        if let Some(window) = carried
            && let Some(rect) = self.space.element_geometry(&window)
        {
            let workspace = self.workspaces.motion.as_ref().map_or(0, |m| m.to);
            scene.windows.push(Placed {
                window,
                rect: rect.to_f64(),
                zoom: zoom_for(k),
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
        match window {
            Some(window) => self.focus_window(Some(&window)),
            None => {
                let top = self.workspaces.list[index].windows.last().cloned();
                self.focus_window(top.as_ref());
            }
        }
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
                self.toggle_overview();
            }
        } else if dx > HOT_CORNER_REARM || dy > HOT_CORNER_REARM {
            self.overview.hot_corner_armed = true;
        }
    }
}

/// The overview's backdrop for `output`, top first: workspace cards (each a
/// small copy of the wallpaper), then the dimming over the full wallpaper.
pub fn backdrop(
    overview: &mut Overview,
    wallpapers: &mut Wallpapers,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_geo: Rectangle<i32, Logical>,
    scene: Option<&Scene>,
) -> Vec<OutputElement> {
    let Some(scene) = scene.filter(|s| s.progress > 0.0) else {
        return Vec::new();
    };
    let scale = Scale::from(output.current_scale().fractional_scale());
    let mut elements = Vec::new();

    let live: Vec<WorkspaceId> = scene.cards.iter().map(|c| c.workspace).collect();
    overview.card_ids.retain(|id, _| live.contains(id));
    if let Some(texture) = wallpapers.texture(output) {
        for card in &scene.cards {
            let id = overview
                .card_ids
                .entry(card.workspace)
                .or_insert_with(Id::new)
                .clone();
            let local = Rectangle::new(card.rect.loc - output_geo.loc.to_f64(), card.rect.size);
            let size: Size<i32, Logical> = local.size.to_i32_round();
            elements.push(OutputElement::Wallpaper(
                TextureRenderElement::from_static_texture(
                    id,
                    renderer.context_id(),
                    local.loc.to_physical(scale),
                    texture.clone(),
                    1,
                    Transform::Normal,
                    None,
                    None,
                    Some(size),
                    None,
                    Kind::Unspecified,
                ),
            ));
        }
    }

    let alpha = DIM * scene.progress as f32;
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
    elements
}
