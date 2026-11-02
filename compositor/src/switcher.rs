//! The Alt+Tab window switcher: while the modifier is held, a panel of live
//! window thumbnails (most recently used first) on one monitor. Tab steps,
//! releasing the modifier picks. A quick tap never draws the panel.
//!
//! The thumbnails are the windows themselves, drawn scaled through the same
//! path the overview uses (`render::window_elements`); the panel behind them,
//! with the selection, app icons and titles, is one painted buffer.

use std::time::{Duration, Instant};

use smithay::utils::IsAlive;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::{SolidColorBuffer, SolidColorRenderElement},
                utils::RescaleRenderElement,
            },
            gles::GlesRenderer,
        },
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Point, Rectangle, Scale, Size, Transform},
};
use tracing::{debug, warn};

use cnde_ipc::Side;
use cnde_ui::{Canvas, Color, Fonts, Icons, Weight};

use crate::{
    animation::Curve,
    render::{OutputElement, Zoom},
    state::State,
    window::with_state,
};

/// A tap shorter than this switches without ever drawing the panel.
const SHOW_AFTER: Duration = Duration::from_millis(150);

/// The panel (and the dim behind it) fades in and out over this, and the
/// selection slides and windows rise and sink over the same time.
const FADE: Duration = Duration::from_millis(150);
/// The panel grows from this scale as it appears.
const PANEL_START_SCALE: f64 = 0.96;
/// How dark the rest of the screen goes behind the panel: all of it when
/// the scene can't be blurred, a touch over the blur when it can.
const DIM: f32 = 0.45;
const DIM_BLURRED: f32 = 0.22;
/// Resting on a dock preview's thumbnail this long peeks at its window:
/// the rest of the screen dims and blurs as under the switcher. Shorter
/// would flash while the pointer only passes over.
const PEEK_AFTER: Duration = Duration::from_millis(250);

const CELL_W: f64 = 216.0;
const CELL_H: f64 = 180.0;
const PAD: f64 = 14.0;
/// The thumbnail box inside a cell.
const THUMB_W: f64 = 196.0;
const THUMB_H: f64 = 124.0;
const THUMB_TOP: f64 = 10.0;
const LABEL_H: f64 = 28.0;
const ICON: f64 = 20.0;
/// Share of the monitor's width the panel may fill before it wraps.
const MAX_WIDTH: f64 = 0.9;

/// Which modifier opened the switcher; releasing it picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Alt,
    Logo,
}

struct Session {
    /// Most recently used first.
    windows: Vec<Window>,
    selected: usize,
    started: Instant,
    output: Output,
    /// `None` when driven by the debug IPC: nothing to release.
    held: Option<Held>,
    /// The selection highlight sliding to its cell.
    slide: Slide,
    /// The window that was selected before, sinking back under the dim,
    /// and when the current one started rising.
    sinking: Option<(Window, Instant)>,
    rising_since: Instant,
}

/// The selection highlight's slide between cells.
struct Slide {
    /// Where it was (relative to the panel) when the selection last moved.
    from: Option<Point<f64, Logical>>,
    since: Instant,
    /// Where it was last drawn.
    last: Option<Point<f64, Logical>>,
}

/// A closed switcher, fading out.
struct Leaving {
    session: Session,
    since: Instant,
    /// How far in it was when it closed.
    level: f64,
    /// The window left lit as the dim fades: the one picked, or the one
    /// that stays focused when cancelled.
    lit: Option<Window>,
}

/// 0 to 1 over `FADE` from `since`, eased.
fn eased(since: Instant, now: Instant) -> f64 {
    Curve::EaseOutCubic
        .apply(now.saturating_duration_since(since).as_secs_f64() / FADE.as_secs_f64())
}

/// The dock's hover preview: the same panel, beside a dock icon, with the
/// thumbnail under the pointer lit and a close button on it.
struct Preview {
    windows: Vec<Window>,
    output: Output,
    /// The dock icon (and the dock's thickness), relative to the output.
    anchor: Rectangle<f64, Logical>,
    side: Side,
    hover: Option<usize>,
    /// The pointer's over the hovered cell's close button.
    close_hot: bool,
    /// The pointer's on the panel.
    inside: bool,
    /// When the panel first appeared, for fading in.
    since: Instant,
    /// Opened by a click on the dock icon: it stays until a pick, Escape
    /// or a click elsewhere, wherever the pointer goes.
    pinned: bool,
    peek: Peek,
}

/// The hovered thumbnail's window shown on its own, everything else under
/// the dim and blur, fading in and out from wherever it had got to.
#[derive(Default)]
struct Peek {
    window: Option<Window>,
    on: bool,
    from: f64,
    since: Option<Instant>,
}

impl Peek {
    fn level(&self, now: Instant) -> f64 {
        let Some(since) = self.since else {
            return 0.0;
        };
        let e = eased(since, now);
        if self.on {
            self.from + (1.0 - self.from) * e
        } else {
            self.from * (1.0 - e)
        }
    }

    /// The pointer rests on `window`'s thumbnail, or (`None`) left them.
    fn set(&mut self, window: Option<Window>, now: Instant) {
        let level = self.level(now);
        match window {
            Some(window) => {
                if !self.on {
                    self.on = true;
                    self.from = level;
                    // Already up (moving between thumbnails): no wait.
                    self.since = Some(if level > 0.0 { now } else { now + PEEK_AFTER });
                }
                self.window = Some(window);
            }
            None if self.on => {
                self.on = false;
                self.from = level;
                self.since = Some(now);
            }
            None => {}
        }
    }

    fn busy(&self, now: Instant) -> bool {
        self.since
            .is_some_and(|since| now < since + FADE + Duration::from_millis(20))
    }
}

#[derive(Default)]
pub struct Switcher {
    session: Option<Session>,
    preview: Option<Preview>,
    fonts: Option<Fonts>,
    icons: Option<Icons>,
    panel: Option<(Key, MemoryRenderBuffer)>,
    /// The closed switcher, fading out.
    leaving: Option<Leaving>,
    /// Darkens everything behind the panel.
    dim: Option<SolidColorBuffer>,
    /// The scene behind it, blurred when it opened.
    blur: crate::scene_blur::SceneBlur,
    /// The selection highlight: one cell, drawn on its own so it can slide.
    highlight: Option<(u64, MemoryRenderBuffer)>,
    /// A dock preview cell's close button, drawn on its own over the
    /// thumbnails (a window reaching the cell's top would hide it), and
    /// whether it's lit.
    close: Option<((u64, bool), MemoryRenderBuffer)>,
}

/// How the panel is drawn this frame.
struct Look {
    /// 0 (gone) to 1 (up): its opacity, and how far it has grown.
    level: f64,
    /// The selection highlight's cell position within the panel; `None` has
    /// the panel paint it itself (the dock's hover).
    highlight: Option<Point<f64, Logical>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    /// Window id, title and app id of each cell.
    cells: Vec<(u64, String, String)>,
    selected: Option<usize>,
    /// The selection is painted into the panel (else it's drawn apart).
    lit: bool,
    cols: usize,
    scale: u64,
    size: (i32, i32),
    appearance: cnde_config::Appearance,
    /// Frosted: the tint the panel takes over its frost, and its corners.
    frosted: Option<(Color, u64)>,
}

/// Where everything sits: cells relative to the panel's top-left, the panel
/// relative to the output's.
struct Layout {
    panel: Rectangle<f64, Logical>,
    cols: usize,
    cells: Vec<Rectangle<f64, Logical>>,
}

/// Where the panel goes.
#[derive(Clone, Copy)]
enum Place {
    /// Middle of the output.
    Centre,
    /// Beside `anchor` (relative to the output), on `side` of it.
    Beside(Rectangle<f64, Logical>, Side),
}

/// Space between a dock and its preview.
const ANCHOR_GAP: f64 = 8.0;
/// Closest a preview comes to the output's edge.
const EDGE_GAP: f64 = 6.0;
const CLOSE: f64 = 22.0;

fn layout(count: usize, output: Size<f64, Logical>, place: Place) -> Layout {
    let fit = ((output.w * MAX_WIDTH - 2.0 * PAD) / CELL_W)
        .floor()
        .max(1.0) as usize;
    let cols = count.clamp(1, fit);
    let rows = count.max(1).div_ceil(cols);
    let size = Size::from((
        cols as f64 * CELL_W + 2.0 * PAD,
        rows as f64 * CELL_H + 2.0 * PAD,
    ));
    let centre = Point::from((
        ((output.w - size.w) / 2.0).round(),
        ((output.h - size.h) / 2.0).round(),
    ));
    let loc = match place {
        Place::Centre => centre,
        Place::Beside(a, side) => {
            let across = (a.loc.x + (a.size.w - size.w) / 2.0).round();
            let along = (a.loc.y + (a.size.h - size.h) / 2.0).round();
            let (x, y) = match side {
                Side::Above => (across, a.loc.y - ANCHOR_GAP - size.h),
                Side::Below => (across, a.loc.y + a.size.h + ANCHOR_GAP),
                Side::Left => (a.loc.x - ANCHOR_GAP - size.w, along),
                Side::Right => (a.loc.x + a.size.w + ANCHOR_GAP, along),
            };
            Point::from((
                x.clamp(EDGE_GAP, (output.w - size.w - EDGE_GAP).max(EDGE_GAP)),
                y.clamp(EDGE_GAP, (output.h - size.h - EDGE_GAP).max(EDGE_GAP)),
            ))
        }
    };
    let panel = Rectangle::new(loc, size);
    let cells = grid(count, cols);
    Layout { panel, cols, cells }
}

/// Cell rects relative to the panel, `cols` to a row.
fn grid(count: usize, cols: usize) -> Vec<Rectangle<f64, Logical>> {
    (0..count)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            // A short last row is centred.
            let in_row = (count - row * cols).min(cols);
            let shift = (cols - in_row) as f64 * CELL_W / 2.0;
            Rectangle::new(
                Point::from((PAD + shift + col as f64 * CELL_W, PAD + row as f64 * CELL_H)),
                Size::from((CELL_W, CELL_H)),
            )
        })
        .collect()
}

/// A cell's close button (dock previews), relative to the panel.
fn close_rect(cell: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((
            cell.loc.x + CELL_W - CLOSE - 10.0,
            cell.loc.y + THUMB_TOP + 6.0,
        )),
        Size::from((CLOSE, CLOSE)),
    )
}

/// How far in a open switcher is: 0 until the tap delay has passed, then
/// fading in.
fn open_level(session: &Session, now: Instant) -> f64 {
    if now < session.started + SHOW_AFTER {
        return 0.0;
    }
    eased(session.started + SHOW_AFTER, now)
}

impl Switcher {
    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// Whether the panel is up: it's been held long enough not to be a tap.
    pub fn is_visible(&self, now: Instant) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| now.saturating_duration_since(s.started) >= SHOW_AFTER)
    }

    pub fn held(&self) -> Option<Held> {
        self.session.as_ref().and_then(|s| s.held)
    }

    /// Whether a frame is due soon: the panel is about to appear or is
    /// fading, or the selection is moving.
    pub fn is_animating(&self, now: Instant) -> bool {
        let busy = |since: Instant| {
            now.saturating_duration_since(since) < FADE + Duration::from_millis(20)
        };
        self.session.as_ref().is_some_and(|s| {
            busy(s.started + SHOW_AFTER - Duration::from_millis(20)) || busy(s.rising_since)
        }) || self.leaving.as_ref().is_some_and(|l| busy(l.since))
            || self
                .preview
                .as_ref()
                .is_some_and(|p| busy(p.since) || p.peek.busy(now))
    }

    fn step(&mut self, back: bool) {
        if let Some(s) = &mut self.session {
            let n = s.windows.len();
            let now = Instant::now();
            s.slide = Slide {
                from: s.slide.last,
                since: now,
                last: s.slide.last,
            };
            s.sinking = s.windows.get(s.selected).map(|w| (w.clone(), now));
            s.rising_since = now;
            s.selected = if back {
                (s.selected + n - 1) % n
            } else {
                (s.selected + 1) % n
            };
        }
    }

    /// The switcher closes: its panel and dim fade out (unless it never
    /// got as far as showing).
    fn close(&mut self, now: Instant, lit: Option<Window>) {
        let Some(session) = self.session.take() else {
            return;
        };
        let level = open_level(&session, now);
        if level > 0.0 {
            self.leaving = Some(Leaving {
                session,
                since: now,
                level,
                lit,
            });
        }
    }

    /// The panel and thumbnails for `output`, front first; empty unless the
    /// switcher (or the dock's preview) is showing there. The switcher also
    /// dims the screen, with the selected window lifted back out of the dim.
    ///
    /// `scene` is what's drawn on the output below the switcher (top
    /// first), blurred once for its backdrop; `frost` is the frost to put
    /// under the panel (`None`: solid).
    #[allow(clippy::too_many_arguments)]
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        decorations: &mut crate::decorations::Decorations,
        space: &Space<Window>,
        output: &Output,
        scene: &[OutputElement],
        mut frost: Option<crate::frost::WindowFrost<'_>>,
        now: Instant,
    ) -> Vec<OutputElement> {
        let started = Instant::now();
        if self.leaving.as_ref().is_some_and(|l| {
            now.saturating_duration_since(l.since) >= FADE || self.session.is_some()
        }) {
            self.leaving = None;
        }
        let Some(size) = space.output_geometry(output).map(|g| g.size.to_f64()) else {
            return Vec::new();
        };
        let open = self.session.is_some();
        let fading = self.leaving.is_some() && !open;
        let peeking = self
            .preview
            .as_ref()
            .is_some_and(|p| p.peek.level(now) > 0.0);
        if !open && self.leaving.is_none() && !peeking {
            self.blur.release();
        }
        if open || fading {
            let (session, level, live, kept) = match (&self.session, &self.leaving) {
                (Some(s), _) => (s, open_level(s, now), true, None),
                (None, Some(l)) => (
                    &l.session,
                    l.level * (1.0 - eased(l.since, now)),
                    false,
                    l.lit.clone(),
                ),
                _ => return Vec::new(),
            };
            if level <= 0.0 {
                return Vec::new();
            }
            // The panel lives on one output; the others just dim and blur.
            let here = &session.output == output;
            if frost.is_some() && !self.blur.has(output) {
                self.blur.capture(renderer, output, scene);
            }
            let windows = session.windows.clone();
            let selected = session.selected;
            let geometry = layout(windows.len(), size, Place::Centre);
            // The highlight slides from where it was to its cell.
            let target = geometry.cells.get(selected).map(|c| c.loc);
            let at = target.map(|target| match session.slide.from.filter(|_| live) {
                Some(from) => {
                    let e = eased(session.slide.since, now);
                    Point::from((
                        from.x + (target.x - from.x) * e,
                        from.y + (target.y - from.y) * e,
                    ))
                }
                None => target,
            });
            // The selected window rises out of the dim as the last one sinks
            // back into it (when they're on screen to do so).
            let rising = (
                if live {
                    windows.get(selected).cloned()
                } else {
                    kept
                },
                if live {
                    eased(session.rising_since, now)
                } else {
                    1.0
                },
            );
            let sinking = session
                .sinking
                .clone()
                .filter(|_| live)
                .map(|(w, since)| (w, 1.0 - eased(since, now)));
            if let (true, Some(s), Some(at)) = (here, &mut self.session, at) {
                s.slide.last = Some(at);
            }
            let look = Look {
                level,
                highlight: at,
            };
            let mut elements = if here {
                self.panel_elements(
                    renderer,
                    decorations,
                    space,
                    output,
                    &windows,
                    &geometry,
                    Some(selected),
                    None,
                    &look,
                    &mut frost,
                    true,
                    now,
                )
            } else {
                Vec::new()
            };
            let scale = Scale::from(output.current_scale().fractional_scale());
            let output_loc = space
                .output_geometry(output)
                .map_or_else(Default::default, |g| g.loc.to_f64());
            let lit = rising.0.map(|w| (w, rising.1)).into_iter().chain(sinking);
            for (window, alpha) in lit {
                // Only a window on screen here has a place to light up.
                let Some(rect) = space
                    .elements_for_output(output)
                    .find(|w| **w == window)
                    .and_then(|w| space.element_geometry(w))
                else {
                    continue;
                };
                elements.extend(crate::render::window_elements(
                    renderer,
                    decorations,
                    None,
                    None,
                    &window,
                    rect.to_f64(),
                    Zoom::IDENTITY,
                    output_loc,
                    scale,
                    now,
                    alpha as f32,
                ));
            }
            let dim = self.dim.get_or_insert_with(|| {
                SolidColorBuffer::new(Size::from((1, 1)), [0.0, 0.0, 0.0, 1.0])
            });
            let size = size.to_i32_round::<i32>();
            let size = Size::from((size.w.max(1), size.h.max(1)));
            dim.resize(size);
            let blurred = self.blur.element(output, level as f32);
            elements.push(OutputElement::Solid(SolidColorRenderElement::from_buffer(
                dim,
                (0, 0),
                scale,
                if blurred.is_some() { DIM_BLURRED } else { DIM } * level as f32,
                Kind::Unspecified,
            )));
            elements.extend(blurred.map(OutputElement::Wallpaper));
            debug!(
                ms = started.elapsed().as_secs_f64() * 1000.0,
                blurred = self.blur.has(output),
                "switcher frame built (cpu side)"
            );
            return elements;
        }
        let Some(preview) = self.preview.as_ref().filter(|p| &p.output == output) else {
            return Vec::new();
        };
        let windows: Vec<Window> = preview
            .windows
            .iter()
            .filter(|w| w.alive())
            .cloned()
            .collect();
        if windows.is_empty() {
            return Vec::new();
        }
        let geometry = layout(
            windows.len(),
            size,
            Place::Beside(preview.anchor, preview.side),
        );
        let (hover, close_hot) = (preview.hover, preview.close_hot);
        let look = Look {
            level: eased(preview.since, now),
            highlight: None,
        };
        let peek = preview.peek.level(now) * look.level;
        let peeked = preview.peek.window.clone().filter(|w| w.alive());
        if peek > 0.0 && frost.is_some() && !self.blur.has(output) {
            self.blur.capture(renderer, output, scene);
        }
        let mut elements = self.panel_elements(
            renderer,
            decorations,
            space,
            output,
            &windows,
            &geometry,
            hover,
            Some(close_hot),
            &look,
            &mut frost,
            false,
            now,
        );
        if peek <= 0.0 {
            return elements;
        }
        // Peeking: the window lit over the dim, as the switcher does it.
        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_loc = space
            .output_geometry(output)
            .map_or_else(Default::default, |g| g.loc.to_f64());
        if let Some(window) = peeked
            && let Some(rect) = space
                .elements_for_output(output)
                .find(|w| **w == window)
                .and_then(|w| space.element_geometry(w))
        {
            elements.extend(crate::render::window_elements(
                renderer,
                decorations,
                None,
                None,
                &window,
                rect.to_f64(),
                Zoom::IDENTITY,
                output_loc,
                scale,
                now,
                1.0,
            ));
        }
        let dim = self.dim.get_or_insert_with(|| {
            SolidColorBuffer::new(Size::from((1, 1)), [0.0, 0.0, 0.0, 1.0])
        });
        let size = size.to_i32_round::<i32>();
        dim.resize(Size::from((size.w.max(1), size.h.max(1))));
        let blurred = self.blur.element(output, peek as f32);
        elements.push(OutputElement::Solid(SolidColorRenderElement::from_buffer(
            dim,
            (0, 0),
            scale,
            if blurred.is_some() { DIM_BLURRED } else { DIM } * peek as f32,
            Kind::Unspecified,
        )));
        elements.extend(blurred.map(OutputElement::Wallpaper));
        elements
    }

    /// The panel `geometry` with a thumbnail of each of `windows`: shared by
    /// the switcher and the dock's preview. `closable`: cells get a close
    /// button, lit or not.
    #[allow(clippy::too_many_arguments)]
    fn panel_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        decorations: &mut crate::decorations::Decorations,
        space: &Space<Window>,
        output: &Output,
        windows: &[Window],
        geometry: &Layout,
        selected: Option<usize>,
        closable: Option<bool>,
        look: &Look,
        frost: &mut Option<crate::frost::WindowFrost<'_>>,
        backdrop: bool,
        now: Instant,
    ) -> Vec<OutputElement> {
        let Some(output_geo) = space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_loc = output_geo.loc.to_f64();
        // The panel grows from its middle as it fades in.
        let grow = PANEL_START_SCALE + (1.0 - PANEL_START_SCALE) * look.level;
        let alpha = look.level as f32;
        let middle = Point::from((
            output_loc.x + geometry.panel.loc.x + geometry.panel.size.w / 2.0,
            output_loc.y + geometry.panel.loc.y + geometry.panel.size.h / 2.0,
        ));
        let about_middle = |rect: Rectangle<f64, Logical>| {
            Rectangle::new(
                Point::from((
                    middle.x + (rect.loc.x - middle.x) * grow,
                    middle.y + (rect.loc.y - middle.y) * grow,
                )),
                Size::from((rect.size.w * grow, rect.size.h * grow)),
            )
        };

        let mut elements = Vec::new();
        for (window, cell) in windows.iter().zip(&geometry.cells) {
            let geo = window.geometry();
            if geo.is_empty() {
                continue;
            }
            let bar = crate::decorations::titlebar_height(window) as f64;
            let (w, h) = (geo.size.w as f64, geo.size.h as f64 + bar);
            let factor = (THUMB_W / w).min(THUMB_H / h).min(1.0);
            let (tw, th) = (w * factor, h * factor);
            // Where the geometry goes; the title bar rides above it.
            let target = Rectangle::new(
                Point::from((
                    output_loc.x + geometry.panel.loc.x + cell.loc.x + (CELL_W - tw) / 2.0,
                    output_loc.y
                        + geometry.panel.loc.y
                        + cell.loc.y
                        + THUMB_TOP
                        + (THUMB_H - th) / 2.0
                        + bar * factor,
                )),
                Size::from((tw, geo.size.h as f64 * factor)),
            );
            let target = about_middle(target);
            let live = Rectangle::new(output_loc, Size::from((w, geo.size.h as f64)));
            // Drawing records where the window was, for close animations;
            // this isn't where it is.
            let recorded = with_state(window, |s| s.last_frame.as_ref().map(|f| f.rect));
            elements.extend(crate::render::window_elements(
                renderer,
                decorations,
                None,
                None,
                window,
                live,
                Zoom::mapping(live, target),
                output_loc,
                scale,
                now,
                alpha,
            ));
            if let Some(rect) = recorded {
                with_state(window, |s| {
                    if let Some(frame) = &mut s.last_frame {
                        frame.rect = rect;
                    }
                });
            }
        }

        let key = Key {
            cells: windows
                .iter()
                .map(|w| {
                    let (title, app_id) = crate::capture::title_and_app_id(w);
                    (with_state(w, |s| s.id), title, app_id)
                })
                .collect(),
            selected,
            lit: look.highlight.is_none(),
            cols: geometry.cols,
            scale: scale.x.to_bits(),
            size: (geometry.panel.size.w as i32, geometry.panel.size.h as i32),
            appearance: decorations.appearance.clone(),
            frosted: frost
                .as_ref()
                .map(|f| (f.panel, f.panel_radius.to_bits())),
        };
        if self.panel.as_ref().is_none_or(|(k, _)| *k != key) {
            let Some(buffer) = self.paint(&key) else {
                return elements;
            };
            self.panel = Some((key, buffer));
        }
        let scale_up = Scale::from(grow);
        let around = (middle - output_loc).to_physical(scale).to_i32_round();
        if let Some(at) = look.highlight {
            let buffer = self.highlight_buffer(scale.x, &decorations.appearance);
            let rect = Rectangle::new(
                Point::from((geometry.panel.loc.x + at.x, geometry.panel.loc.y + at.y)),
                Size::from((CELL_W, CELL_H - 6.0)),
            );
            let location = rect.loc.to_physical(scale);
            let pixels = (
                (CELL_W * scale.x).round(),
                ((CELL_H - 6.0) * scale.x).round(),
            );
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                &buffer,
                Some(alpha),
                Some(Rectangle::from_size(Size::from(pixels))),
                Some(Size::from((CELL_W as i32, CELL_H as i32 - 6))),
                Kind::Unspecified,
            ) {
                Ok(element) => elements.push(OutputElement::Titlebar(
                    RescaleRenderElement::from_element(element, around, scale_up),
                )),
                Err(err) => warn!(?err, "can't upload the window switcher"),
            }
        }
        // In front of all: the hovered cell's close button.
        if let (Some(hot), Some(cell)) = (closable, selected.and_then(|i| geometry.cells.get(i)))
        {
            let rect = close_rect(*cell);
            let buffer = self.close_buffer(scale.x, hot, &decorations.appearance);
            let location =
                Point::<f64, Logical>::from((geometry.panel.loc.x + rect.loc.x, geometry.panel.loc.y + rect.loc.y))
                    .to_physical(scale);
            let pixels = (CLOSE * scale.x).round();
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                &buffer,
                Some(alpha),
                Some(Rectangle::from_size(Size::from((pixels, pixels)))),
                Some(Size::from((CLOSE as i32, CLOSE as i32))),
                Kind::Unspecified,
            ) {
                Ok(element) => elements.insert(
                    0,
                    OutputElement::Titlebar(RescaleRenderElement::from_element(
                        element, around, scale_up,
                    )),
                ),
                Err(err) => warn!(?err, "can't upload the window switcher"),
            }
        }
        if let Some((key, buffer)) = &self.panel {
            let location = geometry.panel.loc.to_physical(scale);
            let (w, h) = key.size;
            let pixels = ((w as f64 * scale.x).round(), (h as f64 * scale.x).round());
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                buffer,
                Some(alpha),
                Some(Rectangle::from_size(Size::from(pixels))),
                Some(Size::from((w, h))),
                Kind::Unspecified,
            ) {
                Ok(element) => elements.push(OutputElement::Titlebar(
                    RescaleRenderElement::from_element(element, around, scale_up),
                )),
                Err(err) => warn!(?err, "can't upload the window switcher"),
            }
        }
        // Behind the panel: the frost it's tinted over. The switcher's
        // has the blurred scene to cut it from; a dock preview, the
        // wallpaper, or a solid backing over a window.
        if let Some(frost) = frost {
            let panel = geometry.panel;
            let rect = about_middle(Rectangle::new(panel.loc + output_loc, panel.size));
            let rect = Rectangle::new(rect.loc - output_loc, rect.size);
            let (source, under) = if backdrop {
                (self.blur.crop(output, rect.to_i32_round(), alpha), None)
            } else {
                let behind = crate::frost::Behind::new(space, output, None);
                (None, behind.under(rect.to_i32_round()).cloned())
            };
            frost.solid = frost.panel;
            let radius = frost.panel_radius;
            elements.extend(frost.elements_from(
                renderer,
                if backdrop { "switcher" } else { "preview" },
                rect,
                radius,
                alpha,
                under.as_ref(),
                source,
            ));
        }
        elements
    }

    /// One cell's selection highlight, painted once per scale and style.
    fn highlight_buffer(
        &mut self,
        scale: f64,
        appearance: &cnde_config::Appearance,
    ) -> MemoryRenderBuffer {
        let key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            scale.to_bits().hash(&mut hasher);
            format!("{appearance:?}").hash(&mut hasher);
            hasher.finish()
        };
        if let Some((k, buffer)) = &self.highlight
            && *k == key
        {
            return buffer.clone();
        }
        let theme = cnde_ui::Theme::for_appearance(appearance);
        let (cw, ch) = (CELL_W as f32, CELL_H as f32 - 6.0);
        let (w, h) = (
            (cw as f64 * scale).round() as u32,
            (ch as f64 * scale).round() as u32,
        );
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        if let Some(mut canvas) = Canvas::new(&mut pixels, w, h, scale as f32) {
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            let r = theme.window_radius.max(8.0);
            canvas.rounded_rect(
                1.0,
                1.0,
                cw - 2.0,
                ch - 2.0,
                r,
                theme.accent.with_alpha(0.28),
            );
            canvas.stroke_rounded_rect(1.0, 1.0, cw - 2.0, ch - 2.0, r, 2.0, theme.accent);
        }
        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (w as i32, h as i32),
            1,
            Transform::Normal,
            None,
        );
        self.highlight = Some((key, buffer.clone()));
        buffer
    }

    /// A cell's close button, painted once per scale, style and lit-ness.
    fn close_buffer(
        &mut self,
        scale: f64,
        hot: bool,
        appearance: &cnde_config::Appearance,
    ) -> MemoryRenderBuffer {
        let key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            scale.to_bits().hash(&mut hasher);
            format!("{appearance:?}").hash(&mut hasher);
            (hasher.finish(), hot)
        };
        if let Some((k, buffer)) = &self.close
            && *k == key
        {
            return buffer.clone();
        }
        let theme = cnde_ui::Theme::for_appearance(appearance);
        let size = (CLOSE * scale).round() as u32;
        let mut pixels = vec![0u8; (size * size * 4) as usize];
        let icons = self.icons.get_or_insert_with(Icons::new);
        icons.set_theme(&appearance.icon_theme);
        if let Some(mut canvas) = Canvas::new(&mut pixels, size, size, scale as f32) {
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            let r = (CLOSE / 2.0) as f32;
            let fill = if hot {
                theme.accent
            } else {
                // Over a thumbnail now, so solid rather than a tint.
                theme.window_background.mix(theme.window_text, 0.18)
            };
            canvas.circle(r, r, r, fill);
            let ink = if hot {
                theme.on_accent
            } else {
                theme.window_text
            };
            canvas.icon(icons, "window-close-symbolic", r * 0.5, r * 0.5, r, ink);
        }
        let buffer = MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (size as i32, size as i32),
            1,
            Transform::Normal,
            None,
        );
        self.close = Some((key, buffer.clone()));
        buffer
    }

    fn paint(&mut self, key: &Key) -> Option<MemoryRenderBuffer> {
        let scale = f64::from_bits(key.scale);
        let (w, h) = (
            (key.size.0 as f64 * scale).round() as u32,
            (key.size.1 as f64 * scale).round() as u32,
        );
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let fonts = self.fonts.get_or_insert_with(|| {
                Fonts::new(&[
                    "Adwaita Sans",
                    "Cantarell",
                    "Inter",
                    "Noto Sans",
                    "DejaVu Sans",
                ])
            });
            let icons = self.icons.get_or_insert_with(Icons::new);
            icons.set_theme(&key.appearance.icon_theme);
            let theme = cnde_ui::Theme::for_appearance(&key.appearance);
            let mut canvas = Canvas::new(&mut pixels, w, h, scale as f32)?;
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            let (pw, ph) = (key.size.0 as f32, key.size.1 as f32);
            let (background, radius) = match key.frosted {
                Some((tint, radius)) => (tint, f64::from_bits(radius) as f32),
                None => (
                    theme.window_background.with_alpha(0.93),
                    theme.window_radius.max(10.0) + 6.0,
                ),
            };
            canvas.rounded_rect(0.0, 0.0, pw, ph, radius, background);
            canvas.stroke_rounded_rect(
                0.0,
                0.0,
                pw,
                ph,
                radius,
                1.0,
                theme.window_text.with_alpha(0.12),
            );
            let cells = grid(key.cells.len(), key.cols);
            for (i, ((_, title, app_id), cell)) in key.cells.iter().zip(&cells).enumerate() {
                let (x, y) = (cell.loc.x as f32, cell.loc.y as f32);
                let (cw, ch) = (CELL_W as f32, CELL_H as f32 - 6.0);
                if key.selected == Some(i) && key.lit {
                    let r = theme.window_radius.max(8.0);
                    canvas.rounded_rect(x, y, cw, ch, r, theme.accent.with_alpha(0.28));
                    canvas.stroke_rounded_rect(x, y, cw, ch, r, 2.0, theme.accent);
                } else {
                    canvas.rounded_rect(
                        x + 6.0,
                        y + THUMB_TOP as f32,
                        cw - 12.0,
                        THUMB_H as f32,
                        8.0,
                        theme.window_text.with_alpha(0.05),
                    );
                }
                let label_y = y + (THUMB_TOP + THUMB_H + 10.0) as f32;
                let mut text_x = x + 14.0;
                let icon_y = label_y + (LABEL_H - ICON) as f32 / 2.0;
                if !app_id.is_empty() && canvas.app_icon(icons, app_id, text_x, icon_y, ICON as f32)
                {
                    text_x += (ICON + 8.0) as f32;
                }
                if !title.is_empty() {
                    let mut layout = fonts.layout(title, 13.0, Weight::Bold, canvas.scale());
                    let room = x + cw - 12.0 - text_x;
                    let ty = (label_y + (LABEL_H as f32 - layout.height) / 2.0).round();
                    canvas.text_clipped(
                        fonts,
                        &mut layout,
                        text_x,
                        ty,
                        theme.window_text,
                        (text_x, label_y, room, LABEL_H as f32),
                    );
                }
            }
        }
        Some(MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (w as i32, h as i32),
            1,
            Transform::Normal,
            None,
        ))
    }
}

impl State {
    /// Tab (or Shift+Tab) pressed: open the switcher on the previous window,
    /// or step through the open one.
    pub fn switcher_step(&mut self, back: bool, held: Option<Held>) {
        if self.switcher.is_open() {
            self.switcher.step(back);
            self.queue_redraw();
            return;
        }
        if self.overview.is_open() || self.screenshot.active || self.lock.is_locked() {
            return;
        }
        let windows = self.switcher_windows();
        if windows.is_empty() {
            return;
        }
        let output = self
            .focused_window()
            .and_then(|w| self.output_for(&w))
            .or_else(|| self.output_under_pointer());
        let Some(output) = output else {
            return;
        };
        self.dock_preview_hide();
        // The window we're on is first; one step goes to the one before it.
        let n = windows.len();
        let selected = if n == 1 {
            0
        } else if back {
            n - 1
        } else {
            1
        };
        let now = Instant::now();
        self.switcher.leaving = None;
        self.switcher.session = Some(Session {
            windows,
            selected,
            started: now,
            output,
            held,
            slide: Slide {
                from: None,
                since: now,
                last: None,
            },
            sinking: None,
            rising_since: now + SHOW_AFTER,
        });
        // Draw the panel once the tap window has passed.
        let _ = self.loop_handle.insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(
                SHOW_AFTER + Duration::from_millis(5),
            ),
            |_, _, state| {
                state.queue_redraw();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        );
        self.queue_redraw();
    }

    /// The windows to switch between, most recently used first.
    fn switcher_windows(&self) -> Vec<Window> {
        let (outputs, _) = self.strip_outputs();
        let mut candidates: Vec<Window> = Vec::new();
        for output in &outputs {
            let shown = self
                .workspace_on(output)
                .and_then(|i| self.workspaces.list.get(i));
            for w in shown.into_iter().flat_map(|ws| &ws.windows) {
                if w.alive() && !candidates.contains(w) {
                    candidates.push(w.clone());
                }
            }
        }
        let mut ordered: Vec<Window> = self
            .mru
            .iter()
            .filter(|w| candidates.contains(w))
            .cloned()
            .collect();
        let rest: Vec<Window> = candidates
            .into_iter()
            .filter(|w| !ordered.contains(w))
            .collect();
        ordered.extend(rest);
        ordered
    }

    /// Modifier released (or Return from the debug IPC): focus the selection.
    pub fn switcher_commit(&mut self) {
        let Some(session) = &self.switcher.session else {
            return;
        };
        let window = session.windows.get(session.selected).cloned();
        self.switcher.close(Instant::now(), window.clone());
        self.queue_redraw();
        if let Some(window) = window.as_ref().filter(|w| w.alive()) {
            let id = with_state(window, |s| s.id);
            tracing::debug!(id, title = %crate::capture::title_and_app_id(window).0, "window switcher picked");
            self.activate_window(id);
        }
    }

    pub fn switcher_cancel(&mut self) {
        if self.switcher.is_open() {
            let focused = self.focused_window();
            self.switcher.close(Instant::now(), focused);
            self.queue_redraw();
        }
    }

    /// A click while the switcher is open: pick the thumbnail under it, if
    /// any. Returns whether the click was the switcher's.
    pub fn switcher_click(&mut self, pos: Point<f64, Logical>) -> bool {
        if !self.switcher.is_open() {
            return false;
        }
        if !self.switcher.is_visible(Instant::now()) {
            return true;
        }
        let picked = self.switcher.session.as_ref().and_then(|session| {
            let geo = self.space.output_geometry(&session.output)?;
            let geometry = layout(session.windows.len(), geo.size.to_f64(), Place::Centre);
            let local = pos - geo.loc.to_f64() - geometry.panel.loc;
            geometry.cells.iter().position(|c| c.contains(local))
        });
        if let Some(i) = picked {
            if let Some(s) = &mut self.switcher.session {
                s.selected = i;
            }
            self.switcher_commit();
        }
        true
    }
}

/// What the pointer's over on the dock's preview panel.
struct PreviewHit {
    inside: bool,
    cell: Option<usize>,
    close: bool,
}

impl State {
    /// The dock asked for a preview of `ids` beside the icon at `cell` (in
    /// the dock surface's coordinates) on `output_name`.
    pub fn dock_preview(&mut self, output_name: &str, ids: &[u64], cell: [f64; 4], side: Side) {
        if self.overview.is_open()
            || self.switcher.is_open()
            || self.screenshot.active
            || self.lock.is_locked()
        {
            return self.dock_preview_hide();
        }
        let Some(output) = self
            .space
            .outputs()
            .find(|o| o.name() == output_name)
            .cloned()
        else {
            return;
        };
        let mut windows: Vec<Window> = ids
            .iter()
            .filter_map(|id| {
                self.workspaces
                    .list
                    .iter()
                    .flat_map(|ws| &ws.windows)
                    .find(|w| with_state(w, |s| s.id) == *id)
                    .cloned()
            })
            .collect();
        // Left to right as they sit across the monitors, so the thumbnails
        // match the screen and don't swap as focus moves; ones off screen
        // (minimized, other workspaces) last.
        windows.sort_by_key(|w| {
            self.space
                .element_geometry(w)
                .map_or((1, 0, 0), |g| (0, g.loc.x, g.loc.y))
        });
        let area = {
            let map = smithay::desktop::layer_map_for_output(&output);
            map.layers()
                .find(|l| l.namespace() == cnde_ipc::dock::NAMESPACE)
                .and_then(|l| map.layer_geometry(l))
        };
        let Some(area) = area.filter(|_| !windows.is_empty()) else {
            return self.dock_preview_hide();
        };
        let area = area.to_f64();
        let cell: Rectangle<f64, Logical> = Rectangle::new(
            Point::from((cell[0], cell[1])),
            Size::from((cell[2], cell[3])),
        );
        // The icon along the dock, the dock's whole thickness across it.
        let anchor = match side {
            Side::Above | Side::Below => Rectangle::new(
                Point::from((area.loc.x + cell.loc.x, area.loc.y)),
                Size::from((cell.size.w, area.size.h)),
            ),
            Side::Left | Side::Right => Rectangle::new(
                Point::from((area.loc.x, area.loc.y + cell.loc.y)),
                Size::from((area.size.w, cell.size.h)),
            ),
        };
        if let Some(p) = &self.switcher.preview
            && p.output == output
            && p.windows == windows
            && p.anchor == anchor
            && p.side == side
        {
            return;
        }
        let inside = self.switcher.preview.as_ref().is_some_and(|p| p.inside);
        let pinned = self.switcher.preview.as_ref().is_some_and(|p| p.pinned);
        // Moving between neighbours doesn't fade again.
        let since = self
            .switcher
            .preview
            .as_ref()
            .map_or_else(Instant::now, |p| p.since);
        self.switcher.preview = Some(Preview {
            since,
            windows,
            output,
            anchor,
            side,
            hover: None,
            close_hot: false,
            inside,
            pinned,
            peek: Peek::default(),
        });
        self.queue_redraw();
    }

    pub fn dock_preview_hide(&mut self) {
        if self.switcher.preview.take().is_some() {
            self.queue_redraw();
            self.queue_pointer_refresh();
        }
    }

    /// The dock's click on a multi-window icon: keep the panel up.
    pub fn dock_preview_pin(&mut self) {
        if let Some(p) = &mut self.switcher.preview {
            p.pinned = true;
        }
    }

    /// Whether the panel is pinned open.
    pub fn preview_pinned(&self) -> bool {
        self.switcher.preview.as_ref().is_some_and(|p| p.pinned)
    }

    /// Close a pinned panel (Escape, a click elsewhere) and tell the shell.
    pub fn dock_preview_dismiss(&mut self) {
        self.dock_preview_hide();
        self.ipc.broadcast(&cnde_ipc::Event::DockPreviewGone);
    }

    fn preview_hit(&self, pos: Point<f64, Logical>) -> Option<PreviewHit> {
        let p = self.switcher.preview.as_ref()?;
        let geo = self.space.output_geometry(&p.output)?;
        let alive = p.windows.iter().filter(|w| w.alive()).count();
        let geometry = layout(alive, geo.size.to_f64(), Place::Beside(p.anchor, p.side));
        let local = pos - geo.loc.to_f64();
        let inside = alive > 0 && geometry.panel.contains(local);
        let local = local - geometry.panel.loc;
        let cell = inside
            .then(|| geometry.cells.iter().position(|c| c.contains(local)))
            .flatten();
        let close = cell.is_some_and(|i| close_rect(geometry.cells[i]).contains(local));
        Some(PreviewHit {
            inside,
            cell,
            close,
        })
    }

    /// Whether the dock's preview covers `pos`: nothing beneath it sees the
    /// pointer.
    pub fn preview_covers(&self, pos: Point<f64, Logical>) -> bool {
        self.preview_hit(pos).is_some_and(|h| h.inside)
    }

    /// The pointer moved: light the thumbnail under it, and tell the shell
    /// when it enters or leaves the panel.
    pub fn preview_motion(&mut self, pos: Point<f64, Logical>) {
        let Some(hit) = self.preview_hit(pos) else {
            return;
        };
        let Some(p) = &mut self.switcher.preview else {
            return;
        };
        let changed = p.hover != hit.cell || p.close_hot != hit.close;
        p.hover = hit.cell;
        p.close_hot = hit.close;
        let window = hit.cell.and_then(|i| p.windows.get(i).cloned());
        p.peek.set(window, Instant::now());
        let entered = std::mem::replace(&mut p.inside, hit.inside) != hit.inside;
        if entered {
            self.ipc
                .broadcast(&cnde_ipc::Event::DockPreviewHover { inside: hit.inside });
        }
        if changed {
            self.queue_redraw();
        }
    }

    /// A button on the panel: a thumbnail focuses its window, its x closes
    /// it. Returns whether the event was the panel's.
    pub fn preview_button(&mut self, pos: Point<f64, Logical>, pressed: bool, left: bool) -> bool {
        let Some(hit) = self.preview_hit(pos).filter(|h| h.inside) else {
            // A click anywhere else puts a pinned panel away, and goes on.
            if pressed && self.preview_pinned() {
                self.dock_preview_dismiss();
            }
            return false;
        };
        if !pressed || !left {
            return true;
        }
        let window = self.switcher.preview.as_ref().and_then(|p| {
            let alive: Vec<_> = p.windows.iter().filter(|w| w.alive()).collect();
            hit.cell.and_then(|i| alive.get(i).map(|w| (*w).clone()))
        });
        if let Some(window) = window {
            if hit.close {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            } else {
                let id = with_state(&window, |s| s.id);
                self.dock_preview_hide();
                self.ipc.broadcast(&cnde_ipc::Event::DockPreviewGone);
                self.activate_window(id);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_sits_beside_its_anchor_and_stays_on_screen() {
        let output = Size::from((1920.0, 1080.0));
        let dock_icon = Rectangle::new(Point::from((900.0, 1000.0)), Size::from((56.0, 72.0)));
        let above = layout(2, output, Place::Beside(dock_icon, Side::Above));
        assert!(above.panel.loc.y + above.panel.size.h <= dock_icon.loc.y);
        let mid = above.panel.loc.x + above.panel.size.w / 2.0;
        assert!((mid - 928.0).abs() <= 1.0);
        // An icon at the far left can't push the panel off screen.
        let corner = Rectangle::new(Point::from((0.0, 1000.0)), Size::from((56.0, 72.0)));
        let left = layout(3, output, Place::Beside(corner, Side::Above));
        assert!(left.panel.loc.x >= 0.0);
        let side = Rectangle::new(Point::from((0.0, 400.0)), Size::from((72.0, 56.0)));
        let right = layout(1, output, Place::Beside(side, Side::Right));
        assert!(right.panel.loc.x >= 72.0);
    }
}
