//! The on-screen keyboard: a QWERTY grid along the bottom of the main
//! monitor, for touch screens and tablets. Keys are sent to the compositor
//! as evdev codes (`Request::OskKey`) so shortcuts, layouts and modifiers
//! treat them like a real keyboard's. It never takes keyboard focus. Held
//! keys repeat by themselves: apps repeat a key that stays down.

use std::time::{Duration, Instant};

use cnde_ui::{Canvas, Painter, Weight};
use smithay_client_toolkit::{
    compositor::FrameCallbackData,
    reexports::client::protocol::{wl_shm, wl_surface::WlSurface},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
    },
};
use tracing::warn;

use crate::App;

const NAMESPACE: &str = "cnde-osk";
/// Width of a row in key units.
const ROW_UNITS: f32 = 15.0;
/// The widest it grows, however big the monitor.
const MAX_WIDTH: u32 = 1200;
const ROW_HEIGHT: f32 = 46.0;
/// Space between keys, and around the grid.
const GAP: f32 = 6.0;
/// Kept free below it, so it floats a little like the dock.
const BOTTOM_MARGIN: i32 = 8;
/// How long it takes to slide in or out (as the notifications do).
const ANIM: Duration = Duration::from_millis(230);
const RADIUS: f32 = 18.0;
const KEY_RADIUS: f32 = 9.0;
const ROWS: usize = 5;

// evdev key codes.
const ESC: u32 = 1;
const BACKSPACE: u32 = 14;
const TAB: u32 = 15;
const ENTER: u32 = 28;
const LEFT_CTRL: u32 = 29;
const LEFT_SHIFT: u32 = 42;
const LEFT_ALT: u32 = 56;
const SPACE: u32 = 57;
const UP: u32 = 103;
const LEFT: u32 = 105;
const RIGHT: u32 = 106;
const DOWN: u32 = 108;
const LEFT_META: u32 = 125;

/// What a key does when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Types `code` (with Shift held if `Key::shift`, or if Shift is on).
    Char,
    Shift,
    /// A modifier that stays on for the next key.
    Mod(u32),
    /// Switches between the letters and the symbols page.
    Page,
    Hide,
    /// Empty space.
    Gap,
}

#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub label: &'static str,
    /// What it shows (and types) with Shift on.
    pub shifted: &'static str,
    pub code: u32,
    /// In key units.
    pub width: f32,
    pub kind: Kind,
    /// Types with Shift held whatever the Shift key says (the symbols page).
    pub shift: bool,
    /// An icon instead of a label.
    pub icon: &'static str,
}

const fn key(label: &'static str, shifted: &'static str, code: u32) -> Key {
    Key {
        label,
        shifted,
        code,
        width: 1.0,
        kind: Kind::Char,
        shift: false,
        icon: "",
    }
}

const fn wide(mut k: Key, width: f32) -> Key {
    k.width = width;
    k
}

/// A symbol typed with Shift held on its base key.
const fn sym(label: &'static str, code: u32) -> Key {
    let mut k = key(label, label, code);
    k.shift = true;
    k
}

const fn special(label: &'static str, kind: Kind, width: f32) -> Key {
    let mut k = key(label, label, 0);
    k.kind = kind;
    k.width = width;
    k
}

const fn named(label: &'static str, code: u32, width: f32) -> Key {
    wide(key(label, label, code), width)
}

const fn with_icon(mut k: Key, icon: &'static str) -> Key {
    k.icon = icon;
    k
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Letters,
    Symbols,
}

/// The bottom row, the same on both pages (the page key says which).
fn bottom_row(page: Page) -> Vec<Key> {
    vec![
        special("Ctrl", Kind::Mod(LEFT_CTRL), 1.5),
        special("Super", Kind::Mod(LEFT_META), 1.25),
        special("Alt", Kind::Mod(LEFT_ALT), 1.25),
        named("", SPACE, 6.5),
        special(
            if page == Page::Letters { "123" } else { "abc" },
            Kind::Page,
            1.5,
        ),
        with_icon(key("", "", LEFT), "go-previous-symbolic"),
        with_icon(key("", "", DOWN), "go-down-symbolic"),
        with_icon(key("", "", RIGHT), "go-next-symbolic"),
    ]
}

/// A run of one-unit keys, `(label, shifted, code)`.
fn run(keys: &[(&'static str, &'static str, u32)]) -> Vec<Key> {
    keys.iter().map(|&(l, s, c)| key(l, s, c)).collect()
}

/// The rows of keys on `page`, top to bottom. Each is `ROW_UNITS` wide.
pub fn rows(page: Page) -> Vec<Vec<Key>> {
    // Hide, then Up above the Down key, then a gap.
    let row_end = [
        special("Hide", Kind::Hide, 1.0),
        with_icon(key("", "", UP), "go-up-symbolic"),
        special("", Kind::Gap, 1.0),
    ];
    let mut rows = Vec::new();
    match page {
        Page::Letters => {
            let mut r0 = run(&[
                ("`", "~", 41),
                ("1", "!", 2),
                ("2", "@", 3),
                ("3", "#", 4),
                ("4", "$", 5),
                ("5", "%", 6),
                ("6", "^", 7),
                ("7", "&", 8),
                ("8", "*", 9),
                ("9", "(", 10),
                ("0", ")", 11),
                ("-", "_", 12),
                ("=", "+", 13),
            ]);
            r0.push(with_icon(named("", BACKSPACE, 2.0), "edit-clear-symbolic"));
            rows.push(r0);
            let mut r1 = vec![named("Tab", TAB, 1.5)];
            r1.extend(run(&[
                ("q", "Q", 16),
                ("w", "W", 17),
                ("e", "E", 18),
                ("r", "R", 19),
                ("t", "T", 20),
                ("y", "Y", 21),
                ("u", "U", 22),
                ("i", "I", 23),
                ("o", "O", 24),
                ("p", "P", 25),
                ("[", "{", 26),
                ("]", "}", 27),
            ]));
            r1.push(wide(key("\\", "|", 43), 1.5));
            rows.push(r1);
            let mut r2 = vec![named("Esc", ESC, 1.75)];
            r2.extend(run(&[
                ("a", "A", 30),
                ("s", "S", 31),
                ("d", "D", 32),
                ("f", "F", 33),
                ("g", "G", 34),
                ("h", "H", 35),
                ("j", "J", 36),
                ("k", "K", 37),
                ("l", "L", 38),
                (";", ":", 39),
                ("'", "\"", 40),
            ]));
            r2.push(named("Enter", ENTER, 2.25));
            rows.push(r2);
            let mut r3 = vec![special("Shift", Kind::Shift, 2.0)];
            r3.extend(run(&[
                ("z", "Z", 44),
                ("x", "X", 45),
                ("c", "C", 46),
                ("v", "V", 47),
                ("b", "B", 48),
                ("n", "N", 49),
                ("m", "M", 50),
                (",", "<", 51),
                (".", ">", 52),
                ("/", "?", 53),
            ]));
            r3.extend(row_end);
            rows.push(r3);
        }
        Page::Symbols => {
            let mut r0 = vec![named("Esc", ESC, 1.5)];
            r0.extend(run(&[
                ("1", "1", 2),
                ("2", "2", 3),
                ("3", "3", 4),
                ("4", "4", 5),
                ("5", "5", 6),
                ("6", "6", 7),
                ("7", "7", 8),
                ("8", "8", 9),
                ("9", "9", 10),
                ("0", "0", 11),
                ("-", "-", 12),
                ("=", "=", 13),
            ]));
            r0.push(with_icon(named("", BACKSPACE, 1.5), "edit-clear-symbolic"));
            rows.push(r0);
            let mut r1 = vec![named("Tab", TAB, 1.5)];
            r1.extend([
                sym("!", 2),
                sym("@", 3),
                sym("#", 4),
                sym("$", 5),
                sym("%", 6),
                sym("^", 7),
                sym("&", 8),
                sym("*", 9),
                sym("(", 10),
                sym(")", 11),
                sym("_", 12),
                sym("+", 13),
            ]);
            r1.push(wide(sym("|", 43), 1.5));
            rows.push(r1);
            let mut r2: Vec<Key> = [
                key("[", "[", 26),
                key("]", "]", 27),
                sym("{", 26),
                sym("}", 27),
                key(";", ";", 39),
                sym(":", 39),
                key("'", "'", 40),
                sym("\"", 40),
                key(",", ",", 51),
                key(".", ".", 52),
            ]
            .into_iter()
            .map(|k| wide(k, 1.25))
            .collect();
            r2.push(named("Enter", ENTER, 2.5));
            rows.push(r2);
            let mut r3 = vec![special("Shift", Kind::Shift, 2.0)];
            r3.extend([
                sym("<", 51),
                sym(">", 52),
                key("/", "/", 53),
                sym("?", 53),
                key("\\", "\\", 43),
                key("`", "`", 41),
                sym("~", 41),
                key("-", "-", 12),
                key("=", "=", 13),
                key(".", ".", 52),
            ]);
            r3.extend(row_end);
            rows.push(r3);
        }
    }
    rows.push(bottom_row(page));
    rows
}

/// Where a key is, in logical px within the surface.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    pub key: Key,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Placed {
    fn contains(&self, x: f64, y: f64) -> bool {
        let (x, y) = (x as f32, y as f32);
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// The height of the keyboard's surface.
pub fn surface_height() -> f32 {
    ROWS as f32 * ROW_HEIGHT + (ROWS as f32 + 1.0) * GAP
}

/// The bottom margin for a presence `p` (0-1): at 0 the whole surface is
/// below the bottom edge, at 1 it sits `BOTTOM_MARGIN` up from it. The
/// layer moves, so the windows (and the frost, which follows the layer)
/// don't have to be told anything.
pub fn bottom_margin(p: f32) -> i32 {
    let below = surface_height().ceil() as i32 + BOTTOM_MARGIN;
    BOTTOM_MARGIN - ((1.0 - p.clamp(0.0, 1.0)) * below as f32).round() as i32
}


/// Scales premultiplied ARGB pixels by `presence` (0 clears them, 1 leaves
/// them as they are).
fn fade(pixels: &mut [u8], presence: f32) {
    for byte in pixels.iter_mut() {
        *byte = (f32::from(*byte) * presence).round() as u8;
    }
}

/// Where the keyboard is in its slide: presence eases from `from` to `to`
/// starting at `at`, so turning back starts from wherever it has got to.
#[derive(Debug, Clone, Copy)]
struct Slide {
    from: f32,
    to: f32,
    at: Instant,
}

impl Slide {
    /// 0 (below the edge) to 1 (in place), eased.
    fn value(&self, now: Instant, duration: Duration) -> f32 {
        let t = cnde_ui::anim::ease_out(cnde_ui::anim::progress(self.at, duration, now));
        self.from + (self.to - self.from) * t
    }

    fn done(&self, now: Instant, duration: Duration) -> bool {
        cnde_ui::anim::progress(self.at, duration, now) >= 1.0
    }

    /// Head for `to` from wherever it is now.
    fn turn(&mut self, to: f32, now: Instant, duration: Duration) {
        *self = Self {
            from: self.value(now, duration),
            to,
            at: now,
        };
    }
}

/// Every key of `page` laid out across a surface `width` wide.
pub fn layout(page: Page, width: f32) -> Vec<Placed> {
    let unit = (width - 2.0 * GAP) / ROW_UNITS;
    let mut out = Vec::new();
    for (r, row) in rows(page).into_iter().enumerate() {
        let y = GAP + r as f32 * (ROW_HEIGHT + GAP);
        let mut x = GAP;
        for key in row {
            let w = key.width * unit;
            if key.kind != Kind::Gap {
                out.push(Placed {
                    key,
                    x,
                    y,
                    w: w - GAP,
                    h: ROW_HEIGHT,
                });
            }
            x += w;
        }
    }
    out
}

/// The key at (`x`, `y`), as an index into `keys`.
pub fn hit(keys: &[Placed], x: f64, y: f64) -> Option<usize> {
    keys.iter().position(|k| k.contains(x, y))
}

/// Shift: off, on for the next key, or locked (caps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shift {
    #[default]
    Off,
    Once,
    Caps,
}

impl Shift {
    /// The Shift key was tapped: off to once, once (a double tap) to caps,
    /// caps back to off.
    pub fn tap(self) -> Self {
        match self {
            Shift::Off => Shift::Once,
            Shift::Once => Shift::Caps,
            Shift::Caps => Shift::Off,
        }
    }

    /// A character key was typed: a one-shot Shift is spent.
    pub fn typed(self) -> Self {
        match self {
            Shift::Once => Shift::Off,
            other => other,
        }
    }

    /// Whether Shift applies to this key: caps only to letters.
    pub fn applies(self, key: &Key) -> bool {
        match self {
            Shift::Off => false,
            Shift::Once => true,
            Shift::Caps => key.label.len() == 1 && key.label.chars().all(char::is_alphabetic),
        }
    }
}

/// A modifier tapped: latched, or let go if it was.
pub fn toggle_mod(mods: &mut Vec<u32>, code: u32) {
    match mods.iter().position(|m| *m == code) {
        Some(i) => {
            mods.remove(i);
        }
        None => mods.push(code),
    }
}

/// A key currently held down, and what to let go of with it.
struct Held {
    index: usize,
    code: u32,
    mods: Vec<u32>,
}

pub struct Osk {
    pub layer: LayerSurface,
    page: Page,
    shift: Shift,
    /// Ctrl, Alt, Super tapped for the next key.
    mods: Vec<u32>,
    keys: Vec<Placed>,
    width: u32,
    scale: i32,
    configured: bool,
    hover: Option<usize>,
    held: Option<Held>,
    /// For the frost behind it.
    output: Option<String>,
    frame_pending: bool,
    slide: Slide,
}

impl Osk {
    /// Heading for the bottom edge (or already there).
    fn closing(&self) -> bool {
        self.slide.to == 0.0
    }

    /// Up, rather than on its way out.
    pub fn open(&self) -> bool {
        !self.closing()
    }
}

impl App {
    /// Show the keyboard, or slide it away; toggled mid-slide, it turns
    /// back from where it is.
    pub fn toggle_osk(&mut self) {
        match self.osk.as_ref().map(Osk::closing) {
            None => self.show_osk(),
            Some(true) => self.turn_osk(1.0),
            Some(false) => self.close_osk(),
        }
        self.redraw_panels();
    }

    /// Slide the keyboard to `to` (1 up, 0 down) from where it is.
    fn turn_osk(&mut self, to: f32) {
        let now = Instant::now();
        let duration = cnde_ui::anim::scaled(ANIM);
        let Some(osk) = &mut self.osk else {
            return;
        };
        osk.slide.turn(to, now, duration);
        self.draw_osk();
    }

    /// Slide the keyboard away, letting go of anything held. It is destroyed
    /// once it's below the edge (at once if animations are off).
    fn close_osk(&mut self) {
        self.osk_release();
        if let Some(osk) = &mut self.osk {
            osk.hover = None;
        }
        self.turn_osk(0.0);
    }

    fn show_osk(&mut self) {
        let Some(panel) = self.panels.first() else {
            return;
        };
        let (output, scale) = (panel.output.clone(), panel.scale());
        let width = panel.width().clamp(320, MAX_WIDTH);
        let output_name = self.outputs.info(&output).and_then(|i| i.name);
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(NAMESPACE),
            Some(&output),
        );
        let height = surface_height().ceil() as u32;
        let now = Instant::now();
        layer.set_anchor(Anchor::BOTTOM);
        // Starts below the edge, and slides up.
        layer.set_margin(0, 0, bottom_margin(0.0), 0);
        layer.set_size(width, height);
        // Drawn over windows, so it reserves no space.
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();
        self.osk = Some(Osk {
            slide: Slide {
                from: 0.0,
                to: 1.0,
                at: now,
            },
            layer,
            page: Page::Letters,
            shift: Shift::Off,
            mods: Vec::new(),
            keys: layout(Page::Letters, width as f32),
            width,
            scale,
            configured: false,
            hover: None,
            held: None,
            output: output_name,
            frame_pending: false,
        });
    }

    /// Put the keyboard away, letting go of anything held, and drop the
    /// frost behind it.
    pub fn drop_osk(&mut self) {
        self.osk_release();
        let Some(osk) = self.osk.take() else {
            return;
        };
        if let Some(output) = osk.output {
            self.ipc.send(&cnde_ipc::Request::NotificationFrost {
                output,
                cards: Vec::new(),
                layer: NAMESPACE.to_string(),
            });
        }
    }

    pub fn osk_configured(&mut self) {
        if let Some(osk) = &mut self.osk {
            osk.configured = true;
        }
        self.draw_osk();
    }

    /// The compositor is ready for a new frame.
    pub fn osk_frame(&mut self, surface: &WlSurface) {
        if let Some(osk) = &mut self.osk
            && osk.layer.wl_surface() == surface
        {
            osk.frame_pending = false;
            self.draw_osk();
        }
    }

    /// The pointer moved over the keyboard (`None`: it left).
    pub fn osk_motion(&mut self, position: Option<(f64, f64)>) {
        let Some(osk) = &mut self.osk else {
            return;
        };
        let hover = position.and_then(|(x, y)| hit(&osk.keys, x, y));
        if hover != osk.hover {
            osk.hover = hover;
            self.draw_osk();
        }
        if position.is_none() {
            self.osk_release();
        }
    }

    /// A press on the keyboard.
    pub fn osk_press(&mut self, (x, y): (f64, f64)) {
        let Some(osk) = &mut self.osk else {
            return;
        };
        // On its way out: nothing gets typed.
        if osk.closing() {
            return;
        }
        let Some(index) = hit(&osk.keys, x, y) else {
            return;
        };
        let key = osk.keys[index].key;
        match key.kind {
            Kind::Shift => osk.shift = osk.shift.tap(),
            Kind::Mod(code) => toggle_mod(&mut osk.mods, code),
            Kind::Page => {
                osk.page = match osk.page {
                    Page::Letters => Page::Symbols,
                    Page::Symbols => Page::Letters,
                };
                osk.keys = layout(osk.page, osk.width as f32);
                osk.hover = None;
            }
            Kind::Hide => {
                self.close_osk();
                self.redraw_panels();
                return;
            }
            Kind::Gap => {}
            Kind::Char => {
                // Modifiers go down first, and come up after the key.
                let mut mods = osk.mods.clone();
                if key.shift || osk.shift.applies(&key) {
                    mods.push(LEFT_SHIFT);
                }
                osk.held = Some(Held {
                    index,
                    code: key.code,
                    mods: mods.clone(),
                });
                for code in mods {
                    self.ipc.send(&cnde_ipc::Request::OskKey {
                        keycode: code,
                        pressed: true,
                    });
                }
                self.ipc.send(&cnde_ipc::Request::OskKey {
                    keycode: key.code,
                    pressed: true,
                });
            }
        }
        self.draw_osk();
    }

    /// The button came up: let go of the held key and its modifiers.
    pub fn osk_release(&mut self) {
        let Some(osk) = &mut self.osk else {
            return;
        };
        let Some(held) = osk.held.take() else {
            return;
        };
        // Typing spends a one-shot Shift and the latched modifiers.
        osk.shift = osk.shift.typed();
        osk.mods.clear();
        self.ipc.send(&cnde_ipc::Request::OskKey {
            keycode: held.code,
            pressed: false,
        });
        for code in held.mods.into_iter().rev() {
            self.ipc.send(&cnde_ipc::Request::OskKey {
                keycode: code,
                pressed: false,
            });
        }
        self.draw_osk();
    }

    fn draw_osk(&mut self) {
        let now = Instant::now();
        let duration = cnde_ui::anim::scaled(ANIM);
        let Some(osk) = &self.osk else {
            return;
        };
        // Slid all the way down: gone.
        if osk.closing() && osk.slide.done(now, duration) {
            self.drop_osk();
            return;
        }
        if !osk.configured {
            return;
        }
        let presence = osk.slide.value(now, duration);
        let animating = !osk.slide.done(now, duration);
        let scale = osk.scale;
        let (lw, lh) = (osk.width as f32, surface_height().ceil());
        let (w, h) = (osk.width as i32 * scale, lh as i32 * scale);
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate the on-screen keyboard");
                return;
            }
        };
        pixels.fill(0);
        if let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) {
            let t = &self.theme;
            canvas.rounded_rect(0.0, 0.0, lw, lh, RADIUS, t.qs_background);
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: t,
            };
            for (i, placed) in osk.keys.iter().enumerate() {
                let key = &placed.key;
                let on = match key.kind {
                    Kind::Shift => osk.shift != Shift::Off,
                    Kind::Mod(code) => osk.mods.contains(&code),
                    _ => false,
                };
                let pressed = osk.held.as_ref().is_some_and(|h| h.index == i);
                let base = if key.kind == Kind::Char {
                    t.qs_tile
                } else {
                    t.qs_toggle_off
                };
                let fill = if on {
                    t.accent
                } else if pressed {
                    t.accent.with_alpha(0.5)
                } else {
                    base
                };
                p.canvas
                    .rounded_rect(placed.x, placed.y, placed.w, placed.h, KEY_RADIUS, fill);
                if osk.hover == Some(i) && !on && !pressed {
                    p.canvas.rounded_rect(
                        placed.x,
                        placed.y,
                        placed.w,
                        placed.h,
                        KEY_RADIUS,
                        t.button_hover,
                    );
                }
                let ink = if on { t.on_accent } else { t.popup_text };
                let (cx, cy) = (placed.x + placed.w / 2.0, placed.y + placed.h / 2.0);
                let icon = match (key.kind, osk.shift) {
                    (Kind::Hide, _) => "go-down-symbolic",
                    (Kind::Shift, Shift::Caps) => "go-top-symbolic",
                    (Kind::Shift, _) => "go-up-symbolic",
                    _ => key.icon,
                };
                if !icon.is_empty() {
                    p.icon(icon, cx, cy, 20.0, ink);
                    continue;
                }
                let label = if key.kind == Kind::Char && osk.shift.applies(key) {
                    key.shifted
                } else {
                    key.label
                };
                let size = if label.chars().count() > 1 {
                    t.font_size * 0.85
                } else {
                    t.font_size * 1.15
                };
                let tw = p.measure(label, size, Weight::Normal).min(placed.w);
                p.label_v(label, cx - tw / 2.0, cy, placed.w - 4.0, size, Weight::Normal, ink);
            }
        }
        // Fades in and out with the slide (the pixels are premultiplied, so
        // every channel scales with the alpha).
        if presence < 1.0 {
            fade(pixels, presence);
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let Some(osk) = &mut self.osk else {
            return;
        };
        // The layer itself moves, so the frost behind it moves too.
        osk.layer.set_margin(0, 0, bottom_margin(presence), 0);
        let surface = osk.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            if animating && !osk.frame_pending {
                surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                osk.frame_pending = true;
            }
            surface.commit();
        }
        if let Some(output) = osk.output.clone() {
            self.ipc.send(&cnde_ipc::Request::NotificationFrost {
                output,
                cards: vec![cnde_ipc::CardFrost {
                    rect: [0.0, 0.0, lw.into(), lh.into()],
                    alpha: presence,
                    radius: Some(RADIUS.into()),
                }],
                layer: NAMESPACE.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_full_width() {
        for page in [Page::Letters, Page::Symbols] {
            assert_eq!(rows(page).len(), ROWS);
            for (i, row) in rows(page).iter().enumerate() {
                let sum: f32 = row.iter().map(|k| k.width).sum();
                assert!((sum - ROW_UNITS).abs() < 1e-4, "{page:?} row {i} is {sum}");
            }
        }
    }

    #[test]
    fn keys_stay_inside_and_apart() {
        let keys = layout(Page::Letters, 900.0);
        for k in &keys {
            assert!(k.x >= 0.0 && k.x + k.w <= 900.0);
            assert!(k.y + k.h <= surface_height());
        }
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                let apart =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{} overlaps {}", a.key.label, b.key.label);
            }
        }
    }

    #[test]
    fn hit_finds_the_key_under_a_point() {
        let keys = layout(Page::Letters, 900.0);
        let q = keys.iter().position(|k| k.key.label == "q").unwrap();
        let k = keys[q];
        let (cx, cy) = ((k.x + k.w / 2.0) as f64, (k.y + k.h / 2.0) as f64);
        assert_eq!(hit(&keys, cx, cy), Some(q));
        // The gap between keys, and outside, hit nothing.
        assert_eq!(hit(&keys, (k.x + k.w + 1.0) as f64, cy), None);
        assert_eq!(hit(&keys, -1.0, -1.0), None);
    }

    #[test]
    fn shift_cycles_and_is_spent() {
        let s = Shift::Off.tap();
        assert_eq!(s, Shift::Once);
        assert_eq!(s.typed(), Shift::Off);
        // A double tap locks.
        assert_eq!(s.tap(), Shift::Caps);
        assert_eq!(Shift::Caps.typed(), Shift::Caps);
        assert_eq!(Shift::Caps.tap(), Shift::Off);
    }

    #[test]
    fn caps_shifts_only_letters() {
        let all = rows(Page::Letters).concat();
        let find = |l: &str| *all.iter().find(|k| k.label == l).unwrap();
        assert!(Shift::Caps.applies(&find("a")));
        assert!(!Shift::Caps.applies(&find("1")));
        assert!(Shift::Once.applies(&find("1")));
        assert!(!Shift::Off.applies(&find("a")));
    }

    #[test]
    fn modifiers_latch_and_unlatch() {
        let mut mods = Vec::new();
        toggle_mod(&mut mods, LEFT_CTRL);
        toggle_mod(&mut mods, LEFT_ALT);
        assert!(mods.contains(&LEFT_CTRL) && mods.contains(&LEFT_ALT));
        toggle_mod(&mut mods, LEFT_CTRL);
        assert!(!mods.contains(&LEFT_CTRL) && mods.contains(&LEFT_ALT));
    }

    #[test]
    fn margin_slides_from_below_the_edge() {
        // At 0 the whole surface is below the bottom edge.
        assert_eq!(bottom_margin(0.0), -(surface_height().ceil() as i32));
        assert_eq!(bottom_margin(1.0), BOTTOM_MARGIN);
        let mid = bottom_margin(0.5);
        assert!(mid > bottom_margin(0.0) && mid < BOTTOM_MARGIN);
    }

    #[test]
    fn slide_eases_in_and_out() {
        let t0 = Instant::now();
        let d = Duration::from_millis(200);
        let up = Slide {
            from: 0.0,
            to: 1.0,
            at: t0,
        };
        assert_eq!(up.value(t0, d), 0.0);
        // Halfway through an ease-out is 7/8 of the way.
        assert!((up.value(t0 + d / 2, d) - 0.875).abs() < 1e-5);
        assert!(!up.done(t0 + d / 2, d));
        assert_eq!(up.value(t0 + d, d), 1.0);
        assert!(up.done(t0 + d, d));
        let down = Slide {
            from: 1.0,
            to: 0.0,
            at: t0,
        };
        assert_eq!(down.value(t0 + d, d), 0.0);
        // No animation: it's already where it's going.
        assert_eq!(up.value(t0, Duration::ZERO), 1.0);
    }

    #[test]
    fn turning_back_starts_from_where_it_is() {
        let t0 = Instant::now();
        let d = Duration::from_millis(200);
        let mut slide = Slide {
            from: 0.0,
            to: 1.0,
            at: t0,
        };
        let mid = t0 + d / 2;
        let p = slide.value(mid, d);
        slide.turn(0.0, mid, d);
        assert_eq!(slide.to, 0.0);
        // No jump at the reversal, and it heads down from there.
        assert!((slide.value(mid, d) - p).abs() < 1e-6);
        assert!(slide.value(mid + d / 2, d) < p);
        assert_eq!(slide.value(mid + d, d), 0.0);
        assert!(slide.done(mid + d, d));
    }

    #[test]
    fn fade_scales_pixels_with_presence() {
        let mut px = [200u8, 100, 50, 255];
        fade(&mut px, 0.5);
        assert_eq!(px, [100, 50, 25, 128]);
        fade(&mut px, 0.0);
        assert_eq!(px, [0, 0, 0, 0]);
    }
}
