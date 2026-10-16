//! The dock (ARCHITECTURE.md §5.2): favorite apps and running ones, plus a
//! button for the app grid. By default it lives in the overview, like
//! GNOME's dash; the config can put it on a screen edge instead.
//!
//! Clicking an app that's running focuses its window (again: the next one),
//! one that isn't launches it, and a middle click always opens a new one.
//! The window list and focusing go through the compositor (see `ipc`).

use cnde_config::DockPosition;
use cnde_ipc::WindowInfo;
use cnde_ui::{Canvas, Color, Icons, Theme};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
    reexports::client::{
        QueueHandle,
        protocol::{wl_output::WlOutput, wl_shm},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface},
    },
    shm::slot::SlotPool,
};
use tracing::warn;

use crate::{App, apps::App as AppEntry};

/// Padding around each icon, and inside the dock's background.
const CELL_PAD: f32 = 8.0;
const INNER_PAD: f32 = 6.0;
/// Gap between the dock and the screen edge.
const EDGE_MARGIN: i32 = 8;
/// In the overview it sits a little higher.
const OVERVIEW_MARGIN: i32 = 20;
/// Separator between favorites and other running apps.
const SEPARATOR: f32 = 12.0;

/// One icon in the dock.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The desktop entry, if we know it.
    pub app: Option<String>,
    /// For running apps without a desktop entry: their window's app ID.
    pub icon: String,
    pub favorite: bool,
    /// Its open windows, in the compositor's order.
    pub windows: Vec<u64>,
    pub focused: bool,
}

/// What the dock shows: favorites first (running or not), then every other
/// running app.
pub fn items(favorites: &[String], apps: &[AppEntry], windows: &[WindowInfo]) -> Vec<Item> {
    let mut claimed = vec![false; windows.len()];
    let mut out = Vec::new();
    let take = |entry: &AppEntry, favorite: bool, claimed: &mut Vec<bool>| {
        let mut item = Item {
            app: Some(entry.id.clone()),
            icon: entry.icon.clone(),
            favorite,
            windows: Vec::new(),
            focused: false,
        };
        for (i, window) in windows.iter().enumerate() {
            if !claimed[i] && entry.owns(&window.app_id) {
                claimed[i] = true;
                item.windows.push(window.id);
                item.focused |= window.focused;
            }
        }
        item
    };
    for id in favorites {
        if let Some(entry) = apps.iter().find(|a| &a.id == id) {
            out.push(take(entry, true, &mut claimed));
        }
    }
    for (i, window) in windows.iter().enumerate() {
        if claimed[i] {
            continue;
        }
        if let Some(entry) = apps.iter().find(|a| a.owns(&window.app_id)) {
            out.push(take(entry, false, &mut claimed));
        } else {
            // No desktop entry: group by app ID, its icon by that name.
            claimed[i] = true;
            match out
                .iter_mut()
                .find(|item| item.app.is_none() && item.icon == window.app_id)
            {
                Some(item) => {
                    item.windows.push(window.id);
                    item.focused |= window.focused;
                }
                None => out.push(Item {
                    app: None,
                    icon: window.app_id.clone(),
                    favorite: false,
                    windows: vec![window.id],
                    focused: window.focused,
                }),
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn contains(&self, (px, py): (f64, f64)) -> bool {
        let (px, py) = (px as f32, py as f32);
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    /// One per item, then the apps button last.
    pub cells: Vec<Rect>,
    /// Where the favorites/others separator goes, if there is one.
    pub separator: Option<Rect>,
}

/// The edit-mode buttons after the apps button, in order.
pub const CONTROLS: [Control; 4] = [
    Control::Move,
    Control::Smaller,
    Control::Bigger,
    Control::Done,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Drag towards a screen edge to move the dock there.
    Move,
    Smaller,
    Bigger,
    Done,
}

/// `editing` adds the edit-mode buttons ([`CONTROLS`]) at the end.
pub fn layout(items: &[Item], icon: f32, vertical: bool, editing: bool) -> Layout {
    let cell = icon + 2.0 * CELL_PAD;
    let favorites = items.iter().filter(|i| i.favorite).count();
    let separated = favorites > 0 && favorites < items.len();
    let mut along = INNER_PAD;
    let mut cells = Vec::new();
    let mut separator = None;
    let rect = |along: f32| {
        if vertical {
            Rect {
                x: INNER_PAD,
                y: along,
                w: cell,
                h: cell,
            }
        } else {
            Rect {
                x: along,
                y: INNER_PAD,
                w: cell,
                h: cell,
            }
        }
    };
    let count = items.len() + 1 + if editing { CONTROLS.len() } else { 0 };
    for i in 0..count {
        // The separator before the first non-favorite, and before the apps
        // button if there are any items at all.
        let before_controls = editing && i == items.len() + 1;
        if (separated && i == favorites)
            || (i == items.len() && !items.is_empty())
            || before_controls
        {
            let line = if vertical {
                Rect {
                    x: INNER_PAD + CELL_PAD,
                    y: along + SEPARATOR / 2.0 - 0.5,
                    w: cell - 2.0 * CELL_PAD,
                    h: 1.0,
                }
            } else {
                Rect {
                    x: along + SEPARATOR / 2.0 - 0.5,
                    y: INNER_PAD + CELL_PAD,
                    w: 1.0,
                    h: cell - 2.0 * CELL_PAD,
                }
            };
            if i == favorites && separated {
                separator = Some(line);
            }
            along += SEPARATOR;
        }
        cells.push(rect(along));
        along += cell;
    }
    along += INNER_PAD;
    let thickness = cell + 2.0 * INNER_PAD;
    let (width, height) = if vertical {
        (thickness, along)
    } else {
        (along, thickness)
    };
    Layout {
        width,
        height,
        cells,
        separator,
    }
}

/// Editing the dock in place (from its right-click menu).
#[derive(Debug, Default)]
pub struct DockEdit {
    pub drag: Option<DockDrag>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DockDrag {
    /// A pinned icon being carried to a new place; `at` is the pointer.
    Icon { index: usize, at: (f64, f64) },
    /// The move grip, being dragged towards a screen edge.
    Move { at: (f64, f64) },
}

/// What a press hits while editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditHit {
    /// The × on a pinned icon: unpin it.
    Unpin(usize),
    /// A pinned icon: start carrying it.
    Icon(usize),
    Control(Control),
}

/// The × badge's size, at an icon's top corner.
const BADGE: f32 = 18.0;

pub enum Click {
    Nothing,
    Launch(String),
    Activate(u64),
    AppGrid,
}

pub struct Dock {
    pub output: WlOutput,
    pub layer: LayerSurface,
    position: DockPosition,
    scale: i32,
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    size: (u32, u32),
    items: Vec<Item>,
    hover: Option<usize>,
    pub edit: Option<DockEdit>,
}

impl Dock {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        qh: &QueueHandle<App>,
        output: WlOutput,
        position: DockPosition,
        scale: i32,
    ) -> Self {
        let surface = compositor.create_surface(qh);
        // In the overview it floats over everything; on an edge it's part of
        // the desktop, like the panel.
        let layer_kind = if position == DockPosition::Overview {
            Layer::Overlay
        } else {
            Layer::Top
        };
        let layer = layer_shell.create_layer_surface(
            qh,
            surface,
            layer_kind,
            Some("cnde-dock"),
            Some(&output),
        );
        let (anchor, margin) = match position {
            DockPosition::Left => (Anchor::LEFT, EDGE_MARGIN),
            DockPosition::Right => (Anchor::RIGHT, EDGE_MARGIN),
            DockPosition::Overview => (Anchor::BOTTOM, OVERVIEW_MARGIN),
            _ => (Anchor::BOTTOM, EDGE_MARGIN),
        };
        layer.set_anchor(anchor);
        layer.set_margin(margin, margin, margin, margin);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.wl_surface().set_buffer_scale(scale);
        Self {
            output,
            layer,
            position,
            scale: scale.max(1),
            configured: false,
            frame_pending: false,
            dirty: false,
            size: (0, 0),
            items: Vec::new(),
            hover: None,
            edit: None,
        }
    }

    pub fn position(&self) -> DockPosition {
        self.position
    }

    fn layout(&self, icon: f32) -> Layout {
        layout(&self.items, icon, self.vertical(), self.edit.is_some())
    }

    /// Recompute the size after starting or finishing editing.
    pub fn relayout(&mut self, icon: f32) {
        let items = std::mem::take(&mut self.items);
        self.size = (0, 0);
        self.update(items, icon);
    }

    /// What a press at `point` hits while editing.
    pub fn edit_hit(&self, point: (f64, f64), icon: f32) -> Option<EditHit> {
        let layout = self.layout(icon);
        let index = layout.cells.iter().position(|c| c.contains(point))?;
        let cell = layout.cells[index];
        if let Some(item) = self.items.get(index) {
            if !item.favorite {
                return None;
            }
            let badge = badge_rect(cell);
            return Some(if badge.contains(point) {
                EditHit::Unpin(index)
            } else {
                EditHit::Icon(index)
            });
        }
        let control = index.checked_sub(self.items.len() + 1)?;
        CONTROLS.get(control).map(|c| EditHit::Control(*c))
    }

    /// Where a carried icon would land: an index among the pinned ones.
    pub fn drop_index(&self, at: (f64, f64), icon: f32) -> usize {
        let layout = self.layout(icon);
        let favorites = self.items.iter().filter(|i| i.favorite).count();
        let along = if self.vertical() { at.1 } else { at.0 } as f32;
        layout.cells[..favorites]
            .iter()
            .filter(|c| {
                let centre = if self.vertical() {
                    c.y + c.h / 2.0
                } else {
                    c.x + c.w / 2.0
                };
                along > centre
            })
            .count()
    }

    fn vertical(&self) -> bool {
        matches!(self.position, DockPosition::Left | DockPosition::Right)
    }

    /// New items or icon size: resize (and reserve the space, on an edge).
    pub fn update(&mut self, items: Vec<Item>, icon: f32) {
        self.items = items;
        let layout = self.layout(icon);
        let size = (layout.width.ceil() as u32, layout.height.ceil() as u32);
        if size != self.size {
            self.size = size;
            self.layer.set_size(size.0, size.1);
            if self.position != DockPosition::Overview {
                // Maximized windows stop at the dock.
                let thickness = if self.vertical() { size.0 } else { size.1 };
                self.layer
                    .set_exclusive_zone(thickness as i32 + EDGE_MARGIN);
            }
            if !self.configured {
                self.layer.commit();
            }
        }
        let cells = self.items.len()
            + 1
            + if self.edit.is_some() {
                CONTROLS.len()
            } else {
                0
            };
        self.hover = self.hover.filter(|&h| h < cells);
    }

    pub fn configured(&mut self) {
        self.configured = true;
    }

    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.max(1);
        self.layer.wl_surface().set_buffer_scale(self.scale);
    }

    pub fn frame_done(&mut self) -> bool {
        self.frame_pending = false;
        self.dirty
    }

    /// Returns true if the hover changed.
    pub fn hover(&mut self, point: Option<(f64, f64)>, icon: f32) -> bool {
        let layout = self.layout(icon);
        let hover = point.and_then(|p| layout.cells.iter().position(|c| c.contains(p)));
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// The cell under `point`: its index and rect. The last index is the
    /// apps button (past the end of [`Dock::items`]).
    pub fn cell_at(&self, point: (f64, f64), icon: f32) -> Option<(usize, Rect)> {
        let layout = self.layout(icon);
        layout
            .cells
            .iter()
            .position(|c| c.contains(point))
            .map(|i| (i, layout.cells[i]))
    }

    pub fn scale(&self) -> i32 {
        self.scale
    }

    /// The dock's own size, logical.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// `new_window`: middle click, always launch.
    pub fn click(&self, point: (f64, f64), icon: f32, new_window: bool) -> Click {
        let layout = self.layout(icon);
        let Some(index) = layout.cells.iter().position(|c| c.contains(point)) else {
            return Click::Nothing;
        };
        let Some(item) = self.items.get(index) else {
            return Click::AppGrid;
        };
        if new_window || item.windows.is_empty() {
            return item.app.clone().map_or(Click::Nothing, Click::Launch);
        }
        // Focused already: cycle to its next window.
        let next = if item.focused && item.windows.len() > 1 {
            item.windows[1]
        } else {
            item.windows[0]
        };
        Click::Activate(next)
    }

    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        icons: &mut Icons,
        theme: &Theme,
        icon: f32,
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let layout = self.layout(icon);
        let scale = self.scale;
        let (w, h) = (
            (layout.width.ceil() * scale as f32) as i32,
            (layout.height.ceil() * scale as f32) as i32,
        );
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate dock buffer");
                return;
            }
        };
        {
            let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
                return;
            };
            paint(
                &mut canvas,
                &layout,
                &self.items,
                self.hover,
                self.vertical(),
                self.edit.as_ref(),
                icons,
                theme,
                icon,
            );
        }
        if format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            surface.commit();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    items: &[Item],
    hover: Option<usize>,
    vertical: bool,
    edit: Option<&DockEdit>,
    icons: &mut Icons,
    theme: &Theme,
    icon: f32,
) {
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    let drag = edit.and_then(|e| e.drag);
    // Carrying the dock: tinted, so it's clear it's moving.
    let background = if matches!(drag, Some(DockDrag::Move { .. })) {
        theme.search_background.mix(theme.accent, 0.35)
    } else {
        theme.search_background
    };
    canvas.rounded_rect(
        0.0,
        0.0,
        layout.width,
        layout.height,
        theme.search_results_radius,
        background,
    );
    if edit.is_some() {
        canvas.stroke_rounded_rect(
            0.0,
            0.0,
            layout.width,
            layout.height,
            theme.search_results_radius,
            2.0,
            theme.accent,
        );
    }
    let carried = match drag {
        Some(DockDrag::Icon { index, at }) => Some((index, at)),
        _ => None,
    };
    if let Some(line) = layout.separator {
        canvas.rounded_rect(line.x, line.y, line.w, line.h, 0.0, theme.qs_toggle_off);
    }
    for (i, cell) in layout.cells.iter().enumerate() {
        // The carried icon is drawn at the pointer instead, last.
        if carried.is_some_and(|(index, _)| index == i) {
            continue;
        }
        if hover == Some(i) && drag.is_none() {
            canvas.rounded_rect(
                cell.x + 2.0,
                cell.y + 2.0,
                cell.w - 4.0,
                cell.h - 4.0,
                theme.search_row_radius,
                theme.search_selected,
            );
        }
        let (x, y) = (cell.x + CELL_PAD, cell.y + CELL_PAD);
        let Some(item) = items.get(i) else {
            // The apps button, then (editing) the edit buttons.
            let control = i.checked_sub(items.len() + 1).and_then(|c| CONTROLS.get(c));
            if control == Some(&Control::Move) {
                // A four-way arrow: drag me somewhere.
                let (cx, cy) = (cell.x + cell.w / 2.0, cell.y + cell.h / 2.0);
                let size = icon * 0.26;
                let reach = icon * 0.17;
                for (name, dx, dy) in [
                    ("pan-up-symbolic", 0.0, -reach),
                    ("pan-down-symbolic", 0.0, reach),
                    ("pan-start-symbolic", -reach, 0.0),
                    ("pan-end-symbolic", reach, 0.0),
                ] {
                    canvas.icon(
                        icons,
                        name,
                        cx + dx - size / 2.0,
                        cy + dy - size / 2.0,
                        size,
                        theme.popup_text,
                    );
                }
                continue;
            }
            let name = match control {
                None => "view-app-grid-symbolic",
                Some(Control::Move) => unreachable!("drawn above"),
                Some(Control::Smaller) => "zoom-out-symbolic",
                Some(Control::Bigger) => "zoom-in-symbolic",
                Some(Control::Done) => "object-select-symbolic",
            };
            let mut color = theme.popup_text;
            if control == Some(&Control::Done) {
                canvas.circle(
                    cell.x + cell.w / 2.0,
                    cell.y + cell.h / 2.0,
                    cell.w * 0.3,
                    theme.accent,
                );
                color = theme.on_accent;
            }
            let size = if control.is_some() {
                icon * 0.4
            } else {
                icon * 0.5
            };
            canvas.icon(
                icons,
                name,
                cell.x + (cell.w - size) / 2.0,
                cell.y + (cell.h - size) / 2.0,
                size,
                color,
            );
            continue;
        };
        if !canvas.app_icon(icons, &item.icon, x, y, icon) {
            canvas.icon(
                icons,
                "application-x-executable-symbolic",
                x + icon * 0.15,
                y + icon * 0.15,
                icon * 0.7,
                theme.qs_dim,
            );
        }
        if edit.is_some() && item.favorite {
            let b = badge_rect(*cell);
            canvas.circle(
                b.x + b.w / 2.0,
                b.y + b.h / 2.0,
                b.w / 2.0,
                theme.popup_text,
            );
            canvas.icon(
                icons,
                "window-close-symbolic",
                b.x + 3.0,
                b.y + 3.0,
                b.w - 6.0,
                theme.search_background.with_alpha(1.0),
            );
        }
        // Running: a dot by the icon; focused: a short bar.
        if !item.windows.is_empty() {
            let dot = 4.0;
            let long = if item.focused { 14.0 } else { dot };
            let color = if item.focused {
                theme.popup_text
            } else {
                theme.qs_dim
            };
            let (dx, dy, dw, dh) = if vertical {
                (cell.x + 1.0, cell.y + (cell.h - long) / 2.0, dot, long)
            } else {
                (
                    cell.x + (cell.w - long) / 2.0,
                    cell.y + cell.h - dot - 1.0,
                    long,
                    dot,
                )
            };
            canvas.rounded_rect(dx, dy, dw, dh, dot / 2.0, color);
        }
    }
    if let Some((index, at)) = carried
        && let (Some(item), Some(cell)) = (items.get(index), layout.cells.get(index))
    {
        // Where it would go: a bar between icons.
        let favorites = items.iter().filter(|i| i.favorite).count();
        let along = if vertical { at.1 } else { at.0 } as f32;
        let slot = layout.cells[..favorites]
            .iter()
            .filter(|c| {
                along
                    > if vertical {
                        c.y + c.h / 2.0
                    } else {
                        c.x + c.w / 2.0
                    }
            })
            .count();
        let edge = match layout.cells.get(slot) {
            Some(c) if slot < favorites => {
                if vertical {
                    c.y
                } else {
                    c.x
                }
            }
            _ => {
                let last = layout.cells[favorites - 1];
                if vertical {
                    last.y + last.h
                } else {
                    last.x + last.w
                }
            }
        };
        if vertical {
            canvas.rounded_rect(
                cell.x + 6.0,
                edge - 1.5,
                cell.w - 12.0,
                3.0,
                1.5,
                theme.accent,
            );
        } else {
            canvas.rounded_rect(
                edge - 1.5,
                cell.y + 6.0,
                3.0,
                cell.h - 12.0,
                1.5,
                theme.accent,
            );
        }
        // The icon itself, under the pointer (along the dock only).
        let (x, y) = if vertical {
            (cell.x + CELL_PAD, at.1 as f32 - icon / 2.0)
        } else {
            (at.0 as f32 - icon / 2.0, cell.y + CELL_PAD)
        };
        canvas.app_icon(icons, &item.icon, x, y, icon);
    }
}

/// The × on a pinned icon while editing: its top-right corner.
fn badge_rect(cell: Rect) -> Rect {
    Rect {
        x: cell.x + cell.w - BADGE - 1.0,
        y: cell.y + 1.0,
        w: BADGE,
        h: BADGE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, app_id: &str, focused: bool) -> WindowInfo {
        WindowInfo {
            id,
            app_id: app_id.into(),
            title: String::new(),
            focused,
        }
    }

    #[test]
    fn layout_has_an_apps_button_last() {
        let items = vec![
            Item {
                app: Some("a.desktop".into()),
                icon: "a".into(),
                favorite: true,
                windows: vec![],
                focused: false,
            };
            3
        ];
        let l = layout(&items, 48.0, false, false);
        assert_eq!(l.cells.len(), 4);
        assert!(
            l.cells[3].x > l.cells[2].x + l.cells[2].w,
            "a gap before the apps button"
        );
        assert!(l.width > l.height);
        let v = layout(&items, 48.0, true, false);
        assert!(v.height > v.width);
    }

    /// `CNDE_DOCK_PREVIEW=/path cargo test -p cnde-shell dock_preview -- --ignored`
    /// renders a sample dock at 1x as raw RGBA with a "WxH" header line.
    #[test]
    #[ignore]
    fn dock_preview() {
        let Some(path) = std::env::var_os("CNDE_DOCK_PREVIEW") else {
            return;
        };
        let theme = Theme::dark();
        let apps = crate::apps::load(&crate::apps::application_dirs());
        let favorites: Vec<String> = [
            "firefox.desktop",
            "org.gnome.Nautilus.desktop",
            "Alacritty.desktop",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let windows = vec![window(1, "firefox", true), window(2, "discord", false)];
        let items = items(&favorites, &apps, &windows);
        let l = layout(&items, 48.0, false, false);
        let (w, h) = (l.width.ceil() as u32, l.height.ceil() as u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut canvas = Canvas::new(&mut pixels, w, h, 1.0).unwrap();
            let mut icons = Icons::new();
            paint(
                &mut canvas,
                &l,
                &items,
                Some(1),
                false,
                None,
                &mut icons,
                &theme,
                48.0,
            );
        }
        let mut data = format!("{w}x{h}\n").into_bytes();
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn unknown_running_apps_get_their_own_items() {
        let windows = vec![window(1, "mystery", true), window(2, "mystery", false)];
        let items = items(&[], &[], &windows);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].windows, vec![1, 2]);
        assert!(items[0].focused);
    }
}
