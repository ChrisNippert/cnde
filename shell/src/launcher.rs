//! The overview's search and app grid (ARCHITECTURE.md §5.2): a pill at the
//! top of the main monitor that takes the keyboard while the overview is
//! open. Typing shows matching apps in a card below it; Enter or a click
//! launches one. The grid button in the pill (or Super+A) shows every app
//! instead, a page at a time.
//!
//! The compositor owns the overview itself and tells us when it opens and
//! closes (see `ipc`); this surface only exists in between, and so does its
//! buffer pool: the grid is big, and its memory goes when the overview does.

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
    shm::{Shm, slot::SlotPool},
};
use tracing::warn;

use crate::{
    App,
    apps::{self, App as AppEntry},
};

/// Gap between the panel and the search pill.
const TOP_MARGIN: i32 = 20;
/// Space kept free below the grid card.
const BOTTOM_MARGIN: f32 = 40.0;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The pill, with results below while there's a query.
    Search,
    /// Every app, a page at a time.
    Grid,
}

/// Where everything goes, in logical pixels on the surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: f32,
    /// The surface is always this tall in a given mode; what's unused is
    /// transparent and lets clicks through.
    pub height: f32,
    pub pill: Rect,
    /// The "show all apps" button at the pill's right end.
    pub grid_button: Rect,
    /// Search results card; `None` while the query is empty.
    pub card: Option<Rect>,
    pub rows: Vec<Rect>,
    pub grid: Option<GridLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridLayout {
    pub card: Rect,
    /// Cells of the current page, left to right, top to bottom.
    pub cells: Vec<Rect>,
    pub columns: usize,
    pub per_page: usize,
    pub pages: usize,
    pub page: usize,
}

/// Everything the layout depends on.
pub struct LayoutInput {
    pub mode: Mode,
    pub results: usize,
    pub query_empty: bool,
    /// The monitor's logical size.
    pub output: (f32, f32),
    pub apps: usize,
    pub page: usize,
}

pub fn layout(theme: &Theme, input: &LayoutInput) -> Layout {
    let pad = 8.0;
    let search_height = theme.search_height
        + theme.search_gap
        + 2.0 * pad
        + theme.search_max_results as f32 * theme.search_row_height;
    let (width, height) = match input.mode {
        Mode::Search => (theme.search_width, search_height),
        Mode::Grid => {
            let max = (input.output.0 - 160.0)
                .min(theme.grid_max_width)
                .max(theme.search_width);
            let columns = ((max - 2.0 * theme.grid_padding) / theme.grid_cell_width)
                .floor()
                .max(1.0);
            let width = columns * theme.grid_cell_width + 2.0 * theme.grid_padding;
            let height =
                input.output.1 - theme.panel_height as f32 - TOP_MARGIN as f32 - BOTTOM_MARGIN;
            (width.max(theme.search_width), height.max(search_height))
        }
    };
    let pill = Rect {
        x: (width - theme.search_width) / 2.0,
        y: 0.0,
        w: theme.search_width,
        h: theme.search_height,
    };
    let button = theme.search_height - 12.0;
    let grid_button = Rect {
        x: pill.x + pill.w - 6.0 - button,
        y: pill.y + 6.0,
        w: button,
        h: button,
    };
    let below = pill.h + theme.search_gap;
    let mut out = Layout {
        width,
        height,
        pill,
        grid_button,
        card: None,
        rows: Vec::new(),
        grid: None,
    };

    if input.mode == Mode::Grid {
        let card = Rect {
            x: 0.0,
            y: below,
            w: width,
            h: height - below,
        };
        let inner_w = card.w - 2.0 * theme.grid_padding;
        // Room at the bottom for the page dots.
        let inner_h = card.h - 2.0 * theme.grid_padding - 16.0;
        let columns = ((inner_w / theme.grid_cell_width).floor() as usize).max(1);
        let rows = ((inner_h / theme.grid_cell_height).floor() as usize).max(1);
        let per_page = columns * rows;
        let pages = input.apps.div_ceil(per_page).max(1);
        let page = input.page.min(pages - 1);
        let on_page = input.apps.saturating_sub(page * per_page).min(per_page);
        // Centre the block of cells in the card.
        let used_w = columns as f32 * theme.grid_cell_width;
        let x0 = card.x + (card.w - used_w) / 2.0;
        let y0 = card.y
            + theme.grid_padding
            + (inner_h - rows as f32 * theme.grid_cell_height).max(0.0) / 2.0;
        let cells = (0..on_page)
            .map(|i| Rect {
                x: x0 + (i % columns) as f32 * theme.grid_cell_width,
                y: y0 + (i / columns) as f32 * theme.grid_cell_height,
                w: theme.grid_cell_width,
                h: theme.grid_cell_height,
            })
            .collect();
        out.grid = Some(GridLayout {
            card,
            cells,
            columns,
            per_page,
            pages,
            page,
        });
        return out;
    }

    if input.query_empty {
        return out;
    }
    out.rows = (0..input.results)
        .map(|i| Rect {
            x: pad,
            y: below + pad + i as f32 * theme.search_row_height,
            w: width - 2.0 * pad,
            h: theme.search_row_height,
        })
        .collect();
    // No results: a card just tall enough to say so.
    let inner = theme.search_row_height * input.results.max(1) as f32;
    out.card = Some(Rect {
        x: 0.0,
        y: below,
        w: width,
        h: inner + 2.0 * pad,
    });
    out
}

pub enum Outcome {
    Nothing,
    Redraw,
    /// Escape with nothing left to back out of.
    Close,
    Launch(String),
}

pub struct Launcher {
    pub output: WlOutput,
    pub layer: LayerSurface,
    pool: Option<SlotPool>,
    scale: i32,
    /// The monitor's logical size, for sizing the grid.
    output_size: (f32, f32),
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    mode: Mode,
    /// The surface size last requested, to notice mode changes.
    size: (u32, u32),
    query: String,
    /// Desktop file IDs of the current matches, best first.
    results: Vec<String>,
    selected: usize,
    /// Grid: index into all apps, and the page showing.
    grid_selected: usize,
    page: usize,
    /// Scroll-wheel travel not yet turned into page flips.
    scroll: f64,
}

impl Launcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        shm: &Shm,
        qh: &QueueHandle<App>,
        output: WlOutput,
        output_size: (f32, f32),
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
        layer.set_anchor(Anchor::TOP);
        layer.set_margin(TOP_MARGIN, 0, 0, 0);
        // Typing goes straight here, the moment the overview opens.
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.wl_surface().set_buffer_scale(scale);
        let pool = SlotPool::new(1024 * 1024, shm)
            .map_err(|err| warn!(?err, "can't create the search buffer pool"))
            .ok();
        let mut launcher = Self {
            output,
            layer,
            pool,
            scale: scale.max(1),
            output_size,
            configured: false,
            frame_pending: false,
            dirty: false,
            mode: Mode::Search,
            size: (0, 0),
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            grid_selected: 0,
            page: 0,
            scroll: 0.0,
        };
        launcher.request_size(theme, 0);
        launcher.layer.commit();
        launcher
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

    /// Show every app, or go back to the search pill.
    pub fn toggle_grid(&mut self, theme: &Theme, apps: usize) {
        self.mode = match self.mode {
            Mode::Search => Mode::Grid,
            Mode::Grid => Mode::Search,
        };
        self.query.clear();
        self.results.clear();
        self.grid_selected = 0;
        self.page = 0;
        self.request_size(theme, apps);
    }

    fn layout(&self, theme: &Theme, apps: usize) -> Layout {
        layout(
            theme,
            &LayoutInput {
                mode: self.mode,
                results: self.results.len(),
                query_empty: self.query.is_empty(),
                output: self.output_size,
                apps,
                page: self.page,
            },
        )
    }

    /// Ask for the size the current mode needs (sent with the next commit).
    fn request_size(&mut self, theme: &Theme, apps: usize) {
        let layout = self.layout(theme, apps);
        let size = (layout.width as u32, layout.height as u32);
        if size != self.size {
            self.size = size;
            self.layer.set_size(size.0, size.1);
        }
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
        self.grid_selected = self.grid_selected.min(apps.len().saturating_sub(1));
    }

    pub fn key(
        &mut self,
        event: &KeyEvent,
        modifiers: &Modifiers,
        apps: &[AppEntry],
        counts: &apps::LaunchCounts,
        theme: &Theme,
    ) -> Outcome {
        if self.mode == Mode::Grid
            && let Some(outcome) = self.grid_key(event, apps, theme)
        {
            return outcome;
        }
        match event.keysym {
            Keysym::Escape if self.query.is_empty() => return Outcome::Close,
            Keysym::Escape => self.query.clear(),
            Keysym::Return | Keysym::KP_Enter => {
                return match self.results.get(self.selected) {
                    Some(id) => Outcome::Launch(id.clone()),
                    None => Outcome::Nothing,
                };
            }
            Keysym::Down | Keysym::Tab => {
                if !self.results.is_empty() {
                    self.selected = (self.selected + 1) % self.results.len();
                }
                return Outcome::Redraw;
            }
            Keysym::Up | Keysym::ISO_Left_Tab => {
                if !self.results.is_empty() {
                    self.selected = (self.selected + self.results.len() - 1) % self.results.len();
                }
                return Outcome::Redraw;
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
                    return Outcome::Nothing;
                };
                if modifiers.ctrl || modifiers.alt || text.chars().any(char::is_control) {
                    return Outcome::Nothing;
                }
                self.query.push_str(text);
            }
        }
        self.refresh(apps, counts, theme);
        Outcome::Redraw
    }

    /// Keys the grid handles itself; `None` passes the key on to search
    /// (typing in the grid starts a search, like GNOME).
    fn grid_key(&mut self, event: &KeyEvent, apps: &[AppEntry], theme: &Theme) -> Option<Outcome> {
        let grid = self.layout(theme, apps.len()).grid?;
        let last = apps.len().saturating_sub(1);
        let columns = grid.columns;
        let next = match event.keysym {
            Keysym::Escape => {
                self.toggle_grid(theme, apps.len());
                return Some(Outcome::Redraw);
            }
            Keysym::Return | Keysym::KP_Enter => {
                return Some(match apps.get(self.grid_selected) {
                    Some(app) => Outcome::Launch(app.id.clone()),
                    None => Outcome::Nothing,
                });
            }
            Keysym::Right | Keysym::Tab => self.grid_selected + 1,
            Keysym::Left | Keysym::ISO_Left_Tab => self.grid_selected.saturating_sub(1),
            Keysym::Down => self.grid_selected + columns,
            Keysym::Up => self.grid_selected.saturating_sub(columns),
            Keysym::Page_Down => self.grid_selected + grid.per_page,
            Keysym::Page_Up => self.grid_selected.saturating_sub(grid.per_page),
            _ => {
                // Printable: switch to search, then let it take the key.
                let printable = event
                    .utf8
                    .as_ref()
                    .is_some_and(|t| !t.chars().any(char::is_control));
                if printable {
                    self.toggle_grid(theme, apps.len());
                }
                return None;
            }
        };
        self.grid_selected = next.min(last);
        self.page = self.grid_selected / grid.per_page.max(1);
        Some(Outcome::Redraw)
    }

    /// Hover selects; returns true if that changed anything.
    pub fn hover(&mut self, point: (f64, f64), theme: &Theme, apps: usize) -> bool {
        let layout = self.layout(theme, apps);
        if let Some(grid) = &layout.grid {
            if let Some(i) = grid.cells.iter().position(|r| r.contains(point)) {
                let index = grid.page * grid.per_page + i;
                if index != self.grid_selected {
                    self.grid_selected = index;
                    return true;
                }
            }
            return false;
        }
        match layout.rows.iter().position(|r| r.contains(point)) {
            Some(row) if row != self.selected => {
                self.selected = row;
                true
            }
            _ => false,
        }
    }

    /// A click.
    pub fn click(&mut self, point: (f64, f64), theme: &Theme, apps: &[AppEntry]) -> Outcome {
        let layout = self.layout(theme, apps.len());
        if layout.grid_button.contains(point) {
            self.toggle_grid(theme, apps.len());
            return Outcome::Redraw;
        }
        if let Some(grid) = &layout.grid {
            return grid
                .cells
                .iter()
                .position(|r| r.contains(point))
                .and_then(|i| apps.get(grid.page * grid.per_page + i))
                .map_or(Outcome::Nothing, |app| Outcome::Launch(app.id.clone()));
        }
        layout
            .rows
            .iter()
            .position(|r| r.contains(point))
            .and_then(|row| self.results.get(row))
            .map_or(Outcome::Nothing, |id| Outcome::Launch(id.clone()))
    }

    /// The scroll wheel flips grid pages (one notch, one page). Returns
    /// true if the page changed.
    pub fn scroll(&mut self, notches: f64, theme: &Theme, apps: usize) -> bool {
        let Some(grid) = self.layout(theme, apps).grid else {
            return false;
        };
        self.scroll += notches;
        let mut changed = false;
        while self.scroll.abs() >= 1.0 {
            let step = self.scroll.signum();
            self.scroll -= step;
            let page = (self.page as isize + step as isize).clamp(0, grid.pages as isize - 1);
            if page as usize != self.page {
                self.page = page as usize;
                self.grid_selected = self.page * grid.per_page;
                changed = true;
            }
        }
        changed
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        compositor: &CompositorState,
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
        self.request_size(theme, apps.len());
        let layout = self.layout(theme, apps.len());
        let scale = self.scale;
        let (w, h) = (
            (layout.width * scale as f32) as i32,
            (layout.height * scale as f32) as i32,
        );
        let Some(pool) = &mut self.pool else {
            return;
        };
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
            let state = PaintState {
                query: &self.query,
                results: &results,
                selected: self.selected,
                apps,
                grid_selected: self.grid_selected,
            };
            paint(
                &mut canvas,
                &layout,
                fonts,
                icons,
                theme,
                &state,
                scale as f32,
            );
        }
        // ARGB8888 is BGRA in memory; we drew RGBA.
        if format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }

        // Clicks outside the drawn parts go to the overview underneath.
        let surface = self.layer.wl_surface();
        if let Ok(region) = Region::new(compositor) {
            let card = layout.grid.as_ref().map(|g| g.card).or(layout.card);
            for rect in std::iter::once(layout.pill).chain(card) {
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

struct PaintState<'a> {
    query: &'a str,
    results: &'a [&'a AppEntry],
    selected: usize,
    /// All apps, for the grid.
    apps: &'a [AppEntry],
    grid_selected: usize,
}

/// Draw everything. Separate from Wayland so tests can render it.
fn paint(
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    state: &PaintState<'_>,
    scale: f32,
) {
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    let fg = theme.panel_foreground;
    paint_pill(canvas, layout, fonts, icons, theme, state.query, scale);
    if let Some(grid) = &layout.grid {
        paint_grid(canvas, grid, fonts, icons, theme, state, scale);
        return;
    }

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
    if state.results.is_empty() {
        let mut none = fonts.layout("No apps found", 15.0, Weight::Normal, scale);
        let (x, y) = (
            card.x + (card.w - none.width) / 2.0,
            card.y + (card.h - none.height) / 2.0,
        );
        canvas.text(fonts, &mut none, x, y, theme.qs_dim);
        return;
    }
    for (i, (row, app)) in layout.rows.iter().zip(state.results).enumerate() {
        if i == state.selected {
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
        app_icon(canvas, icons, theme, &app.icon, icon_x, icon_y, size);
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

/// The pill: magnifier, the query (or a hint) and a caret, and the grid
/// button at the right end.
fn paint_pill(
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    query: &str,
    scale: f32,
) {
    let fg = theme.panel_foreground;
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
    let button = layout.grid_button;
    let max_text = button.x - text_x - 12.0;
    let grid_open = layout.grid.is_some();
    let (shown, color) = if !query.is_empty() {
        (query, fg)
    } else if grid_open {
        ("Type to search apps", theme.qs_dim)
    } else {
        ("Type to search", theme.qs_dim)
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

    let background = if grid_open {
        theme.qs_toggle_on
    } else {
        theme.qs_toggle_off
    };
    canvas.circle(
        button.x + button.w / 2.0,
        button.y + button.h / 2.0,
        button.w / 2.0,
        background,
    );
    let size = 18.0;
    canvas.icon(
        icons,
        "view-app-grid-symbolic",
        button.x + (button.w - size) / 2.0,
        button.y + (button.h - size) / 2.0,
        size,
        fg,
    );
}

fn paint_grid(
    canvas: &mut Canvas<'_>,
    grid: &GridLayout,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    state: &PaintState<'_>,
    scale: f32,
) {
    let card = grid.card;
    canvas.rounded_rect(
        card.x,
        card.y,
        card.w,
        card.h,
        theme.search_results_radius,
        theme.search_background,
    );
    let first = grid.page * grid.per_page;
    for (i, cell) in grid.cells.iter().enumerate() {
        let Some(app) = state.apps.get(first + i) else {
            break;
        };
        if first + i == state.grid_selected {
            canvas.rounded_rect(
                cell.x + 6.0,
                cell.y + 4.0,
                cell.w - 12.0,
                cell.h - 8.0,
                theme.search_row_radius,
                theme.search_selected,
            );
        }
        let size = theme.grid_icon_size;
        let icon_x = cell.x + (cell.w - size) / 2.0;
        let icon_y = cell.y + 16.0;
        app_icon(canvas, icons, theme, &app.icon, icon_x, icon_y, size);
        let mut name = fit(fonts, &app.name, 13.0, Weight::Normal, scale, cell.w - 16.0);
        let x = (cell.x + (cell.w - name.width) / 2.0).round();
        canvas.text(
            fonts,
            &mut name,
            x,
            icon_y + size + 10.0,
            theme.panel_foreground,
        );
    }
    // Page dots.
    if grid.pages > 1 {
        let dot = 7.0;
        let gap = 8.0;
        let total = grid.pages as f32 * dot + (grid.pages - 1) as f32 * gap;
        let mut x = card.x + (card.w - total) / 2.0;
        let y = card.y + card.h - theme.grid_padding / 2.0 - dot;
        for page in 0..grid.pages {
            let color = if page == grid.page {
                theme.workspace_active
            } else {
                theme.workspace_dot
            };
            canvas.circle(x + dot / 2.0, y + dot / 2.0, dot / 2.0, color);
            x += dot + gap;
        }
    }
}

/// An app's icon, or a generic one if its theme has none.
fn app_icon(
    canvas: &mut Canvas<'_>,
    icons: &mut Icons,
    theme: &Theme,
    name: &str,
    x: f32,
    y: f32,
    size: f32,
) {
    if !canvas.app_icon(icons, name, x, y, size) {
        canvas.icon(
            icons,
            "application-x-executable-symbolic",
            x + size * 0.15,
            y + size * 0.15,
            size * 0.7,
            theme.qs_dim,
        );
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

    fn input(mode: Mode, results: usize, query_empty: bool, apps: usize) -> LayoutInput {
        LayoutInput {
            mode,
            results,
            query_empty,
            output: (2560.0, 1440.0),
            apps,
            page: 0,
        }
    }

    #[test]
    fn empty_query_shows_only_the_pill() {
        let theme = Theme::dark();
        let l = layout(&theme, &input(Mode::Search, 0, true, 50));
        assert!(l.card.is_none());
        assert!(l.rows.is_empty());
        assert!(l.grid.is_none());
        // The surface is sized for the most results up front, so it never
        // has to be resized (and re-configured) while typing.
        let full = layout(
            &theme,
            &input(Mode::Search, theme.search_max_results, false, 50),
        );
        assert_eq!(l.height, full.height);
        // The grid button sits inside the pill.
        assert!(l.grid_button.x + l.grid_button.w <= l.pill.x + l.pill.w);
    }

    #[test]
    fn rows_stack_inside_the_card() {
        let theme = Theme::dark();
        let l = layout(&theme, &input(Mode::Search, 3, false, 50));
        let card = l.card.unwrap();
        assert_eq!(l.rows.len(), 3);
        let last = l.rows[2];
        assert!(last.y + last.h <= card.y + card.h);
        assert!(card.y + card.h <= l.height);
    }

    #[test]
    fn grid_pages_fit_the_card() {
        let theme = Theme::dark();
        let l = layout(&theme, &input(Mode::Grid, 0, true, 200));
        let grid = l.grid.unwrap();
        assert!(grid.pages >= 2, "200 apps don't fit one page");
        assert_eq!(grid.cells.len(), grid.per_page);
        for cell in &grid.cells {
            assert!(cell.x >= grid.card.x && cell.x + cell.w <= grid.card.x + grid.card.w);
            assert!(cell.y + cell.h <= grid.card.y + grid.card.h);
        }
        // The pill stays centred over the wider grid.
        assert!((l.pill.x + l.pill.w / 2.0 - l.width / 2.0).abs() < 0.5);
        // The last page holds just the rest.
        let mut last = input(Mode::Grid, 0, true, 200);
        last.page = 99;
        let grid_last = layout(&theme, &last).grid.unwrap();
        assert_eq!(grid_last.page, grid_last.pages - 1);
        assert_eq!(
            grid_last.cells.len(),
            200 - (grid_last.pages - 1) * grid_last.per_page
        );
    }

    /// `CNDE_SEARCH_PREVIEW=/path cargo test -p cnde-shell search_preview -- --ignored`
    /// renders the search at 1x as raw RGBA with a "WxH" header line.
    /// `CNDE_SEARCH_QUERY` sets the query; `CNDE_SEARCH_GRID` shows the grid.
    #[test]
    #[ignore]
    fn search_preview() {
        let Some(path) = std::env::var_os("CNDE_SEARCH_PREVIEW") else {
            return;
        };
        let theme = Theme::dark();
        let dirs = apps::application_dirs();
        let all = apps::load(&dirs);
        let grid = std::env::var_os("CNDE_SEARCH_GRID").is_some();
        let query = if grid {
            String::new()
        } else {
            std::env::var("CNDE_SEARCH_QUERY").unwrap_or_else(|_| "fi".into())
        };
        let counts = std::collections::HashMap::new();
        let results = apps::search(&all, &query, &counts, theme.search_max_results);
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let mode = if grid { Mode::Grid } else { Mode::Search };
        let layout = layout(
            &theme,
            &LayoutInput {
                mode,
                results: results.len(),
                query_empty: query.is_empty(),
                output: (1920.0, 1080.0),
                apps: all.len(),
                page: 0,
            },
        );
        let scale = 1.0;
        let (w, h) = (
            (layout.width * scale) as u32,
            (layout.height * scale) as u32,
        );
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
            let state = PaintState {
                query: &query,
                results: &results,
                selected: 0,
                apps: &all,
                grid_selected: 0,
            };
            paint(
                &mut canvas,
                &layout,
                &mut fonts,
                &mut icons,
                &theme,
                &state,
                scale,
            );
        }
        let mut data = format!("{w}x{h}\n").into_bytes();
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }
}
