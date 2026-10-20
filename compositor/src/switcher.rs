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
                utils::RescaleRenderElement,
            },
            gles::GlesRenderer,
        },
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Point, Rectangle, Scale, Size, Transform},
};
use tracing::warn;

use cnde_ipc::Side;
use cnde_ui::{Canvas, Color, Fonts, Icons, Weight};

use crate::{
    render::{OutputElement, Zoom},
    state::State,
    window::with_state,
};

/// A tap shorter than this switches without ever drawing the panel.
const SHOW_AFTER: Duration = Duration::from_millis(150);

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
}

#[derive(Default)]
pub struct Switcher {
    session: Option<Session>,
    preview: Option<Preview>,
    fonts: Option<Fonts>,
    icons: Option<Icons>,
    panel: Option<(Key, MemoryRenderBuffer)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    /// Window id, title and app id of each cell.
    cells: Vec<(u64, String, String)>,
    selected: Option<usize>,
    /// Cells carry a close button (dock previews); is the hovered one's lit.
    closable: Option<bool>,
    cols: usize,
    scale: u64,
    size: (i32, i32),
    appearance: cnde_config::Appearance,
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

    /// Whether a frame is due soon: the panel is about to appear.
    pub fn is_animating(&self, now: Instant) -> bool {
        self.session.as_ref().is_some_and(|s| {
            now.saturating_duration_since(s.started) < SHOW_AFTER + Duration::from_millis(20)
        })
    }

    fn step(&mut self, back: bool) {
        if let Some(s) = &mut self.session {
            let n = s.windows.len();
            s.selected = if back {
                (s.selected + n - 1) % n
            } else {
                (s.selected + 1) % n
            };
        }
    }

    /// The panel and thumbnails for `output`, front first; empty unless the
    /// switcher (or the dock's preview) is showing there.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        decorations: &mut crate::decorations::Decorations,
        space: &Space<Window>,
        output: &Output,
        now: Instant,
    ) -> Vec<OutputElement> {
        if self.session.is_some() {
            if !self.is_visible(now) {
                return Vec::new();
            }
            let Some(session) = self.session.as_ref().filter(|s| &s.output == output) else {
                return Vec::new();
            };
            let (windows, selected) = (session.windows.clone(), Some(session.selected));
            let Some(size) = space.output_geometry(output).map(|g| g.size.to_f64()) else {
                return Vec::new();
            };
            let geometry = layout(windows.len(), size, Place::Centre);
            return self.panel_elements(
                renderer,
                decorations,
                space,
                output,
                &windows,
                &geometry,
                selected,
                None,
                now,
            );
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
        let Some(size) = space.output_geometry(output).map(|g| g.size.to_f64()) else {
            return Vec::new();
        };
        if windows.is_empty() {
            return Vec::new();
        }
        let geometry = layout(
            windows.len(),
            size,
            Place::Beside(preview.anchor, preview.side),
        );
        let (hover, close_hot) = (preview.hover, preview.close_hot);
        self.panel_elements(
            renderer,
            decorations,
            space,
            output,
            &windows,
            &geometry,
            hover,
            Some(close_hot),
            now,
        )
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
        now: Instant,
    ) -> Vec<OutputElement> {
        let Some(output_geo) = space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_loc = output_geo.loc.to_f64();

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
            closable,
            cols: geometry.cols,
            scale: scale.x.to_bits(),
            size: (geometry.panel.size.w as i32, geometry.panel.size.h as i32),
            appearance: decorations.appearance.clone(),
        };
        if self.panel.as_ref().is_none_or(|(k, _)| *k != key) {
            let Some(buffer) = self.paint(&key) else {
                return elements;
            };
            self.panel = Some((key, buffer));
        }
        if let Some((key, buffer)) = &self.panel {
            let location = geometry.panel.loc.to_physical(scale);
            let (w, h) = key.size;
            let pixels = ((w as f64 * scale.x).round(), (h as f64 * scale.x).round());
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                buffer,
                None,
                Some(Rectangle::from_size(Size::from(pixels))),
                Some(Size::from((w, h))),
                Kind::Unspecified,
            ) {
                Ok(element) => {
                    elements.push(OutputElement::Titlebar(RescaleRenderElement::from_element(
                        element,
                        location.to_i32_round(),
                        Scale::from(1.0),
                    )))
                }
                Err(err) => warn!(?err, "can't upload the window switcher"),
            }
        }
        elements
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
            let radius = theme.window_radius.max(10.0) + 6.0;
            canvas.rounded_rect(
                0.0,
                0.0,
                pw,
                ph,
                radius,
                theme.window_background.with_alpha(0.93),
            );
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
                if key.selected == Some(i) {
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
                if key.selected == Some(i)
                    && let Some(hot) = key.closable
                {
                    let close = close_rect(*cell);
                    let (cx, cy) = (
                        (close.loc.x + close.size.w / 2.0) as f32,
                        (close.loc.y + close.size.h / 2.0) as f32,
                    );
                    let r = (CLOSE / 2.0) as f32;
                    let fill = if hot {
                        theme.accent
                    } else {
                        theme.window_text.with_alpha(0.18)
                    };
                    canvas.circle(cx, cy, r, fill);
                    let ink = if hot {
                        theme.on_accent
                    } else {
                        theme.window_text
                    };
                    canvas.icon(
                        icons,
                        "window-close-symbolic",
                        cx - r * 0.5,
                        cy - r * 0.5,
                        r,
                        ink,
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
        self.switcher.session = Some(Session {
            windows,
            selected,
            started: Instant::now(),
            output,
            held,
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
        let Some(session) = self.switcher.session.take() else {
            return;
        };
        self.queue_redraw();
        if let Some(window) = session.windows.get(session.selected).filter(|w| w.alive()) {
            let id = with_state(window, |s| s.id);
            tracing::debug!(id, title = %crate::capture::title_and_app_id(window).0, "window switcher picked");
            self.activate_window(id);
        }
    }

    pub fn switcher_cancel(&mut self) {
        if self.switcher.session.take().is_some() {
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
        let windows: Vec<Window> = ids
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
        self.switcher.preview = Some(Preview {
            windows,
            output,
            anchor,
            side,
            hover: None,
            close_hot: false,
            inside,
        });
        self.queue_redraw();
    }

    pub fn dock_preview_hide(&mut self) {
        if self.switcher.preview.take().is_some() {
            self.queue_redraw();
            self.queue_pointer_refresh();
        }
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
