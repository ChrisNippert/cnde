//! The screenshot tool (ARCHITECTURE.md §5.2), drawn over the frozen screen
//! the compositor shows after Super+Shift+S: a dimmed screen with a bright
//! selection to drag, and a toolbar with three modes. Selection: drag out a
//! rectangle (then move it, or drag a corner). Screen: click a monitor.
//! Window: click a window. Enter, Space or the round button takes the shot;
//! Escape cancels. The compositor cuts the image and saves it.
//!
//! One overlay surface per monitor, so a selection can be dragged on any of
//! them; the toolbar is on the main one. Their buffers live in a pool of
//! their own, gone when the tool closes.

use cnde_ipc::{ScreenshotTarget, ScreenshotWindow};
use cnde_ui::{Canvas, Color, Fonts, Icons, Theme, Weight};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
    reexports::client::{
        QueueHandle,
        protocol::{wl_output::WlOutput, wl_shm, wl_surface::WlSurface},
    },
    seat::keyboard::{KeyEvent, Keysym},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface},
    },
    shm::{Shm, slot::SlotPool},
};
use tracing::warn;

use crate::App;

/// Grab distance for the selection's corners, logical px.
const HANDLE: f32 = 14.0;
const DIM: Color = Color::rgba(0, 0, 0, 0.45);
const TOOLBAR_HEIGHT: f32 = 60.0;
const TOOLBAR_BOTTOM: f32 = 48.0;
const MODE_WIDTH: f32 = 96.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    /// From two corners, in any order.
    fn spanning((ax, ay): (f32, f32), (bx, by): (f32, f32)) -> Self {
        Self {
            x: ax.min(bx),
            y: ay.min(by),
            w: (ax - bx).abs(),
            h: (ay - by).abs(),
        }
    }

    fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.w).min(other.x + other.w);
        let bottom = (self.y + self.h).min(other.y + other.h);
        (right > x && bottom > y).then_some(Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Selection,
    Screen,
    Window,
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Drawing a new selection from this corner.
    New((f32, f32)),
    /// Moving the selection; pointer offset from its top-left.
    Move((f32, f32)),
    /// Resizing; the opposite corner stays put.
    Resize((f32, f32)),
}

/// What the toolbar is made of, in the main overlay's coordinates.
struct Toolbar {
    bar: Rect,
    modes: [(Rect, Mode); 3],
    capture: Rect,
    close: Rect,
}

fn toolbar(size: (f32, f32)) -> Toolbar {
    let width = 3.0 * MODE_WIDTH + 64.0 + 52.0 + 24.0;
    let bar = Rect {
        x: (size.0 - width) / 2.0,
        y: size.1 - TOOLBAR_BOTTOM - TOOLBAR_HEIGHT,
        w: width,
        h: TOOLBAR_HEIGHT,
    };
    let mode = |i: usize, mode| {
        (
            Rect {
                x: bar.x + 8.0 + i as f32 * MODE_WIDTH,
                y: bar.y + 8.0,
                w: MODE_WIDTH - 4.0,
                h: bar.h - 16.0,
            },
            mode,
        )
    };
    let capture = Rect {
        x: bar.x + 8.0 + 3.0 * MODE_WIDTH + 10.0,
        y: bar.y + 6.0,
        w: 48.0,
        h: 48.0,
    };
    let close = Rect {
        x: bar.x + bar.w - 44.0,
        y: bar.y + 14.0,
        w: 32.0,
        h: 32.0,
    };
    Toolbar {
        bar,
        modes: [
            mode(0, Mode::Selection),
            mode(1, Mode::Screen),
            mode(2, Mode::Window),
        ],
        capture,
        close,
    }
}

pub struct Overlay {
    name: String,
    pub layer: LayerSurface,
    /// Global logical position and size of its monitor.
    origin: (f32, f32),
    size: (f32, f32),
    scale: i32,
    main: bool,
    configured: bool,
    frame_pending: bool,
    dirty: bool,
}

/// One of the app's monitors, for opening the tool.
pub struct Monitor {
    pub output: WlOutput,
    pub name: String,
    pub origin: (i32, i32),
    pub size: (i32, i32),
    pub scale: i32,
    pub main: bool,
}

pub enum Outcome {
    Nothing,
    Redraw,
    Cancel,
    Take(ScreenshotTarget),
}

pub struct ScreenshotUi {
    pub overlays: Vec<Overlay>,
    pool: Option<SlotPool>,
    mode: Mode,
    /// Global logical.
    selection: Rect,
    drag: Option<Drag>,
    windows: Vec<ScreenshotWindow>,
    /// Global logical pointer position.
    pointer: (f32, f32),
}

impl ScreenshotUi {
    pub fn open(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        shm: &Shm,
        qh: &QueueHandle<App>,
        monitors: Vec<Monitor>,
        windows: Vec<ScreenshotWindow>,
    ) -> Self {
        let overlays: Vec<Overlay> = monitors
            .into_iter()
            .map(|m| {
                let layer = layer_shell.create_layer_surface(
                    qh,
                    compositor.create_surface(qh),
                    Layer::Overlay,
                    Some("cnde-screenshot"),
                    Some(&m.output),
                );
                layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
                // Over the panel too.
                layer.set_exclusive_zone(-1);
                layer.set_keyboard_interactivity(if m.main {
                    KeyboardInteractivity::Exclusive
                } else {
                    KeyboardInteractivity::None
                });
                layer.wl_surface().set_buffer_scale(m.scale);
                layer.commit();
                Overlay {
                    name: m.name,
                    layer,
                    origin: (m.origin.0 as f32, m.origin.1 as f32),
                    size: (m.size.0 as f32, m.size.1 as f32),
                    scale: m.scale.max(1),
                    main: m.main,
                    configured: false,
                    frame_pending: false,
                    dirty: false,
                }
            })
            .collect();
        // Start with the middle of the main monitor selected.
        let main = overlays.iter().find(|o| o.main).or(overlays.first());
        let selection = main.map_or(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 300.0,
            },
            |o| Rect {
                x: o.origin.0 + o.size.0 / 4.0,
                y: o.origin.1 + o.size.1 / 4.0,
                w: o.size.0 / 2.0,
                h: o.size.1 / 2.0,
            },
        );
        let pool = SlotPool::new(4 * 1024 * 1024, shm)
            .map_err(|err| warn!(?err, "can't create the screenshot buffer pool"))
            .ok();
        Self {
            overlays,
            pool,
            mode: Mode::Selection,
            selection,
            drag: None,
            windows,
            pointer: (0.0, 0.0),
        }
    }

    fn overlay_of(&self, surface: &WlSurface) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.layer.wl_surface() == surface)
    }

    pub fn configured(&mut self, surface: &WlSurface, size: (u32, u32)) -> bool {
        let Some(i) = self.overlay_of(surface) else {
            return false;
        };
        let overlay = &mut self.overlays[i];
        overlay.configured = true;
        if size.0 > 0 && size.1 > 0 {
            overlay.size = (size.0 as f32, size.1 as f32);
        }
        true
    }

    pub fn set_scale(&mut self, surface: &WlSurface, scale: i32) -> bool {
        let Some(i) = self.overlay_of(surface) else {
            return false;
        };
        self.overlays[i].scale = scale.max(1);
        self.overlays[i]
            .layer
            .wl_surface()
            .set_buffer_scale(scale.max(1));
        true
    }

    pub fn frame_done(&mut self, surface: &WlSurface) -> bool {
        let Some(i) = self.overlay_of(surface) else {
            return false;
        };
        self.overlays[i].frame_pending = false;
        self.overlays[i].dirty
    }

    pub fn owns(&self, surface: &WlSurface) -> bool {
        self.overlay_of(surface).is_some()
    }

    fn main(&self) -> Option<&Overlay> {
        self.overlays
            .iter()
            .find(|o| o.main)
            .or(self.overlays.first())
    }

    /// The main overlay's toolbar, in global coordinates.
    fn toolbar(&self) -> Option<Toolbar> {
        let main = self.main()?;
        let mut bar = toolbar(main.size);
        let shift = |r: &mut Rect| {
            r.x += main.origin.0;
            r.y += main.origin.1;
        };
        shift(&mut bar.bar);
        shift(&mut bar.capture);
        shift(&mut bar.close);
        for (rect, _) in &mut bar.modes {
            shift(rect);
        }
        Some(bar)
    }

    fn monitor_at(&self, point: (f32, f32)) -> Option<&Overlay> {
        self.overlays.iter().find(|o| {
            Rect {
                x: o.origin.0,
                y: o.origin.1,
                w: o.size.0,
                h: o.size.1,
            }
            .contains(point)
        })
    }

    fn window_at(&self, point: (f32, f32)) -> Option<&ScreenshotWindow> {
        self.windows.iter().find(|w| {
            Rect {
                x: w.x as f32,
                y: w.y as f32,
                w: w.w as f32,
                h: w.h as f32,
            }
            .contains(point)
        })
    }

    /// The shot for the current mode (Enter or the round button).
    fn take(&self) -> Outcome {
        match self.mode {
            Mode::Selection => {
                let s = self.selection;
                if s.w < 2.0 || s.h < 2.0 {
                    return Outcome::Nothing;
                }
                Outcome::Take(ScreenshotTarget::Region {
                    x: s.x.round() as i32,
                    y: s.y.round() as i32,
                    w: s.w.round() as i32,
                    h: s.h.round() as i32,
                })
            }
            Mode::Screen => {
                self.monitor_at(self.pointer)
                    .or(self.main())
                    .map_or(Outcome::Nothing, |o| {
                        Outcome::Take(ScreenshotTarget::Output {
                            name: o.name.clone(),
                        })
                    })
            }
            Mode::Window => self.window_at(self.pointer).map_or(Outcome::Nothing, |w| {
                Outcome::Take(ScreenshotTarget::Window { id: w.id })
            }),
        }
    }

    fn global(&self, surface: &WlSurface, (x, y): (f64, f64)) -> Option<(f32, f32)> {
        let o = &self.overlays[self.overlay_of(surface)?];
        Some((o.origin.0 + x as f32, o.origin.1 + y as f32))
    }

    pub fn motion(&mut self, surface: &WlSurface, position: (f64, f64)) -> Outcome {
        let Some(point) = self.global(surface, position) else {
            return Outcome::Nothing;
        };
        self.pointer = point;
        match self.drag {
            Some(Drag::New(anchor)) | Some(Drag::Resize(anchor)) => {
                self.selection = Rect::spanning(anchor, point);
            }
            Some(Drag::Move((dx, dy))) => {
                self.selection.x = point.0 - dx;
                self.selection.y = point.1 - dy;
            }
            None => {}
        }
        Outcome::Redraw
    }

    pub fn press(&mut self, surface: &WlSurface, position: (f64, f64)) -> Outcome {
        let Some(point) = self.global(surface, position) else {
            return Outcome::Nothing;
        };
        self.pointer = point;
        if let Some(bar) = self.toolbar()
            && bar.bar.contains(point)
        {
            if bar.close.contains(point) {
                return Outcome::Cancel;
            }
            if bar.capture.contains(point) {
                return self.take();
            }
            if let Some((_, mode)) = bar.modes.iter().find(|(r, _)| r.contains(point)) {
                self.mode = *mode;
                return Outcome::Redraw;
            }
            return Outcome::Nothing;
        }
        match self.mode {
            Mode::Selection => {
                let s = self.selection;
                let corners = [
                    ((s.x, s.y), (s.x + s.w, s.y + s.h)),
                    ((s.x + s.w, s.y), (s.x, s.y + s.h)),
                    ((s.x, s.y + s.h), (s.x + s.w, s.y)),
                    ((s.x + s.w, s.y + s.h), (s.x, s.y)),
                ];
                let near = |(cx, cy): (f32, f32)| {
                    (point.0 - cx).abs() <= HANDLE && (point.1 - cy).abs() <= HANDLE
                };
                self.drag = Some(
                    if let Some((_, opposite)) = corners.iter().find(|(c, _)| near(*c)) {
                        Drag::Resize(*opposite)
                    } else if s.contains(point) {
                        Drag::Move((point.0 - s.x, point.1 - s.y))
                    } else {
                        self.selection = Rect::spanning(point, point);
                        Drag::New(point)
                    },
                );
                Outcome::Redraw
            }
            // A click picks the monitor or window under it.
            Mode::Screen | Mode::Window => self.take(),
        }
    }

    pub fn release(&mut self) -> Outcome {
        if self.drag.take().is_some() {
            Outcome::Redraw
        } else {
            Outcome::Nothing
        }
    }

    pub fn key(&mut self, event: &KeyEvent) -> Outcome {
        match event.keysym {
            Keysym::Escape => Outcome::Cancel,
            Keysym::Return | Keysym::KP_Enter | Keysym::space => self.take(),
            Keysym::s | Keysym::S => {
                self.mode = Mode::Selection;
                Outcome::Redraw
            }
            Keysym::c | Keysym::C => {
                self.mode = Mode::Screen;
                Outcome::Redraw
            }
            Keysym::w | Keysym::W => {
                self.mode = Mode::Window;
                Outcome::Redraw
            }
            _ => Outcome::Nothing,
        }
    }

    pub fn draw_all(
        &mut self,
        qh: &QueueHandle<App>,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
    ) {
        for i in 0..self.overlays.len() {
            self.draw(i, qh, format, fonts, icons, theme);
        }
    }

    pub fn draw_surface(
        &mut self,
        surface: &WlSurface,
        qh: &QueueHandle<App>,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
    ) {
        if let Some(i) = self.overlay_of(surface) {
            self.draw(i, qh, format, fonts, icons, theme);
        }
    }

    fn draw(
        &mut self,
        i: usize,
        qh: &QueueHandle<App>,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
    ) {
        let overlay = &self.overlays[i];
        if !overlay.configured || overlay.frame_pending {
            self.overlays[i].dirty = true;
            return;
        }
        let scale = overlay.scale;
        let (w, h) = (
            (overlay.size.0 * scale as f32) as i32,
            (overlay.size.1 * scale as f32) as i32,
        );
        // What stays bright on this monitor (global coordinates).
        let monitor = Rect {
            x: overlay.origin.0,
            y: overlay.origin.1,
            w: overlay.size.0,
            h: overlay.size.1,
        };
        let bright = match self.mode {
            Mode::Selection => self.selection.intersect(&monitor),
            Mode::Screen => self
                .monitor_at(self.pointer)
                .filter(|o| o.name == overlay.name)
                .map(|_| monitor),
            Mode::Window => self.window_at(self.pointer).and_then(|w| {
                Rect {
                    x: w.x as f32,
                    y: w.y as f32,
                    w: w.w as f32,
                    h: w.h as f32,
                }
                .intersect(&monitor)
            }),
        };
        let bright = bright.map(|r| Rect {
            x: r.x - monitor.x,
            y: r.y - monitor.y,
            ..r
        });
        let (size, mode, bar) = (
            overlay.size,
            self.mode,
            overlay.main.then(|| toolbar(overlay.size)),
        );
        let Some(pool) = &mut self.pool else {
            return;
        };
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate screenshot buffer");
                return;
            }
        };
        {
            let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
                return;
            };
            paint(
                &mut canvas,
                size,
                bright,
                mode,
                bar,
                fonts,
                icons,
                theme,
                scale as f32,
            );
        }
        if format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let overlay = &mut self.overlays[i];
        overlay.dirty = false;
        let surface = overlay.layer.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            overlay.frame_pending = true;
            surface.commit();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    size: (f32, f32),
    bright: Option<Rect>,
    mode: Mode,
    bar: Option<Toolbar>,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    scale: f32,
) {
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    // Dim around the bright part (four bands), or everything.
    match bright {
        Some(b) => {
            canvas.rounded_rect(0.0, 0.0, size.0, b.y, 0.0, DIM);
            canvas.rounded_rect(0.0, b.y + b.h, size.0, size.1 - b.y - b.h, 0.0, DIM);
            canvas.rounded_rect(0.0, b.y, b.x, b.h, 0.0, DIM);
            canvas.rounded_rect(b.x + b.w, b.y, size.0 - b.x - b.w, b.h, 0.0, DIM);
            // Its outline (and corner handles, for a selection).
            let line = 2.0;
            let white = Color::rgb(255, 255, 255);
            let edge = if mode == Mode::Selection {
                white
            } else {
                theme.accent
            };
            canvas.rounded_rect(b.x - line, b.y - line, b.w + 2.0 * line, line, 0.0, edge);
            canvas.rounded_rect(b.x - line, b.y + b.h, b.w + 2.0 * line, line, 0.0, edge);
            canvas.rounded_rect(b.x - line, b.y, line, b.h, 0.0, edge);
            canvas.rounded_rect(b.x + b.w, b.y, line, b.h, 0.0, edge);
            if mode == Mode::Selection {
                for (cx, cy) in [
                    (b.x, b.y),
                    (b.x + b.w, b.y),
                    (b.x, b.y + b.h),
                    (b.x + b.w, b.y + b.h),
                ] {
                    canvas.circle(cx, cy, 6.0, white);
                }
            }
        }
        None => canvas.rounded_rect(0.0, 0.0, size.0, size.1, 0.0, DIM),
    }

    let Some(bar) = bar else {
        return;
    };
    canvas.rounded_rect(
        bar.bar.x,
        bar.bar.y,
        bar.bar.w,
        bar.bar.h,
        bar.bar.h / 2.0,
        theme.search_background,
    );
    for (rect, which) in &bar.modes {
        if *which == mode {
            canvas.rounded_rect(
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                rect.h / 2.0,
                theme.qs_toggle_off,
            );
        }
        let label = match which {
            Mode::Selection => "Selection",
            Mode::Screen => "Screen",
            Mode::Window => "Window",
        };
        let mut text = fonts.layout(label, 14.0, Weight::Normal, scale);
        let color = if *which == mode {
            theme.popup_text
        } else {
            theme.qs_dim
        };
        let (tx, ty) = (
            (rect.x + (rect.w - text.width) / 2.0).round(),
            (rect.y + (rect.h - text.height) / 2.0).round(),
        );
        canvas.text(fonts, &mut text, tx, ty, color);
    }
    // The shutter: a white ring around a white dot.
    let c = bar.capture;
    let (cx, cy, r) = (c.x + c.w / 2.0, c.y + c.h / 2.0, c.w / 2.0);
    canvas.circle(cx, cy, r, theme.popup_text);
    canvas.circle(cx, cy, r - 3.0, theme.search_background);
    canvas.circle(cx, cy, r - 6.0, theme.popup_text);
    let x = bar.close;
    canvas.circle(
        x.x + x.w / 2.0,
        x.y + x.h / 2.0,
        x.w / 2.0,
        theme.qs_toggle_off,
    );
    canvas.icon(
        icons,
        "window-close-symbolic",
        x.x + (x.w - 16.0) / 2.0,
        x.y + (x.h - 16.0) / 2.0,
        16.0,
        theme.popup_text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spanning_any_direction() {
        let r = Rect::spanning((100.0, 50.0), (20.0, 80.0));
        assert_eq!(
            r,
            Rect {
                x: 20.0,
                y: 50.0,
                w: 80.0,
                h: 30.0
            }
        );
    }

    #[test]
    fn toolbar_is_centred_at_the_bottom() {
        let bar = toolbar((1920.0, 1080.0));
        assert!((bar.bar.x + bar.bar.w / 2.0 - 960.0).abs() < 0.5);
        assert!(bar.bar.y + bar.bar.h < 1080.0);
        for (mode, _) in &bar.modes {
            assert!(bar.bar.intersect(mode) == Some(*mode));
        }
        assert!(bar.bar.intersect(&bar.capture) == Some(bar.capture));
    }
}
