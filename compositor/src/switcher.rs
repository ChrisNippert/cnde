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

#[derive(Default)]
pub struct Switcher {
    session: Option<Session>,
    fonts: Option<Fonts>,
    icons: Option<Icons>,
    panel: Option<(Key, MemoryRenderBuffer)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    /// Window id, title and app id of each cell.
    cells: Vec<(u64, String, String)>,
    selected: usize,
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

fn layout(count: usize, output: Size<f64, Logical>) -> Layout {
    let fit = ((output.w * MAX_WIDTH - 2.0 * PAD) / CELL_W)
        .floor()
        .max(1.0) as usize;
    let cols = count.clamp(1, fit);
    let rows = count.max(1).div_ceil(cols);
    let size = Size::from((
        cols as f64 * CELL_W + 2.0 * PAD,
        rows as f64 * CELL_H + 2.0 * PAD,
    ));
    let panel = Rectangle::new(
        Point::from((
            ((output.w - size.w) / 2.0).round(),
            ((output.h - size.h) / 2.0).round(),
        )),
        size,
    );
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
    /// switcher is showing there.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        decorations: &mut crate::decorations::Decorations,
        space: &Space<Window>,
        output: &Output,
        now: Instant,
    ) -> Vec<OutputElement> {
        if !self.is_visible(now) {
            return Vec::new();
        }
        let Some(session) = self.session.as_ref().filter(|s| &s.output == output) else {
            return Vec::new();
        };
        let Some(output_geo) = space.output_geometry(output) else {
            return Vec::new();
        };
        let scale = Scale::from(output.current_scale().fractional_scale());
        let output_loc = output_geo.loc.to_f64();
        let geometry = layout(session.windows.len(), output_geo.size.to_f64());

        let mut elements = Vec::new();
        for (window, cell) in session.windows.iter().zip(&geometry.cells) {
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
            cells: session
                .windows
                .iter()
                .map(|w| {
                    let (title, app_id) = crate::capture::title_and_app_id(w);
                    (with_state(w, |s| s.id), title, app_id)
                })
                .collect(),
            selected: session.selected,
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
                if i == key.selected {
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
            let geometry = layout(session.windows.len(), geo.size.to_f64());
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
