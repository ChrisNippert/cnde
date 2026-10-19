//! Right-click menus: on dock icons, the dock itself, the top bar and the
//! wallpaper. An xdg_popup like quick settings, so the compositor grabs
//! input for it and a click anywhere else closes it.
//!
//! The wallpaper isn't one of our surfaces, so for the desktop menu the
//! compositor tells us where the click was (`Event::DesktopMenu`) and we
//! hang the menu off a small transparent layer surface placed there.

use std::time::{Duration, Instant};

use cnde_ui::{Animator, Canvas, Color, Fonts, Icons, Theme, Weight, anim::SHORT};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
    reexports::{
        client::{
            QueueHandle,
            protocol::{wl_seat::WlSeat, wl_shm},
        },
        protocols::xdg::shell::client::xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
    },
    shell::{
        wlr_layer::LayerSurface,
        xdg::{XdgPositioner, XdgShell, popup::Popup},
    },
    shm::slot::SlotPool,
};
use tracing::warn;

use crate::App;

/// A value to write into config.toml.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<String>),
}

/// What choosing an item does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Open cnde-settings, on a page.
    Settings(Option<&'static str>),
    /// Write these keys to config.toml together.
    Config(Vec<(&'static str, Value)>),
    /// Start an app (a desktop file ID): a new window.
    Launch(String),
    /// Run a shell command line, detached.
    Run(String),
    Activate(u64),
    /// Close these windows (quitting an app).
    Close(Vec<u64>),
    /// Start editing the dock in place.
    EditDock,
    /// Start editing the top bar in place.
    EditPanel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Bold, not clickable: what the menu is about (an app's name).
    Header(String),
    /// Small and dim, not clickable: names the choices under it.
    Section(String),
    Item {
        label: String,
        action: Action,
        /// A check mark: `Some(true)` shows it, `Some(false)` leaves room.
        check: Option<bool>,
    },
    Separator,
}

impl Entry {
    pub fn item(label: impl Into<String>, action: Action) -> Self {
        Entry::Item {
            label: label.into(),
            action,
            check: None,
        }
    }

    pub fn check(label: impl Into<String>, checked: bool, action: Action) -> Self {
        Entry::Item {
            label: label.into(),
            action,
            check: Some(checked),
        }
    }

    fn height(&self) -> f32 {
        match self {
            Entry::Header(_) | Entry::Item { .. } => ROW,
            Entry::Section(_) => 26.0,
            Entry::Separator => 9.0,
        }
    }
}

const ROW: f32 = 34.0;
const PAD: f32 = 6.0;
/// Room on the left for check marks, when any item has one.
const CHECK_SPACE: f32 = 24.0;
const MIN_WIDTH: f32 = 200.0;

/// Where to put the menu, in the parent surface's coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub rect: (i32, i32, i32, i32),
    pub anchor: Anchor,
    pub gravity: Gravity,
}

pub struct Menu {
    pub popup: Popup,
    /// For the desktop menu: the 1x1 surface it hangs from.
    pub holder: Option<LayerSurface>,
    entries: Vec<Entry>,
    width: f32,
    height: f32,
    scale: i32,
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    hover: Option<usize>,
    anim: Animator<usize>,
    opened: Instant,
}

impl Menu {
    /// Open `entries` as a popup of `parent` (a layer surface).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        compositor: &CompositorState,
        xdg_shell: &XdgShell,
        qh: &QueueHandle<App>,
        parent: &LayerSurface,
        seat: &WlSeat,
        serial: u32,
        placement: Placement,
        entries: Vec<Entry>,
        fonts: &mut Fonts,
        theme: &Theme,
        scale: i32,
    ) -> Option<Self> {
        let width = measure(&entries, fonts, theme);
        let height = entries.iter().map(Entry::height).sum::<f32>() + 2.0 * PAD;
        let positioner = XdgPositioner::new(xdg_shell)
            .map_err(|err| warn!(?err, "no xdg positioner"))
            .ok()?;
        positioner.set_size(width.ceil() as i32, height.ceil() as i32);
        let (x, y, w, h) = placement.rect;
        positioner.set_anchor_rect(x, y, w.max(1), h.max(1));
        positioner.set_anchor(placement.anchor);
        positioner.set_gravity(placement.gravity);
        positioner.set_constraint_adjustment(
            ConstraintAdjustment::SlideX
                | ConstraintAdjustment::SlideY
                | ConstraintAdjustment::FlipX
                | ConstraintAdjustment::FlipY,
        );
        let surface = compositor.create_surface(qh);
        let popup = Popup::from_surface(None, &positioner, qh, surface, xdg_shell)
            .map_err(|err| warn!(?err, "can't create menu popup"))
            .ok()?;
        parent.get_popup(popup.xdg_popup());
        popup.xdg_popup().grab(seat, serial);
        popup.wl_surface().set_buffer_scale(scale);
        popup.wl_surface().commit();
        Some(Self {
            popup,
            holder: None,
            entries,
            width,
            height,
            scale: scale.max(1),
            configured: false,
            frame_pending: false,
            dirty: false,
            hover: None,
            anim: Animator::new(),
            opened: Instant::now(),
        })
    }

    pub fn configured(&mut self) {
        self.configured = true;
    }

    pub fn frame_done(&mut self) -> bool {
        self.frame_pending = false;
        self.dirty
    }

    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.max(1);
        self.popup.wl_surface().set_buffer_scale(self.scale);
    }

    fn entry_at(&self, (_, y): (f64, f64)) -> Option<usize> {
        entry_at(&self.entries, y)
    }

    /// A release (left or right) picks an item, so press-drag-release
    /// works; but not the release of the right-click that opened it.
    pub fn release_counts(&self) -> bool {
        self.opened.elapsed() > RELEASE_GRACE
    }

    /// Returns true if the hover changed.
    pub fn motion(&mut self, point: Option<(f64, f64)>) -> bool {
        let hover = point.and_then(|p| self.entry_at(p));
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// What clicking at `point` does.
    pub fn click(&self, point: (f64, f64)) -> Option<Action> {
        match self.entries.get(self.entry_at(point)?)? {
            Entry::Item { action, .. } => Some(action.clone()),
            _ => None,
        }
    }

    /// Up/Down move the highlight; returns the action for Return.
    pub fn key(&mut self, step: i32) {
        let items: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Entry::Item { .. }))
            .map(|(i, _)| i)
            .collect();
        if items.is_empty() {
            return;
        }
        let at = self.hover.and_then(|h| items.iter().position(|&i| i == h));
        let next = match at {
            None if step > 0 => 0,
            None => items.len() - 1,
            Some(at) => (at as i32 + step).rem_euclid(items.len() as i32) as usize,
        };
        self.hover = Some(items[next]);
    }

    pub fn chosen(&self) -> Option<Action> {
        match self.entries.get(self.hover?)? {
            Entry::Item { action, .. } => Some(action.clone()),
            _ => None,
        }
    }

    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let scale = self.scale;
        let (w, h) = (
            (self.width * scale as f32).ceil() as i32,
            (self.height * scale as f32).ceil() as i32,
        );
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate menu buffer");
                return;
            }
        };
        {
            let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
                return;
            };
            self.anim.begin();
            paint(
                &mut canvas,
                &self.entries,
                self.hover,
                &mut self.anim,
                fonts,
                icons,
                theme,
                self.width,
                self.height,
            );
        }
        if format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = self.popup.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            if self.anim.running() {
                self.dirty = true;
            }
            surface.commit();
        }
    }
}

/// The clickable entry at height `y`.
fn entry_at(entries: &[Entry], y: f64) -> Option<usize> {
    let mut top = PAD;
    for (i, entry) in entries.iter().enumerate() {
        let h = entry.height();
        if (y as f32) >= top && (y as f32) < top + h {
            return matches!(entry, Entry::Item { .. }).then_some(i);
        }
        top += h;
    }
    None
}

fn has_checks(entries: &[Entry]) -> bool {
    entries
        .iter()
        .any(|e| matches!(e, Entry::Item { check: Some(_), .. }))
}

fn measure(entries: &[Entry], fonts: &mut Fonts, theme: &Theme) -> f32 {
    let check = if has_checks(entries) {
        CHECK_SPACE
    } else {
        0.0
    };
    let widest = entries
        .iter()
        .map(|e| match e {
            Entry::Header(text) => fonts.layout(text, theme.font_size, Weight::Bold, 1.0).width,
            Entry::Item { label, .. } => {
                check
                    + fonts
                        .layout(label, theme.font_size, Weight::Normal, 1.0)
                        .width
            }
            Entry::Section(text) => fonts.layout(text, 12.0, Weight::Normal, 1.0).width,
            Entry::Separator => 0.0,
        })
        .fold(0.0, f32::max);
    (widest + 2.0 * PAD + 28.0).clamp(MIN_WIDTH, 420.0).ceil()
}

#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    entries: &[Entry],
    hover: Option<usize>,
    anim: &mut Animator<usize>,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    width: f32,
    height: f32,
) {
    let s = canvas.scale();
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    canvas.rounded_rect(
        0.0,
        0.0,
        width,
        height,
        theme.menu_radius,
        theme.qs_background,
    );
    let check_space = if has_checks(entries) {
        CHECK_SPACE
    } else {
        0.0
    };
    let text_x = PAD + 12.0;
    let mut y = PAD;
    for (i, entry) in entries.iter().enumerate() {
        let h = entry.height();
        match entry {
            Entry::Header(text) => {
                let mut layout = fonts.layout(text, theme.font_size, Weight::Bold, s);
                let ty = (y + (h - layout.height) / 2.0).round();
                canvas.text(fonts, &mut layout, text_x, ty, theme.popup_text);
            }
            Entry::Section(text) => {
                let mut layout = fonts.layout(text, 12.0, Weight::Normal, s);
                let ty = (y + h - layout.height - 2.0).round();
                canvas.text(fonts, &mut layout, text_x, ty, theme.qs_dim);
            }
            Entry::Separator => {
                let line = theme.qs_dim.with_alpha(theme.qs_dim.a * 0.3);
                canvas.rounded_rect(
                    PAD + 8.0,
                    y + h / 2.0,
                    width - 2.0 * PAD - 16.0,
                    1.0,
                    0.0,
                    line,
                );
            }
            Entry::Item { label, check, .. } => {
                let lit = anim.get(i, if hover == Some(i) { 1.0 } else { 0.0 }, SHORT);
                if lit > 0.0 {
                    let fill = theme.search_selected;
                    canvas.rounded_rect(
                        PAD,
                        y,
                        width - 2.0 * PAD,
                        h,
                        (theme.menu_radius - PAD).max(0.0),
                        fill.with_alpha(fill.a * lit),
                    );
                }
                if *check == Some(true) {
                    canvas.icon(
                        icons,
                        "object-select-symbolic",
                        text_x - 2.0,
                        y + (h - 16.0) / 2.0,
                        16.0,
                        theme.popup_text,
                    );
                }
                let mut layout = fonts.layout(label, theme.font_size, Weight::Normal, s);
                let ty = (y + (h - layout.height) / 2.0).round();
                canvas.text(
                    fonts,
                    &mut layout,
                    text_x + check_space,
                    ty,
                    theme.popup_text,
                );
            }
        }
        y += h;
    }
}

/// How long after a right-click its release still counts as part of it
/// (so releasing over the menu doesn't pick the item under the pointer).
pub const RELEASE_GRACE: Duration = Duration::from_millis(300);

#[cfg(test)]
mod tests {
    use super::*;

    fn menu_entries() -> Vec<Entry> {
        vec![
            Entry::Header("Firefox".into()),
            Entry::item("New Window", Action::Launch("firefox.desktop".into())),
            Entry::Separator,
            Entry::Section("Position".into()),
            Entry::check("Bottom", true, Action::EditDock),
        ]
    }

    /// CNDE_SNAPSHOTS=dir saves a rendering of a menu to look at.
    #[test]
    fn paints() {
        let theme = Theme::dark();
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let entries = menu_entries();
        let width = measure(&entries, &mut fonts, &theme);
        let height = entries.iter().map(Entry::height).sum::<f32>() + 2.0 * PAD;
        let scale = 2.0;
        let (w, h) = ((width * scale) as u32, (height * scale) as u32);
        let mut pixmap = cnde_ui::tiny_skia::Pixmap::new(w, h).unwrap();
        let mut canvas = Canvas::new(pixmap.data_mut(), w, h, scale).unwrap();
        paint(
            &mut canvas,
            &entries,
            Some(1),
            &mut Animator::new(),
            &mut fonts,
            &mut icons,
            &theme,
            width,
            height,
        );
        if let Some(dir) = std::env::var_os("CNDE_SNAPSHOTS") {
            pixmap
                .save_png(std::path::Path::new(&dir).join("menu.png"))
                .unwrap();
        }
    }

    #[test]
    fn only_items_are_clickable() {
        let entries = menu_entries();
        let at = |index: usize| {
            (PAD + entries[..index].iter().map(Entry::height).sum::<f32>() + 2.0) as f64
        };
        assert_eq!(entry_at(&entries, at(0)), None, "header");
        assert_eq!(entry_at(&entries, at(1)), Some(1));
        assert_eq!(entry_at(&entries, at(2)), None, "separator");
        assert_eq!(entry_at(&entries, at(4)), Some(4));
    }
}
