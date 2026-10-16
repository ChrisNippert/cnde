//! The overview's search (ARCHITECTURE.md §5.2): a pill at the top of the
//! main monitor that takes the keyboard while the overview is open. Typing
//! shows matching apps in a card below it; Enter or a click launches one.
//!
//! The compositor owns the overview itself and tells us when it opens and
//! closes (see `ipc`); this surface only exists in between.

use cnde_ui::{Canvas, Color, Fonts, Icons, Theme, Weight};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData, Region},
    reexports::client::{
        QueueHandle,
        protocol::{wl_output::WlOutput, wl_shm},
    },
    seat::keyboard::{KeyEvent, Keysym, Modifiers},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface},
    },
    shm::slot::SlotPool,
};
use tracing::warn;

use crate::{
    App,
    apps::{self, App as AppEntry},
};

/// Gap between the panel and the search pill.
const TOP_MARGIN: i32 = 20;

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

/// Where everything goes, in logical pixels on the surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: f32,
    /// The surface is always this tall; what's unused is transparent and
    /// lets clicks through.
    pub height: f32,
    pub pill: Rect,
    /// `None` while the query is empty.
    pub card: Option<Rect>,
    pub rows: Vec<Rect>,
}

/// Layout for `results` matches (`query_empty`: no card at all).
pub fn layout(theme: &Theme, results: usize, query_empty: bool) -> Layout {
    let width = theme.search_width;
    let pad = 8.0;
    let pill = Rect {
        x: 0.0,
        y: 0.0,
        w: width,
        h: theme.search_height,
    };
    let card_y = pill.h + theme.search_gap;
    let height = card_y + 2.0 * pad + theme.search_max_results as f32 * theme.search_row_height;
    if query_empty {
        return Layout {
            width,
            height,
            pill,
            card: None,
            rows: Vec::new(),
        };
    }
    let rows: Vec<Rect> = (0..results)
        .map(|i| Rect {
            x: pad,
            y: card_y + pad + i as f32 * theme.search_row_height,
            w: width - 2.0 * pad,
            h: theme.search_row_height,
        })
        .collect();
    // No results: a card just tall enough to say so.
    let inner = if results == 0 {
        theme.search_row_height
    } else {
        results as f32 * theme.search_row_height
    };
    Layout {
        width,
        height,
        pill,
        card: Some(Rect {
            x: 0.0,
            y: card_y,
            w: width,
            h: inner + 2.0 * pad,
        }),
        rows,
    }
}

pub enum KeyOutcome {
    Nothing,
    Redraw,
    /// Escape on an empty query.
    Close,
    Launch(String),
}

pub struct Launcher {
    pub output: WlOutput,
    pub layer: LayerSurface,
    scale: i32,
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    query: String,
    /// Desktop file IDs of the current matches, best first.
    results: Vec<String>,
    selected: usize,
}

impl Launcher {
    pub fn new(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        qh: &QueueHandle<App>,
        output: WlOutput,
        theme: &Theme,
        scale: i32,
    ) -> Self {
        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("cnde-overview-search"),
            Some(&output),
        );
        let size = layout(theme, 0, true);
        layer.set_anchor(Anchor::TOP);
        layer.set_size(size.width as u32, size.height as u32);
        layer.set_margin(TOP_MARGIN, 0, 0, 0);
        // Typing goes straight here, the moment the overview opens.
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();
        Self {
            output,
            layer,
            scale: scale.max(1),
            configured: false,
            frame_pending: false,
            dirty: false,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
        }
    }

    pub fn configured(&mut self) {
        self.configured = true;
    }

    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.max(1);
        self.layer.wl_surface().set_buffer_scale(self.scale);
    }

    /// The compositor is ready for a new frame. Returns true if one is owed.
    pub fn frame_done(&mut self) -> bool {
        self.frame_pending = false;
        self.dirty
    }

    /// Re-run the search, e.g. after the app list was reloaded.
    pub fn refresh(&mut self, apps: &[AppEntry], counts: &apps::LaunchCounts, theme: &Theme) {
        let previous = self.results.get(self.selected).cloned();
        self.results = apps::search(apps, &self.query, &counts.counts, theme.search_max_results)
            .into_iter()
            .map(|a| a.id.clone())
            .collect();
        // Keep the same app selected if it's still there.
        self.selected = previous
            .and_then(|id| self.results.iter().position(|r| *r == id))
            .unwrap_or(0);
    }

    pub fn key(
        &mut self,
        event: &KeyEvent,
        modifiers: &Modifiers,
        apps: &[AppEntry],
        counts: &apps::LaunchCounts,
        theme: &Theme,
    ) -> KeyOutcome {
        match event.keysym {
            Keysym::Escape if self.query.is_empty() => return KeyOutcome::Close,
            Keysym::Escape => self.query.clear(),
            Keysym::Return | Keysym::KP_Enter => {
                return match self.results.get(self.selected) {
                    Some(id) => KeyOutcome::Launch(id.clone()),
                    None => KeyOutcome::Nothing,
                };
            }
            Keysym::Down | Keysym::Tab => {
                if !self.results.is_empty() {
                    self.selected = (self.selected + 1) % self.results.len();
                }
                return KeyOutcome::Redraw;
            }
            Keysym::Up | Keysym::ISO_Left_Tab => {
                if !self.results.is_empty() {
                    self.selected = (self.selected + self.results.len() - 1) % self.results.len();
                }
                return KeyOutcome::Redraw;
            }
            Keysym::BackSpace if modifiers.ctrl => {
                // Delete the last word.
                let trimmed = self.query.trim_end().len();
                let start = self.query[..trimmed].rfind(' ').map_or(0, |i| i + 1);
                self.query.truncate(start);
            }
            Keysym::BackSpace => {
                self.query.pop();
            }
            _ => {
                let Some(text) = &event.utf8 else {
                    return KeyOutcome::Nothing;
                };
                if modifiers.ctrl || modifiers.alt || text.chars().any(char::is_control) {
                    return KeyOutcome::Nothing;
                }
                self.query.push_str(text);
            }
        }
        self.refresh(apps, counts, theme);
        KeyOutcome::Redraw
    }

    /// Hover selects; returns true if that changed anything.
    pub fn hover(&mut self, point: (f64, f64), theme: &Theme) -> bool {
        let layout = self.layout(theme);
        match layout.rows.iter().position(|r| r.contains(point)) {
            Some(row) if row != self.selected => {
                self.selected = row;
                true
            }
            _ => false,
        }
    }

    /// A click: the app under it, if any.
    pub fn click(&self, point: (f64, f64), theme: &Theme) -> Option<String> {
        let layout = self.layout(theme);
        let row = layout.rows.iter().position(|r| r.contains(point))?;
        self.results.get(row).cloned()
    }

    fn layout(&self, theme: &Theme) -> Layout {
        layout(theme, self.results.len(), self.query.is_empty())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        compositor: &CompositorState,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
        apps: &[AppEntry],
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let layout = self.layout(theme);
        let scale = self.scale;
        let (w, h) = (
            (layout.width * scale as f32) as i32,
            (layout.height * scale as f32) as i32,
        );
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate search buffer");
                return;
            }
        };
        let results: Vec<&AppEntry> = self
            .results
            .iter()
            .filter_map(|id| apps.iter().find(|a| &a.id == id))
            .collect();
        {
            let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
                return;
            };
            paint(
                &mut canvas,
                &layout,
                fonts,
                icons,
                theme,
                &self.query,
                &results,
                self.selected,
                scale as f32,
            );
        }
        // ARGB8888 is BGRA in memory; we drew RGBA.
        if format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }

        // Clicks outside the pill and card go to the overview underneath.
        let surface = self.layer.wl_surface();
        if let Ok(region) = Region::new(compositor) {
            for rect in std::iter::once(layout.pill).chain(layout.card) {
                region.add(rect.x as i32, rect.y as i32, rect.w as i32, rect.h as i32);
            }
            surface.set_input_region(Some(region.wl_region()));
        }
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            surface.commit();
        }
    }
}

/// Draw the pill and results. Separate from Wayland so tests can render it.
#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    query: &str,
    results: &[&AppEntry],
    selected: usize,
    scale: f32,
) {
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    let fg = theme.panel_foreground;

    // The pill: magnifier, then the query (or a hint) and a caret.
    let pill = layout.pill;
    canvas.rounded_rect(
        pill.x,
        pill.y,
        pill.w,
        pill.h,
        pill.h / 2.0,
        theme.search_background,
    );
    let icon = 18.0;
    let text_x = pill.x + 20.0 + icon + 12.0;
    canvas.icon(
        icons,
        "system-search-symbolic",
        pill.x + 20.0,
        pill.y + (pill.h - icon) / 2.0,
        icon,
        theme.qs_dim,
    );
    let max_text = pill.w - (text_x - pill.x) - 24.0;
    let (shown, color) = if query.is_empty() {
        ("Type to search", theme.qs_dim)
    } else {
        (query, fg)
    };
    // Long queries scroll: show the end, where you're typing.
    let mut text = fonts.layout(shown, theme.search_text_size, Weight::Normal, scale);
    let mut skip = 0;
    while !query.is_empty() && text.width > max_text && skip < shown.len() {
        skip = shown.ceil_char_boundary(skip + 1);
        text = fonts.layout(
            &shown[skip..],
            theme.search_text_size,
            Weight::Normal,
            scale,
        );
    }
    let text_y = pill.y + (pill.h - text.height) / 2.0;
    canvas.text(fonts, &mut text, text_x, text_y, color);
    let caret_x = if query.is_empty() {
        text_x
    } else {
        text_x + text.width + 1.0
    };
    let caret_h = theme.search_text_size * 1.3;
    canvas.rounded_rect(
        caret_x,
        pill.y + (pill.h - caret_h) / 2.0,
        2.0,
        caret_h,
        1.0,
        theme.accent,
    );

    // The results card.
    let Some(card) = layout.card else {
        return;
    };
    canvas.rounded_rect(
        card.x,
        card.y,
        card.w,
        card.h,
        theme.search_results_radius,
        theme.search_background,
    );
    if results.is_empty() {
        let mut none = fonts.layout("No apps found", 15.0, Weight::Normal, scale);
        let (x, y) = (
            card.x + (card.w - none.width) / 2.0,
            card.y + (card.h - none.height) / 2.0,
        );
        canvas.text(fonts, &mut none, x, y, theme.qs_dim);
        return;
    }
    for (i, (row, app)) in layout.rows.iter().zip(results).enumerate() {
        if i == selected {
            canvas.rounded_rect(
                row.x,
                row.y,
                row.w,
                row.h,
                theme.search_row_radius,
                theme.search_selected,
            );
        }
        let size = theme.search_icon_size;
        let icon_x = row.x + 12.0;
        let icon_y = row.y + (row.h - size) / 2.0;
        if !canvas.app_icon(icons, &app.icon, icon_x, icon_y, size) {
            canvas.icon(
                icons,
                "application-x-executable-symbolic",
                icon_x + size * 0.15,
                icon_y + size * 0.15,
                size * 0.7,
                theme.qs_dim,
            );
        }
        let x = icon_x + size + 14.0;
        let max = row.x + row.w - x - 12.0;
        let mut name = fit(fonts, &app.name, 15.0, Weight::Bold, scale, max);
        if app.description.is_empty() {
            let y = row.y + (row.h - name.height) / 2.0;
            canvas.text(fonts, &mut name, x, y, fg);
        } else {
            let mut description = fit(fonts, &app.description, 12.5, Weight::Normal, scale, max);
            let total = name.height + description.height;
            let top = row.y + (row.h - total) / 2.0;
            canvas.text(fonts, &mut name, x, top, fg);
            canvas.text(fonts, &mut description, x, top + name.height, theme.qs_dim);
        }
    }
}

/// Lay out `text`, cut short with "…" to fit `max` logical px.
fn fit(
    fonts: &mut Fonts,
    text: &str,
    size: f32,
    weight: Weight,
    scale: f32,
    max: f32,
) -> cnde_ui::TextLayout {
    let mut layout = fonts.layout(text, size, weight, scale);
    if layout.width <= max {
        return layout;
    }
    let chars: Vec<char> = text.chars().collect();
    // Start from a proportional guess, then trim until it fits.
    let mut keep = ((chars.len() as f32 * max / layout.width) as usize).min(chars.len());
    while keep > 0 {
        let candidate: String = chars[..keep]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
            + "…";
        layout = fonts.layout(&candidate, size, weight, scale);
        if layout.width <= max {
            break;
        }
        keep -= 1;
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_shows_only_the_pill() {
        let theme = Theme::dark();
        let l = layout(&theme, 0, true);
        assert!(l.card.is_none());
        assert!(l.rows.is_empty());
        // The surface is sized for the most results up front, so it never
        // has to be resized (and re-configured) while typing.
        assert_eq!(
            l.height,
            layout(&theme, theme.search_max_results, false).height
        );
    }

    #[test]
    fn rows_stack_inside_the_card() {
        let theme = Theme::dark();
        let l = layout(&theme, 3, false);
        let card = l.card.unwrap();
        assert_eq!(l.rows.len(), 3);
        let last = l.rows[2];
        assert!(last.y + last.h <= card.y + card.h);
        assert!(card.y + card.h <= l.height);
        assert!(l.rows[1].contains(((l.rows[1].x + 10.0) as f64, (l.rows[1].y + 5.0) as f64)));
    }

    /// `CNDE_SEARCH_PREVIEW=/path cargo test -p cnde-shell search_preview -- --ignored`
    /// renders the search at 2x as raw RGBA with a "WxH" header line.
    #[test]
    #[ignore]
    fn search_preview() {
        let Some(path) = std::env::var_os("CNDE_SEARCH_PREVIEW") else {
            return;
        };
        let theme = Theme::dark();
        let dirs = apps::application_dirs();
        let all = apps::load(&dirs);
        let query = std::env::var("CNDE_SEARCH_QUERY").unwrap_or_else(|_| "fi".into());
        let counts = std::collections::HashMap::new();
        let results = apps::search(&all, &query, &counts, theme.search_max_results);
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let layout = layout(&theme, results.len(), query.is_empty());
        let scale = 2.0;
        let (w, h) = (
            (layout.width * scale) as u32,
            (layout.height * scale) as u32,
        );
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
            paint(
                &mut canvas,
                &layout,
                &mut fonts,
                &mut icons,
                &theme,
                &query,
                &results,
                0,
                scale,
            );
        }
        let mut data = format!("{w}x{h}\n").into_bytes();
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }
}
