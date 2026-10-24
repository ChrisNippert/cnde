//! Title bars cnde draws for apps (server-side decorations), so apps that
//! would otherwise fall back to their toolkit's plain bar match the desktop.
//!
//! Apps opt in through xdg-decoration. With `appearance.titlebars = "cnde"`
//! the compositor picks server-side unless the app asks to draw its own
//! (GNOME apps do, for their header bars). The bar sits above the window's
//! geometry: placements that fill an area leave room for it, and it's
//! drawn with the window, so it follows every animation.
//!
//! Floating windows also get an invisible border for resizing, since an app
//! that doesn't decorate itself doesn't offer resize edges either.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use cnde_ui::{Canvas, Color, Fonts, Icons, Weight};
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
    desktop::Window,
    input::pointer::{CursorIcon, CursorImageStatus, GrabStartData},
    reexports::wayland_protocols::xdg::{
        decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        shell::server::xdg_toplevel::State as XdgState,
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Scale, Size, Transform},
    wayland::{compositor::with_states, shell::xdg::ToplevelSurface},
};
use tracing::warn;

use crate::{
    grabs::ResizeEdge,
    render::OutputElement,
    state::State,
    window::{Placement, WindowState, with_state},
};

/// Logical pixels, like a GNOME title bar.
pub const HEIGHT: i32 = 38;
/// The round buttons at the right end.
const BUTTON: f32 = 24.0;
const BUTTON_GAP: f32 = 8.0;
/// Space between the buttons and the bar's right end.
const SIDE: f32 = 7.0;
/// The invisible resize border around floating windows.
const BORDER: f64 = 6.0;
/// Two clicks on a bar closer than this toggle maximize.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How far a press on the bar moves before it becomes a drag.
const DRAG_THRESHOLD: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Title,
    Minimize,
    Maximize,
    Close,
    Resize(ResizeEdge),
}

/// Marks a toplevel that bound xdg-decoration (stored on its surface):
/// only those may get a server-side bar. Holds the mode the app asked
/// for, so a config change can honour it again.
struct DecorationBound(Mutex<Option<DecorationMode>>);

/// Whether cnde draws `window`'s title bar.
pub fn is_ssd(window: &Window) -> bool {
    let Some(toplevel) = window.toplevel() else {
        return false;
    };
    let bound = with_states(toplevel.wl_surface(), |states| {
        states.data_map.get::<DecorationBound>().is_some()
    });
    bound && toplevel.current_state().decoration_mode == Some(DecorationMode::ServerSide)
}

/// How tall `window`'s bar is (0: none, e.g. fullscreen or self-decorated).
pub fn titlebar_height(window: &Window) -> i32 {
    if is_ssd(window) && with_state(window, |s| s.placement) != Placement::Fullscreen {
        HEIGHT
    } else {
        0
    }
}

/// The part of `rect` (a window's whole slot) left for its contents once
/// the bar takes `bar` off the top.
pub fn content_rect(rect: Rectangle<i32, Logical>, bar: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (rect.loc.x, rect.loc.y + bar).into(),
        Size::from((rect.size.w, (rect.size.h - bar).max(1))),
    )
}

/// The button (or the title) at `x` along a bar `width` wide; `all`:
/// minimize and maximize too, not just close. Tiles have no maximize
/// button: minimize takes its place.
fn part_at(width: f64, x: f64, all: bool, tiled: bool) -> Part {
    let step = (BUTTON + BUTTON_GAP) as f64;
    let close = width - SIDE as f64 - BUTTON as f64;
    let on = |start: f64| x >= start && x < start + BUTTON as f64;
    if on(close) {
        Part::Close
    } else if !all {
        Part::Title
    } else if on(close - step) {
        if tiled { Part::Minimize } else { Part::Maximize }
    } else if !tiled && on(close - 2.0 * step) {
        Part::Minimize
    } else {
        Part::Title
    }
}

/// A bar's last drawing, redrawn when anything shown on it changes.
pub struct Titlebar {
    key: Key,
    buffer: MemoryRenderBuffer,
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    title: String,
    width: i32,
    scale: u64,
    focused: bool,
    hover: Option<Part>,
    appearance: cnde_config::Appearance,
    rounded: bool,
    maximized: bool,
    /// In a tile: no maximize button.
    tiled: bool,
    all_buttons: bool,
}

/// Fonts and icons for drawing bars, loaded when the first one is needed.
#[derive(Default)]
pub struct Decorations {
    fonts: Option<Fonts>,
    icons: Option<Icons>,
    /// The style and palette.
    pub appearance: cnde_config::Appearance,
    /// Minimize and maximize buttons too (`appearance.window_buttons`).
    pub all_buttons: bool,
    /// The overview's close button, per scale and hover state.
    close_buttons: Vec<((u64, bool), MemoryRenderBuffer)>,
}

/// A press on a bar that may yet become a drag (or a double-click).
pub struct BarPress {
    window: Window,
    part: Part,
    location: Point<f64, Logical>,
}

#[derive(Default)]
pub struct BarInput {
    press: Option<BarPress>,
    last_click: Option<(Window, Instant)>,
    /// The bar the pointer is over, for hover highlights.
    hover: Option<(Window, Part)>,
    /// The pointer is on a bar or border (we set the cursor there).
    on_bar: bool,
}

impl Decorations {
    /// The bar for `window`, drawn over `rect` (output-local, after any
    /// animation); `width` is the bar's unanimated width.
    #[allow(clippy::too_many_arguments)]
    pub fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &Window,
        state: &mut WindowState,
        rect: Rectangle<f64, Logical>,
        width: i32,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Option<OutputElement> {
        if width <= 0 {
            return None;
        }
        let toplevel = window.toplevel()?;
        let current = toplevel.current_state();
        let focused = current.states.contains(XdgState::Activated);
        let key = Key {
            title: crate::capture::title_and_app_id(window).0,
            width,
            scale: scale.x.to_bits(),
            focused,
            hover: state.titlebar_hover,
            appearance: self.appearance.clone(),
            rounded: state.placement == Placement::Floating,
            maximized: state.placement == Placement::Maximized,
            tiled: state.placement == Placement::Floating
                && current.states.contains(XdgState::TiledLeft),
            all_buttons: self.all_buttons,
        };
        if state.titlebar.as_ref().is_none_or(|t| t.key != key) {
            let buffer = self.paint(&key)?;
            state.titlebar = Some(Titlebar { key, buffer });
        }
        let titlebar = state.titlebar.as_ref()?;
        let location = rect.loc.to_physical(scale);
        // The whole buffer, shrunk to the bar's logical size. (Without a
        // source, a size override crops the buffer instead: at 125% only
        // the left four fifths of the bar showed.)
        let pixels = (
            (width as f64 * scale.x).round(),
            (HEIGHT as f64 * scale.x).round(),
        );
        let element = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &titlebar.buffer,
            Some(alpha),
            Some(Rectangle::from_size(Size::from(pixels))),
            Some(Size::from((width, HEIGHT))),
            Kind::Unspecified,
        )
        .map_err(|err| warn!(?err, "can't upload a title bar"))
        .ok()?;
        let stretch = Scale {
            x: rect.size.w / width as f64,
            y: rect.size.h / HEIGHT as f64,
        };
        Some(OutputElement::Titlebar(RescaleRenderElement::from_element(
            element,
            location.to_i32_round(),
            stretch,
        )))
    }

    /// The overview's close button over `rect` (global; see
    /// `overview::CloseButton`), on an output at `output_loc`.
    pub fn close_button(
        &mut self,
        renderer: &mut GlesRenderer,
        rect: Rectangle<f64, Logical>,
        hovered: bool,
        output_loc: Point<f64, Logical>,
        scale: Scale<f64>,
    ) -> Option<OutputElement> {
        let key = (scale.x.to_bits(), hovered);
        let size = crate::overview::CLOSE_SIZE;
        let pixels = (size * scale.x).round() as u32;
        if !self.close_buttons.iter().any(|(k, _)| *k == key) {
            let mut data = vec![0u8; (pixels * pixels * 4) as usize];
            {
                let icons = self.icons.get_or_insert_with(Icons::new);
                icons.set_theme(&self.appearance.icon_theme);
                let mut canvas = Canvas::new(&mut data, pixels, pixels, scale.x as f32)?;
                let s = size as f32;
                canvas.clear(Color::rgba(0, 0, 0, 0.0));
                let fill = if hovered { 0x50 } else { 0x36 };
                canvas.circle(
                    s / 2.0,
                    s / 2.0,
                    s / 2.0,
                    Color::rgba(fill, fill, fill, 0.95),
                );
                let icon = 16.0;
                let at = (s - icon) / 2.0;
                let white = Color::rgba(0xff, 0xff, 0xff, 1.0);
                canvas.icon(icons, "window-close-symbolic", at, at, icon, white);
            }
            let buffer = MemoryRenderBuffer::from_slice(
                &data,
                Fourcc::Abgr8888,
                (pixels as i32, pixels as i32),
                1,
                Transform::Normal,
                None,
            );
            self.close_buttons.push((key, buffer));
        }
        let (_, buffer) = self.close_buttons.iter().find(|(k, _)| *k == key)?;
        let location = (rect.loc - output_loc).to_physical(scale);
        let element = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            buffer,
            None,
            Some(Rectangle::from_size(Size::from((
                pixels as f64,
                pixels as f64,
            )))),
            Some(Size::from((size as i32, size as i32))),
            Kind::Unspecified,
        )
        .map_err(|err| warn!(?err, "can't upload the close button"))
        .ok()?;
        Some(OutputElement::Titlebar(RescaleRenderElement::from_element(
            element,
            location.to_i32_round(),
            Scale::from(1.0),
        )))
    }

    fn paint(&mut self, key: &Key) -> Option<MemoryRenderBuffer> {
        let scale = f64::from_bits(key.scale);
        let (w, h) = (
            (key.width as f64 * scale).round() as u32,
            (HEIGHT as f64 * scale).round() as u32,
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
            let mut canvas = Canvas::new(&mut pixels, w, h, scale as f32)?;
            paint(&mut canvas, fonts, icons, key);
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

/// libadwaita's header bar colours.
fn paint(canvas: &mut Canvas<'_>, fonts: &mut Fonts, icons: &mut Icons, key: &Key) {
    let (width, height) = (key.width as f32, HEIGHT as f32);
    // The palette's window colours: the sidebar shade for a focused bar,
    // the page shade for the others.
    let theme = cnde_ui::Theme::for_appearance(&key.appearance);
    let dark = theme.dark;
    let ink = theme.window_text.with_alpha(1.0);
    let (background, text) = if key.focused {
        (theme.window_sidebar, theme.window_text)
    } else {
        (theme.window_background, theme.window_text.with_alpha(0.5))
    };
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    if key.rounded {
        let radius = theme.window_radius.min(height);
        canvas.rounded_rect(0.0, 0.0, width, height, radius, background);
        // Square at the bottom, where the window's contents start.
        canvas.rounded_rect(0.0, height - radius, width, radius, 0.0, background);
    } else {
        canvas.rounded_rect(0.0, 0.0, width, height, 0.0, background);
    }
    // A hairline between the bar and the contents.
    canvas.rounded_rect(
        0.0,
        height - 1.0,
        width,
        1.0,
        0.0,
        ink.with_alpha(if dark { 0.10 } else { 0.08 }),
    );

    // The buttons, right to left: close, maximize, minimize.
    let y = (height - BUTTON) / 2.0;
    let close_x = width - SIDE - BUTTON;
    let maximize_x = close_x - BUTTON_GAP - BUTTON;
    let minimize_x = if key.tiled {
        maximize_x
    } else {
        maximize_x - BUTTON_GAP - BUTTON
    };
    let maximize_icon = if key.maximized {
        "window-restore-symbolic"
    } else {
        "window-maximize-symbolic"
    };
    let buttons = [
        (close_x, Part::Close, "window-close-symbolic"),
        (maximize_x, Part::Maximize, maximize_icon),
        (minimize_x, Part::Minimize, "window-minimize-symbolic"),
    ];
    let shown = if key.all_buttons { 3 } else { 1 };
    let tiled = key.tiled;
    for (x, part, icon) in buttons
        .into_iter()
        .filter(|(_, part, _)| !(tiled && *part == Part::Maximize))
        .take(shown)
    {
        let fill = if key.hover == Some(part) { 0.18 } else { 0.10 };
        canvas.circle(
            x + BUTTON / 2.0,
            y + BUTTON / 2.0,
            BUTTON / 2.0,
            ink.with_alpha(fill),
        );
        let size = 16.0;
        canvas.icon(
            icons,
            icon,
            x + (BUTTON - size) / 2.0,
            y + (BUTTON - size) / 2.0,
            size,
            text,
        );
    }

    // The title, centred on the bar but kept clear of the buttons.
    let reserve = SIDE + 3.0 * BUTTON + 2.0 * BUTTON_GAP + 12.0;
    let reserve = if key.all_buttons {
        reserve
    } else {
        SIDE + BUTTON + 12.0
    };
    let room = (width - 2.0 * reserve).max(0.0);
    if room > 0.0 && !key.title.is_empty() {
        let mut layout = fonts.layout(&key.title, 14.0, Weight::Bold, canvas.scale());
        let x = if layout.width <= room {
            ((width - layout.width) / 2.0).round()
        } else {
            reserve
        };
        let ty = ((height - layout.height) / 2.0).round();
        canvas.text_clipped(
            fonts,
            &mut layout,
            x,
            ty,
            text,
            (reserve, 0.0, room, height),
        );
    }
}

impl State {
    /// Every toplevel that bound xdg-decoration gets the mode the config
    /// asks for; `requested` is what the app asked for, if anything.
    pub fn choose_decoration(
        &mut self,
        toplevel: &ToplevelSurface,
        requested: Option<DecorationMode>,
    ) {
        with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .insert_if_missing_threadsafe(|| DecorationBound(Mutex::new(None)));
            *states
                .data_map
                .get::<DecorationBound>()
                .unwrap()
                .0
                .lock()
                .unwrap() = requested;
        });
        self.send_decoration(toplevel);
    }

    /// The mode the config and `toplevel`'s own request give.
    fn decoration_mode(&self, toplevel: &ToplevelSurface) -> DecorationMode {
        let requested = with_states(toplevel.wl_surface(), |states| {
            *states
                .data_map
                .get::<DecorationBound>()
                .unwrap()
                .0
                .lock()
                .unwrap()
        });
        match self.config.appearance.titlebars {
            cnde_config::Titlebars::App => DecorationMode::ClientSide,
            // Apps with their own header bars ask for client-side.
            cnde_config::Titlebars::Cnde => requested.unwrap_or(DecorationMode::ServerSide),
        }
    }

    /// Sends `toplevel` the mode the config and its own request give.
    fn send_decoration(&mut self, toplevel: &ToplevelSurface) {
        let mode = self.decoration_mode(toplevel);
        toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    /// `appearance.titlebars` changed: tell every app that asked, and make
    /// room for the bar (or take it back) in maximized and snapped windows.
    pub fn apply_titlebars_config(&mut self) {
        let windows: Vec<Window> = self
            .space
            .elements()
            .chain(&self.unmapped)
            .filter(|w| {
                w.toplevel().is_some_and(|t| {
                    with_states(t.wl_surface(), |states| {
                        states.data_map.get::<DecorationBound>().is_some()
                    })
                })
            })
            .cloned()
            .collect();
        for window in windows {
            let toplevel = window.toplevel().unwrap().clone();
            let placement = with_state(&window, |s| s.placement);
            let mapped = self.space.element_geometry(&window).is_some();
            let area = self.area_for(&window, placement).filter(|_| mapped);
            let fills = !matches!(placement, Placement::Floating | Placement::Fullscreen);
            let size = match area {
                Some(area) if fills => {
                    let bar = match self.decoration_mode(&toplevel) {
                        DecorationMode::ServerSide => HEIGHT,
                        _ => 0,
                    };
                    let rect = content_rect(crate::window::placement_rect(placement, area), bar);
                    self.move_window(&window, rect.loc);
                    Some(rect.size)
                }
                // winit sizes its own bar from the configure and crashes
                // on 0x0 ("pick your own size") when bars come back.
                _ => toplevel
                    .with_pending_state(|s| s.size)
                    .or(Some(window.geometry().size)),
            };
            toplevel.with_pending_state(|s| s.size = size);
            self.send_decoration(&toplevel);
        }
        self.queue_redraw();
    }

    /// The bar or resize border at `pos`, if that's the topmost thing there.
    pub fn decoration_under(&self, pos: Point<f64, Logical>) -> Option<(Window, Part)> {
        for window in self.space.elements().rev() {
            let Some(geo) = self.space.element_geometry(window) else {
                continue;
            };
            let geo = geo.to_f64();
            if geo.contains(pos) {
                return None;
            }
            let bar = titlebar_height(window) as f64;
            if bar == 0.0 {
                continue;
            }
            let frame = Rectangle::new(
                Point::from((geo.loc.x, geo.loc.y - bar)),
                Size::from((geo.size.w, geo.size.h + bar)),
            );
            if frame.contains(pos) {
                let all = self.decorations.all_buttons;
                let tiled = self.is_tiled(window);
                return Some((
                    window.clone(),
                    part_at(frame.size.w, pos.x - frame.loc.x, all, tiled),
                ));
            }
            if with_state(window, |s| s.placement) != Placement::Floating {
                continue;
            }
            let (left, top) = (frame.loc.x, frame.loc.y);
            let (right, bottom) = (left + frame.size.w, top + frame.size.h);
            let near = pos.x >= left - BORDER
                && pos.x < right + BORDER
                && pos.y >= top - BORDER
                && pos.y < bottom + BORDER;
            if !near {
                continue;
            }
            // Outside the frame, so on at least one side; near a corner,
            // both.
            let corner = BORDER * 3.0;
            let mut edges = ResizeEdge::empty();
            if pos.x
                < left
                    + if pos.y < top || pos.y >= bottom {
                        corner
                    } else {
                        0.0
                    }
            {
                edges |= ResizeEdge::LEFT;
            } else if pos.x
                >= right
                    - if pos.y < top || pos.y >= bottom {
                        corner
                    } else {
                        0.0
                    }
            {
                edges |= ResizeEdge::RIGHT;
            }
            let along_side = !edges.is_empty();
            if pos.y < top + if along_side { corner } else { 0.0 } {
                edges |= ResizeEdge::TOP;
            } else if pos.y >= bottom - if along_side { corner } else { 0.0 } {
                edges |= ResizeEdge::BOTTOM;
            }
            if !edges.is_empty() {
                return Some((window.clone(), Part::Resize(edges)));
            }
        }
        None
    }

    /// The pointer moved: hover highlights, the cursor over a bar or
    /// border, and a press on a bar turning into a drag.
    pub fn bar_motion(&mut self, location: Point<f64, Logical>) {
        if let Some(press) = &self.bar_input.press
            && press.part == Part::Title
            && (location.x - press.location.x).hypot(location.y - press.location.y) > DRAG_THRESHOLD
        {
            let press = self.bar_input.press.take().expect("checked above");
            let start = GrabStartData {
                focus: None,
                button: crate::input::BTN_LEFT,
                location: press.location,
            };
            self.begin_move(press.window, start, SERIAL_COUNTER.next_serial());
            return;
        }
        let under = if self.overview.is_open() || self.screenshot.active {
            None
        } else {
            self.decoration_under(location)
        };
        match &under {
            Some((_, part)) => {
                let icon = match part {
                    Part::Resize(edges) => resize_cursor(*edges),
                    _ => CursorIcon::Default,
                };
                self.cursor_status = CursorImageStatus::Named(icon);
                self.bar_input.on_bar = true;
            }
            // Off a bar: back to the plain pointer until whatever is under
            // it now sets its own.
            None if std::mem::take(&mut self.bar_input.on_bar) => {
                self.cursor_status = CursorImageStatus::default_named();
            }
            None => {}
        }
        let hover =
            under.filter(|(_, part)| matches!(part, Part::Close | Part::Maximize | Part::Minimize));
        if hover.as_ref().map(|(w, p)| (w, *p))
            != self.bar_input.hover.as_ref().map(|(w, p)| (w, *p))
        {
            if let Some((old, _)) = &self.bar_input.hover {
                with_state(old, |s| s.titlebar_hover = None);
            }
            if let Some((new, part)) = &hover {
                with_state(new, |s| s.titlebar_hover = Some(*part));
            }
            self.bar_input.hover = hover;
            self.queue_redraw();
        }
    }

    /// A button press at `location`. Returns true if a bar or border took it.
    pub fn bar_press(&mut self, button: u32, location: Point<f64, Logical>) -> bool {
        let Some((window, part)) = self.decoration_under(location) else {
            return false;
        };
        self.focus_window(Some(&window));
        if button != crate::input::BTN_LEFT {
            return true;
        }
        if let Part::Resize(edges) = part {
            let start = GrabStartData {
                focus: None,
                button,
                location,
            };
            self.begin_resize(window, start, SERIAL_COUNTER.next_serial(), edges);
            return true;
        }
        if part == Part::Title {
            let now = Instant::now();
            let double = self
                .bar_input
                .last_click
                .take()
                .is_some_and(|(w, at)| w == window && now.duration_since(at) < DOUBLE_CLICK);
            if double {
                self.toggle_maximize(&window);
                return true;
            }
            self.bar_input.last_click = Some((window.clone(), now));
        }
        self.bar_input.press = Some(BarPress {
            window,
            part,
            location,
        });
        true
    }

    /// The button came back up: a click on a bar button acts now, if the
    /// pointer is still on it.
    pub fn bar_release(&mut self, location: Point<f64, Logical>) -> bool {
        let Some(press) = self.bar_input.press.take() else {
            return false;
        };
        let still = self
            .decoration_under(location)
            .is_some_and(|(w, p)| w == press.window && p == press.part);
        if still {
            match press.part {
                Part::Close => {
                    if let Some(toplevel) = press.window.toplevel() {
                        toplevel.send_close();
                    }
                }
                Part::Maximize => self.toggle_maximize(&press.window),
                Part::Minimize => self.minimize(&press.window),
                Part::Title | Part::Resize(_) => {}
            }
        }
        true
    }

    fn toggle_maximize(&mut self, window: &Window) {
        let target = if Self::placement_of(window) == Placement::Floating {
            Placement::Maximized
        } else {
            Placement::Floating
        };
        self.set_placement(window, target, true);
    }
}

fn resize_cursor(edges: ResizeEdge) -> CursorIcon {
    let (top, bottom) = (
        edges.contains(ResizeEdge::TOP),
        edges.contains(ResizeEdge::BOTTOM),
    );
    let (left, right) = (
        edges.contains(ResizeEdge::LEFT),
        edges.contains(ResizeEdge::RIGHT),
    );
    match (top, bottom, left, right) {
        (true, _, true, _) => CursorIcon::NwResize,
        (true, _, _, true) => CursorIcon::NeResize,
        (_, true, true, _) => CursorIcon::SwResize,
        (_, true, _, true) => CursorIcon::SeResize,
        (true, ..) => CursorIcon::NResize,
        (_, true, ..) => CursorIcon::SResize,
        (_, _, true, _) => CursorIcon::WResize,
        _ => CursorIcon::EResize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_sit_at_the_right_end() {
        let width = 600.0;
        assert_eq!(part_at(width, 10.0, true, false), Part::Title);
        assert_eq!(part_at(width, width - SIDE as f64 - 1.0, true, false), Part::Close);
        let maximize = width - SIDE as f64 - 2.0 * BUTTON as f64 - BUTTON_GAP as f64 + 1.0;
        assert_eq!(part_at(width, maximize, true, false), Part::Maximize);
        let minimize = maximize - (BUTTON + BUTTON_GAP) as f64;
        assert_eq!(part_at(width, minimize, true, false), Part::Minimize);
        // A tile has no maximize: minimize sits next to close.
        assert_eq!(part_at(width, maximize, true, true), Part::Minimize);
        assert_eq!(part_at(width, minimize, true, true), Part::Title);
    }

    #[test]
    fn content_leaves_room_for_the_bar() {
        let rect = Rectangle::new((0, 32).into(), Size::from((800, 600)));
        let content = content_rect(rect, HEIGHT);
        assert_eq!(content.loc.y, 32 + HEIGHT);
        assert_eq!(content.size.h, 600 - HEIGHT);
    }
}
