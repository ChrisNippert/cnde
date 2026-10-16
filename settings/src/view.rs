//! Painting the window: sidebar, header bars and the current page. Every
//! paint also records where each control ended up ([`Target`]s), which is
//! what the pointer is tested against until the next paint.

use std::collections::HashSet;

use cnde_config::Config;
use cnde_ui::{
    Animator, Canvas, Color, Fonts, Icons, Painter, Rect, Theme, Weight,
    anim::{MEDIUM, SHORT},
};

use crate::{
    pages::{self, Control, Display, Key, Page, Row, Value},
    wallpapers::Thumb,
};

/// Something on screen the pointer can hit. Row indices are into
/// [`Painted::rows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Page(Page),
    Minimize,
    Maximize,
    Close,
    /// Empty header bar space: drag to move the window.
    Header,
    /// A whole row; clicking it works its control (switches, dropdowns).
    Row(usize),
    Switch(usize),
    Segment(usize, usize),
    Dropdown(usize),
    /// A choice in the open dropdown.
    Choice(usize),
    Slider(usize),
    Entry(usize),
    Reset(usize),
    /// A wallpaper tile; `None` is the built-in one.
    Wallpaper(Option<usize>),
    Advanced,
}

/// What's animating.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnimKey {
    /// A hover highlight fading in or out.
    Hover(Target),
    /// The sidebar's highlight on the current page.
    Selected(Page),
    Switch(Key),
    Slider(Key),
    /// Where a segmented control's highlight is.
    Segment(Key),
    /// The page fading in after switching to it.
    Page,
    /// The open dropdown appearing.
    Popover,
}

pub type Anim = Animator<AnimKey>;

/// How lit `target`'s hover highlight is (0-1), fading as the pointer
/// comes and goes.
fn hover(a: &mut Anim, ui: &Ui, target: Target) -> f32 {
    let on = if ui.hover == Some(target) { 1.0 } else { 0.0 };
    a.get(AnimKey::Hover(target), on, SHORT)
}

/// A text entry being edited.
pub struct Edit {
    pub key: Key,
    pub text: String,
    /// In chars.
    pub cursor: usize,
    pub int: bool,
}

/// Everything that decides what the window looks like.
pub struct Ui {
    pub config: Config,
    pub displays: Vec<Display>,
    pub thumbs: Vec<Thumb>,
    pub page: Page,
    /// Pages showing their advanced settings.
    pub advanced: HashSet<Page>,
    pub scroll: f32,
    pub hover: Option<Target>,
    /// The dropdown that's open.
    pub popover: Option<Key>,
    pub edit: Option<Edit>,
    /// Shown at the bottom until it times out.
    pub error: Option<String>,
    /// Square corners when maximized or tiled.
    pub rounded: bool,
    /// The slider being dragged (it follows the pointer, unanimated).
    pub dragging: Option<Key>,
}

/// The result of a paint, for hit-testing.
#[derive(Default)]
pub struct Painted {
    pub hits: Vec<(Rect, Target)>,
    pub rows: Vec<Row>,
    /// Where each row's slider is (for dragging).
    pub sliders: Vec<(usize, Rect)>,
    /// How far the page can scroll.
    pub max_scroll: f32,
}

impl Painted {
    /// The topmost target under `point`.
    pub fn hit(&self, point: (f64, f64)) -> Option<Target> {
        self.hits
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, t)| *t)
    }
}

pub struct Res {
    pub fonts: Fonts,
    pub icons: Icons,
    pub theme: Theme,
    pub anim: Anim,
}

const CORNER: f32 = 14.0;
const MAX_COLUMN: f32 = 680.0;
const GROUP_GAP: f32 = 20.0;
const SLIDER_WIDTH: f32 = 200.0;
const ENTRY_WIDTH: f32 = 240.0;

pub fn paint(ui: &Ui, res: &mut Res, canvas: &mut Canvas<'_>) -> Painted {
    let (w, h) = canvas.size();
    let theme = res.theme.clone();
    let t = &theme;
    let mut out = Painted::default();
    let a = &mut res.anim;
    a.begin();
    let mut p = Painter {
        canvas,
        fonts: &mut res.fonts,
        icons: &mut res.icons,
        theme: t,
    };
    let radius = if ui.rounded { CORNER } else { 0.0 };
    let sw = t.sidebar_width;
    let hh = t.window_header_height;

    p.canvas.clear(Color::rgba(0, 0, 0, 0.0));
    p.canvas
        .rounded_rect(0.0, 0.0, w, h, radius, t.window_background);

    // The page first: it scrolls under the header bar drawn after it.
    let content = Rect::new(sw, hh, w - sw, h - hh);
    // A new page fades in, rising a little.
    let appear = a.get(AnimKey::Page, 1.0, MEDIUM);
    page(ui, a, &mut p, &mut out, content, 10.0 * (1.0 - appear));
    if appear < 1.0 {
        let veil = t.window_background.with_alpha(1.0 - appear);
        p.canvas
            .rounded_rect(content.x, content.y, content.w, content.h, 0.0, veil);
    }

    // Content header bar (the page scrolls under it).
    p.canvas
        .rounded_rect(sw, 0.0, w - sw - radius, hh, 0.0, t.window_background);
    p.canvas.rounded_rect(
        w - radius * 2.0,
        0.0,
        radius * 2.0,
        hh,
        radius,
        t.window_background,
    );
    out.hits
        .push((Rect::new(sw, 0.0, w - sw, hh), Target::Header));
    // The page's title on the left, the window buttons on the right.
    p.label_v(
        ui.page.title(),
        sw + 14.0,
        hh / 2.0 + 2.0,
        w - sw - 140.0,
        t.window_title_size + 4.0,
        Weight::Bold,
        t.window_text,
    );
    let size = 22.0;
    let maximize_icon = if ui.rounded {
        "window-maximize-symbolic"
    } else {
        "window-restore-symbolic"
    };
    for (i, (target, icon)) in [
        (Target::Close, "window-close-symbolic"),
        (Target::Maximize, maximize_icon),
        (Target::Minimize, "window-minimize-symbolic"),
    ]
    .into_iter()
    .enumerate()
    {
        let x = w - 10.0 - size - i as f32 * (size + 8.0);
        let button = Rect::new(x, (hh - size) / 2.0, size, size);
        let fill = t
            .button_background
            .mix(t.button_hover, hover(a, ui, target));
        p.canvas.circle(
            button.x + size / 2.0,
            button.y + size / 2.0,
            size / 2.0,
            fill,
        );
        p.icon(
            icon,
            button.x + size / 2.0,
            button.y + size / 2.0,
            14.0,
            t.window_text,
        );
        out.hits.push((button, target));
    }
    if ui.scroll > 0.5 {
        p.canvas
            .rounded_rect(sw, hh - 1.0, w - sw, 1.0, 0.0, t.card_separator);
    }

    sidebar(ui, a, &mut p, &mut out, h);

    if let Some(key) = &ui.popover {
        popover(ui, a, &mut p, &mut out, key, w, h);
    }
    if let Some(error) = &ui.error {
        let size = t.font_size;
        let tw = p.measure(error, size, Weight::Normal).min(w - sw - 48.0);
        let bw = tw + 32.0;
        let (bx, by) = (sw + (w - sw - bw) / 2.0, h - 24.0 - 40.0);
        p.canvas
            .rounded_rect(bx, by, bw, 40.0, 20.0, Color::rgba(0x10, 0x10, 0x12, 0.96));
        p.label_v(
            error,
            bx + 16.0,
            by + 20.0,
            tw,
            size,
            Weight::Normal,
            t.error,
        );
    }
    a.end();
    out
}

/// The sidebar floats as a card inset from the window's edges, its
/// highlight inside it.
const SIDEBAR_INSET: f32 = 6.0;

fn sidebar(ui: &Ui, a: &mut Anim, p: &mut Painter<'_, '_>, out: &mut Painted, h: f32) {
    let t = p.theme;
    let sw = t.sidebar_width;
    let hh = t.window_header_height;
    let inset = SIDEBAR_INSET;
    p.canvas.rounded_rect(
        inset,
        inset,
        sw - 2.0 * inset,
        h - 2.0 * inset,
        CORNER - inset / 2.0,
        t.window_sidebar,
    );
    out.hits.push((Rect::new(0.0, 0.0, sw, hh), Target::Header));
    let title = "Settings";
    p.label_v(
        title,
        18.0,
        hh / 2.0,
        sw,
        t.window_title_size,
        Weight::Bold,
        t.window_text,
    );
    let mut y = hh + 6.0;
    for page in Page::ALL {
        if y + t.sidebar_row_height > h {
            break;
        }
        let rect = Rect::new(
            inset + 6.0,
            y,
            sw - 2.0 * (inset + 6.0),
            t.sidebar_row_height,
        );
        let selected = a.get(
            AnimKey::Selected(page),
            if ui.page == page { 1.0 } else { 0.0 },
            SHORT,
        );
        let hovered = hover(a, ui, Target::Page(page)) * (1.0 - selected);
        // The current page: tinted with the accent, its icon in the accent.
        let chosen = t.accent.with_alpha(0.22);
        for (color, amount) in [(t.hover, hovered), (chosen, selected)] {
            if amount > 0.0 {
                let fill = color.with_alpha(color.a * amount);
                p.canvas
                    .rounded_rect(rect.x, rect.y, rect.w, rect.h, 10.0, fill);
            }
        }
        let cy = rect.y + rect.h / 2.0;
        let icon_color = t.window_text.mix(t.accent, selected);
        p.icon(page.icon(), rect.x + 20.0, cy, 16.0, icon_color);
        p.label_v(
            page.title(),
            rect.x + 40.0,
            cy,
            rect.w - 48.0,
            t.font_size,
            Weight::Normal,
            t.window_text,
        );
        out.hits.push((rect, Target::Page(page)));
        y += t.sidebar_row_height + 2.0;
    }
}

fn page(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    out: &mut Painted,
    area: Rect,
    offset: f32,
) {
    let t = p.theme;
    let groups = pages::build(ui.page, &ui.config, &ui.displays);
    let show_advanced = ui.advanced.contains(&ui.page);
    let has_advanced = groups.iter().flat_map(|g| &g.rows).any(|r| r.advanced);
    let col_w = (area.w - 48.0).min(MAX_COLUMN);
    let x = area.x + (area.w - col_w) / 2.0;
    let top = area.y + 16.0;
    let mut y = top - ui.scroll + offset;

    for group in groups {
        let rows: Vec<Row> = group
            .rows
            .into_iter()
            .filter(|r| show_advanced || !r.advanced)
            .collect();
        if rows.is_empty() && group.description.is_empty() {
            continue;
        }
        if !group.title.is_empty() {
            p.label(
                &group.title.to_uppercase(),
                x + t.row_padding,
                y + 2.0,
                t.subtitle_size - 1.5,
                Weight::Bold,
                t.window_dim,
            );
            y += 24.0;
        }
        if !group.description.is_empty() {
            p.label_v(
                &group.description,
                x + 4.0,
                y + 8.0,
                col_w,
                t.subtitle_size,
                Weight::Normal,
                t.window_dim,
            );
            y += 26.0;
        }
        if rows.is_empty() {
            y += GROUP_GAP - 12.0;
            continue;
        }
        let heights: Vec<f32> = rows.iter().map(|r| row_height(ui, r, t, col_w)).collect();
        let card_h: f32 = heights.iter().sum();
        p.canvas
            .rounded_rect(x, y, col_w, card_h, t.card_radius, t.card_background);
        let mut ry = y;
        let count = rows.len();
        for (i, (row, rh)) in rows.into_iter().zip(heights).enumerate() {
            if i > 0 {
                p.canvas.rounded_rect(
                    x + t.row_padding,
                    ry,
                    col_w - 2.0 * t.row_padding,
                    1.0,
                    0.0,
                    t.card_separator,
                );
            }
            let index = out.rows.len();
            let rect = Rect::new(x, ry, col_w, rh);
            // Off-screen rows are skipped, but still counted for scrolling.
            if rect.bottom() > area.y && rect.y < area.bottom() {
                paint_row(ui, a, p, out, &row, index, rect, i == 0, i + 1 == count);
            }
            out.rows.push(row);
            ry += rh;
        }
        y += card_h + GROUP_GAP;
    }

    if has_advanced {
        let rect = Rect::new(x, y, col_w, t.row_height);
        let hovered = hover(a, ui, Target::Advanced);
        p.canvas
            .rounded_rect(x, y, col_w, rect.h, t.card_radius, t.card_background);
        if hovered > 0.0 {
            let fill = t.hover.with_alpha(t.hover.a * hovered);
            p.canvas
                .rounded_rect(x, y, col_w, rect.h, t.card_radius, fill);
        }
        p.label_v(
            "Advanced settings",
            x + t.row_padding,
            y + rect.h / 2.0,
            col_w - 100.0,
            t.font_size,
            Weight::Normal,
            t.window_text,
        );
        let sx = rect.right() - t.row_padding - t.switch_width;
        let on = a.get(
            AnimKey::Switch(Key::Path("advanced")),
            if show_advanced { 1.0 } else { 0.0 },
            SHORT,
        );
        p.switch(sx, y + (rect.h - t.switch_height) / 2.0, on);
        out.hits.push((rect, Target::Advanced));
        y += rect.h + GROUP_GAP;
    }

    let content_h = y + ui.scroll - offset - top + 24.0 - GROUP_GAP;
    out.max_scroll = (content_h - area.h + 24.0).max(0.0);
}

fn row_height(ui: &Ui, row: &Row, t: &Theme, col_w: f32) -> f32 {
    if let Control::Wallpapers = row.control {
        let (cols, _, tile_h) = wallpaper_grid(col_w, t);
        let count = ui.thumbs.len() + 1;
        let lines = count.div_ceil(cols);
        return t.row_padding * 2.0 + lines as f32 * (tile_h + 12.0) - 12.0;
    }
    if row.subtitle.is_empty() {
        t.row_height
    } else {
        t.row_subtitle_height
    }
}

/// Columns, tile width and tile height for a card `col_w` wide.
fn wallpaper_grid(col_w: f32, t: &Theme) -> (usize, f32, f32) {
    let inner = col_w - t.row_padding * 2.0;
    let gap = 12.0;
    let cols = (((inner + gap) / (130.0 + gap)).floor() as usize).max(1);
    let tile_w = (inner - gap * (cols - 1) as f32) / cols as f32;
    (cols, tile_w, tile_w * 10.0 / 16.0)
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    out: &mut Painted,
    row: &Row,
    index: usize,
    rect: Rect,
    first: bool,
    last: bool,
) {
    let t = p.theme;
    if let Control::Wallpapers = row.control {
        wallpapers(ui, a, p, out, row, rect);
        return;
    }
    let activatable = matches!(row.control, Control::Switch { .. } | Control::Dropdown(_));
    let row_hover = if activatable {
        hover(a, ui, Target::Row(index))
    } else {
        0.0
    };
    if row_hover > 0.0 {
        // Opaque, so squaring off the corners inside the card can overdraw.
        let color = over(
            over(t.window_background, t.card_background),
            t.hover.with_alpha(t.hover.a * row_hover),
        );
        let Rect { x, y, w, h } = rect;
        p.canvas.rounded_rect(x, y, w, h, t.card_radius, color);
        if !first {
            p.canvas.rounded_rect(x, y, w, h / 2.0, 0.0, color);
        }
        if !last {
            p.canvas
                .rounded_rect(x, y + h / 2.0, w, h / 2.0, 0.0, color);
        }
    }
    if activatable {
        out.hits.push((rect, Target::Row(index)));
    }
    let cy = rect.y + rect.h / 2.0;
    let right = rect.right() - t.row_padding;

    // The control, right-aligned; returns its left edge.
    let control_x = match &row.control {
        Control::Switch { .. } => {
            let x = right - t.switch_width;
            let on = a.get(
                AnimKey::Switch(row.key.clone()),
                if row.is_on() { 1.0 } else { 0.0 },
                SHORT,
            );
            let r = p.switch(x, cy - t.switch_height / 2.0, on);
            out.hits.push((r, Target::Switch(index)));
            x
        }
        Control::Segmented(choices) => {
            let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
            let x = right - p.segmented_width(&labels);
            let selected = choices
                .iter()
                .position(|c| c.value == row.value)
                .map(|i| a.get(AnimKey::Segment(row.key.clone()), i as f32, SHORT));
            let hovers: Vec<f32> = (0..labels.len())
                .map(|j| hover(a, ui, Target::Segment(index, j)))
                .collect();
            let rects = p.segmented(x, cy - t.button_height / 2.0, &labels, selected, |j| {
                hovers[j]
            });
            for (j, r) in rects.into_iter().enumerate() {
                out.hits.push((r, Target::Segment(index, j)));
            }
            x
        }
        Control::Dropdown(choices) => {
            let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
            let w = p.dropdown_width(&labels).min(280.0);
            let r = Rect::new(right - w, cy - t.button_height / 2.0, w, t.button_height);
            let label = choices
                .iter()
                .find(|c| c.value == row.value)
                .map_or("", |c| c.label.as_str());
            let lit = hover(a, ui, Target::Dropdown(index)).max(row_hover);
            p.dropdown(r, label, lit);
            out.hits.push((r, Target::Dropdown(index)));
            r.x
        }
        Control::Slider {
            min, max, format, ..
        } => {
            let r = Rect::new(right - SLIDER_WIDTH, cy - 14.0, SLIDER_WIDTH, 28.0);
            let v = row.value.as_f64().unwrap_or(*min);
            let fraction = ((v - min) / (max - min)) as f32;
            let key = AnimKey::Slider(row.key.clone());
            // Glides to a clicked spot; follows the pointer while dragged.
            let shown = if ui.dragging.as_ref() == Some(&row.key) {
                a.set(key, fraction)
            } else {
                a.get(key, fraction, SHORT)
            };
            p.scale(r, shown);
            out.hits.push((r, Target::Slider(index)));
            out.sliders.push((index, r));
            let text = format(v);
            let tw = p.measure(&text, t.subtitle_size, Weight::Normal);
            p.label_v(
                &text,
                r.x - 8.0 - tw,
                cy,
                tw + 1.0,
                t.subtitle_size,
                Weight::Normal,
                t.window_dim,
            );
            r.x - 8.0 - tw.max(40.0)
        }
        Control::Text { placeholder, .. } => {
            let r = Rect::new(
                right - ENTRY_WIDTH,
                cy - t.button_height / 2.0,
                ENTRY_WIDTH,
                t.button_height,
            );
            match &ui.edit {
                Some(edit) if edit.key == row.key => {
                    p.entry(r, &edit.text, placeholder, Some(edit.cursor), false)
                }
                _ => {
                    let text = match &row.value {
                        Value::Int(v) => v.to_string(),
                        Value::Float(v) => v.to_string(),
                        other => other.as_str().to_string(),
                    };
                    p.entry(r, &text, placeholder, None, false);
                }
            }
            out.hits.push((r, Target::Entry(index)));
            r.x
        }
        Control::Wallpapers => unreachable!(),
    };

    let mut text_right = control_x - 12.0;
    if row.changed() {
        let r = Rect::new(control_x - 8.0 - 28.0, cy - 14.0, 28.0, 28.0);
        let lit = hover(a, ui, Target::Reset(index));
        if lit > 0.0 {
            let fill = t.button_hover.with_alpha(t.button_hover.a * lit);
            p.canvas.circle(r.x + 14.0, cy, 14.0, fill);
        }
        p.icon("edit-undo-symbolic", r.x + 14.0, cy, 16.0, t.window_dim);
        out.hits.push((r, Target::Reset(index)));
        text_right = r.x - 8.0;
    }

    let tx = rect.x + t.row_padding;
    let max = (text_right - tx).max(0.0);
    if row.subtitle.is_empty() {
        p.label_v(
            &row.title,
            tx,
            cy,
            max,
            t.font_size,
            Weight::Normal,
            t.window_text,
        );
    } else {
        p.label_v(
            &row.title,
            tx,
            cy - 10.0,
            max,
            t.font_size,
            Weight::Normal,
            t.window_text,
        );
        p.label_v(
            &row.subtitle,
            tx,
            cy + 10.0,
            max,
            t.subtitle_size,
            Weight::Normal,
            t.window_dim,
        );
    }
}

fn wallpapers(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    out: &mut Painted,
    row: &Row,
    rect: Rect,
) {
    let t = p.theme;
    let (cols, tile_w, tile_h) = wallpaper_grid(rect.w, t);
    let current = expand(row.value.as_str());
    let origin = (rect.x + t.row_padding, rect.y + t.row_padding);
    let place = |i: usize| {
        let (c, r) = (i % cols, i / cols);
        Rect::new(
            origin.0 + c as f32 * (tile_w + 12.0),
            origin.1 + r as f32 * (tile_h + 12.0),
            tile_w,
            tile_h,
        )
    };
    let ring = |p: &mut Painter<'_, '_>, r: Rect, color: Color| {
        p.canvas
            .stroke_rounded_rect(r.x - 4.0, r.y - 4.0, r.w + 8.0, r.h + 8.0, 12.0, 3.0, color);
    };

    // The built-in gradient first, roughly as the compositor draws it.
    let r = place(0);
    let appearance = &ui.config.appearance;
    let dark = appearance.color_scheme == cnde_config::ColorScheme::Dark;
    let (top, bottom, label_color) = match (appearance.palette, dark) {
        (cnde_config::Palette::Cnde, true) => (
            Color::rgb(0x2b, 0x1a, 0x40),
            Color::rgb(0x4a, 0x26, 0x48),
            Color::rgb(0xff, 0xff, 0xff),
        ),
        (cnde_config::Palette::Cnde, false) => (
            Color::rgb(0xf5, 0xd9, 0xe8),
            Color::rgb(0xe8, 0xe2, 0xf7),
            Color::rgb(0x2d, 0x22, 0x38),
        ),
        (cnde_config::Palette::Adwaita, true) => (
            Color::rgb(0x24, 0x3b, 0x5e),
            Color::rgb(0x3d, 0x2b, 0x55),
            Color::rgb(0xff, 0xff, 0xff),
        ),
        (cnde_config::Palette::Adwaita, false) => (
            Color::rgb(0xc7, 0xd6, 0xf2),
            Color::rgb(0xf0, 0xe0, 0xdc),
            Color::rgb(0x24, 0x24, 0x28),
        ),
    };
    p.canvas.rounded_rect(r.x, r.y, r.w, r.h, 8.0, top);
    p.canvas
        .rounded_rect(r.x, r.y + r.h / 2.0, r.w, r.h / 2.0, 8.0, bottom);
    p.canvas
        .rounded_rect(r.x, r.y + r.h / 2.0, r.w, 8.0, 0.0, top.mix(bottom, 0.5));
    let label = "Default";
    let lw = p.measure(label, t.subtitle_size, Weight::Normal);
    p.label_v(
        label,
        r.x + (r.w - lw) / 2.0,
        r.y + r.h / 2.0,
        r.w,
        t.subtitle_size,
        Weight::Normal,
        label_color,
    );
    let lit = hover(a, ui, Target::Wallpaper(None));
    if current.is_none() {
        ring(p, r, t.accent);
    } else if lit > 0.0 {
        ring(p, r, t.button_hover.with_alpha(t.button_hover.a * lit));
    }
    out.hits.push((r, Target::Wallpaper(None)));

    for (i, thumb) in ui.thumbs.iter().enumerate() {
        let r = place(i + 1);
        p.canvas
            .image(thumb.image.as_ref(), r.x, r.y, r.w, r.h, 8.0);
        let lit = hover(a, ui, Target::Wallpaper(Some(i)));
        if current.as_deref() == Some(thumb.path.as_path()) {
            ring(p, r, t.accent);
        } else if lit > 0.0 {
            ring(p, r, t.button_hover.with_alpha(t.button_hover.a * lit));
        }
        out.hits.push((r, Target::Wallpaper(Some(i))));
    }
}

/// `top` blended over an opaque `base`.
fn over(base: Color, top: Color) -> Color {
    let mix = |b: f32, t: f32| b + (t - b) * top.a;
    Color {
        r: mix(base.r, top.r),
        g: mix(base.g, top.g),
        b: mix(base.b, top.b),
        a: 1.0,
    }
}

/// A config path with "~/" expanded; `None` for empty.
pub fn expand(path: &str) -> Option<std::path::PathBuf> {
    if path.is_empty() {
        return None;
    }
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(rest)),
        None => Some(path.into()),
    }
}

fn popover(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    out: &mut Painted,
    key: &Key,
    w: f32,
    h: f32,
) {
    let Some((index, row)) = out
        .rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.key == *key && matches!(r.control, Control::Dropdown(_)))
    else {
        return;
    };
    let Control::Dropdown(choices) = &row.control else {
        return;
    };
    let Some(anchor) = out
        .hits
        .iter()
        .find(|(_, t)| *t == Target::Dropdown(index))
        .map(|(r, _)| *r)
    else {
        return;
    };
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    let width = (p.dropdown_width(&labels) + 16.0).max(anchor.w);
    let height = Painter::popover_height(labels.len());
    let x = (anchor.right() - width).clamp(8.0, (w - width - 8.0).max(8.0));
    let below = anchor.bottom() + 4.0;
    let y = if below + height <= h - 8.0 {
        below
    } else {
        (anchor.y - 4.0 - height).max(8.0)
    };
    let selected = choices.iter().position(|c| c.value == row.value);
    let hovers: Vec<f32> = (0..labels.len())
        .map(|i| hover(a, ui, Target::Choice(i)))
        .collect();
    let appear = a.get(AnimKey::Popover, 1.0, SHORT);
    let rects = p.popover(x, y, width, &labels, selected, |i| hovers[i], appear);
    for (i, r) in rects.into_iter().enumerate() {
        out.hits.push((r, Target::Choice(i)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(page: Page) -> Ui {
        Ui {
            config: Config::default(),
            displays: vec![
                Display {
                    name: "DP-1".into(),
                    description: "Dell U2720Q · 3840×2160 at 60 Hz".into(),
                },
                Display {
                    name: "HDMI-A-1".into(),
                    description: String::new(),
                },
            ],
            thumbs: Vec::new(),
            page,
            advanced: HashSet::new(),
            scroll: 0.0,
            hover: None,
            popover: None,
            edit: None,
            error: None,
            rounded: true,
            dragging: None,
        }
    }

    fn render(ui: &Ui, name: &str) -> Painted {
        let (w, h) = (920u32, 680u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let theme = Theme::for_appearance(&ui.config.appearance);
        let mut res = Res {
            fonts: Fonts::new(theme.font_families),
            icons: Icons::new(),
            theme,
            anim: Anim::new(),
        };
        let painted = {
            let mut canvas = Canvas::new(&mut pixels, w, h, 1.0).unwrap();
            paint(ui, &mut res, &mut canvas)
        };
        // CNDE_SNAPSHOTS=dir saves each render to look at.
        if let Some(dir) = std::env::var_os("CNDE_SNAPSHOTS") {
            let path = std::path::Path::new(&dir).join(format!("{name}.png"));
            image::RgbaImage::from_raw(w, h, pixels)
                .unwrap()
                .save(path)
                .unwrap();
        }
        painted
    }

    #[test]
    fn every_page_paints_with_targets() {
        for page in Page::ALL {
            let mut ui = ui(page);
            let painted = render(&ui, &format!("{page:?}"));
            assert!(painted.hit((20.0, 70.0)).is_some(), "{page:?}: sidebar");
            ui.advanced.insert(page);
            render(&ui, &format!("{page:?}-advanced"));
        }
    }

    #[test]
    fn dropdown_popover_and_entry() {
        let mut ui = ui(Page::Panel);
        ui.advanced.insert(Page::Panel);
        let painted = render(&ui, "panel");
        let clock = painted
            .rows
            .iter()
            .position(|r| matches!(r.control, Control::Dropdown(_)))
            .unwrap();
        ui.popover = Some(painted.rows[clock].key.clone());
        ui.hover = Some(Target::Choice(1));
        let painted = render(&ui, "panel-popover");
        assert!(painted.hits.iter().any(|(_, t)| *t == Target::Choice(5)));

        ui.popover = None;
        ui.edit = Some(Edit {
            key: Key::Path("panel.background"),
            text: "#00000080".into(),
            cursor: 3,
            int: false,
        });
        ui.error = Some("Not saved: panel.height must be 16 to 96, got 4".into());
        ui.scroll = 200.0;
        render(&ui, "panel-edit");

        ui.config.appearance.color_scheme = cnde_config::ColorScheme::Light;
        ui.edit = None;
        ui.error = None;
        ui.scroll = 0.0;
        ui.popover = Some(painted.rows[clock].key.clone());
        render(&ui, "panel-light");
    }
}
