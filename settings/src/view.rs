//! Painting the window: sidebar, header bars and the current page. Every
//! paint also records where each control ended up ([`Target`]s), which is
//! what the pointer is tested against until the next paint.

use cnde_config::Config;
use cnde_ui::{
    Animator, Canvas, Color, Fonts, Icons, Painter, Rect, Theme, Weight,
    anim::{MEDIUM, SHORT},
};

use crate::{
    pages::{self, Control, Display, Key, Page, Row, SWATCHES, Value},
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
    /// Part of the Sound page's mixer.
    Mixer(crate::mixer::Hit),
    /// A monitor on the arrangement canvas (index into `Ui::displays`).
    Screen(usize),
    /// Its turn button: clockwise (top right) or not (top left).
    Rotate(usize, bool),
    /// A wallpaper tile; `None` is the built-in one.
    Wallpaper(Option<usize>),
    /// The tile after the wallpapers: pick a picture of your own.
    AddWallpaper,
    /// A colour row's swatch: the row, and which of `SWATCHES` (row-major).
    Swatch(usize, usize),
    /// The accent row's "Auto" button: follow the wallpaper.
    AutoAccent(usize),
    Advanced,
    /// The sidebar's search field.
    Search,
    /// The header's back button, on a sub-page.
    Back,
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
    /// A mixer slider, by what it sets.
    Level(String),
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
pub(crate) fn hover(a: &mut Anim, ui: &Ui, target: Target) -> f32 {
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
/// How opaque a frosted sidebar's tint is over the compositor's frost.
const FROSTED_SIDEBAR: f32 = 0.6;

pub struct Ui {
    pub config: Config,
    pub displays: Vec<Display>,
    pub thumbs: Vec<Thumb>,
    pub page: Page,
    /// Advanced rows are shown, on every page.
    pub advanced: bool,
    pub scroll: f32,
    /// How far the sidebar's pages are scrolled (the search field stays put).
    pub sidebar_scroll: f32,
    /// How far the open dropdown's choices are scrolled.
    pub popover_scroll: f32,
    pub hover: Option<Target>,
    /// The dropdown that's open.
    pub popover: Option<Key>,
    pub edit: Option<Edit>,
    /// What the sidebar's search is looking for; results replace the page.
    pub search: String,
    /// The shortcut row waiting for a key combo.
    pub capturing: Option<Key>,
    /// Why the last captured combo wasn't used, under its row.
    pub shortcut_note: Option<(Key, String)>,
    /// Shown at the bottom until it times out.
    pub error: Option<String>,
    /// Square corners when maximized or tiled.
    pub rounded: bool,
    /// The sidebar shows the compositor's frost through it (only while
    /// rounded: snapped or maximized it's solid, as the compositor knows).
    pub frosted: bool,
    /// The slider being dragged (it follows the pointer, unanimated).
    pub dragging: Option<Key>,
    /// The Wi-Fi worker's last report (once the page has been opened).
    pub wifi: Option<crate::wifi::State>,
    /// The network whose password is being asked for.
    /// The network being joined, whose form shows.
    pub wifi_join: Option<crate::wifi::Join>,
    /// The network whose details were opened (a sub-page of Wi-Fi).
    pub wifi_details: Option<String>,
    /// The Bluetooth worker's last report (once the page has been opened).
    pub bluetooth: Option<crate::bluetooth::State>,
    /// The backlight's percent; `None` without one, which hides the row.
    pub brightness: Option<u32>,
    /// The power mode worker's last report (once the page has been opened).
    pub power_mode: Option<crate::power_mode::State>,
    /// The installed apps and defaults (once the page has been opened).
    pub default_apps: Option<crate::defaults::State>,
    /// The Sound page's devices and playing apps (once the page has been
    /// opened).
    pub sound: Option<crate::sound::State>,
    /// The mixer slider being dragged.
    pub sound_slider: Option<(crate::mixer::SliderId, Rect)>,
    /// An app row being pressed or dragged to another output.
    pub sound_drag: Option<crate::mixer::Drag>,
    /// The app (by id) whose "Play on" menu is open.
    pub sound_menu: Option<String>,
    /// Where the arrangement shows each display while one's dragged (and
    /// until the compositor reports the result).
    pub arranged: Option<Vec<(i32, i32)>>,
    /// The display being dragged.
    pub arranging: Option<usize>,
}

impl Ui {
    /// Whether search results are showing instead of a page.
    pub fn searching(&self) -> bool {
        !self.search.trim().is_empty()
    }

    /// The sub-page showing, by its title: the Wi-Fi network whose details
    /// are open, while it's still connected or saved.
    pub fn sub_page(&self) -> Option<&str> {
        let ssid = self.wifi_details.as_deref()?;
        let connected = self
            .wifi
            .as_ref()?
            .networks
            .iter()
            .any(|n| (n.connected || n.known) && n.ssid == ssid);
        (self.page == Page::Wifi && connected).then_some(ssid)
    }
}

/// The result of a paint, for hit-testing.
#[derive(Default)]
pub struct Painted {
    pub hits: Vec<(Rect, Target)>,
    pub rows: Vec<Row>,
    /// Where each row's slider is (for dragging).
    pub sliders: Vec<(usize, Rect)>,
    /// The mixer as last painted, to resolve drops against.
    pub mixer: Option<crate::mixer::Layout>,
    /// The Bluetooth device groups, as last painted.
    pub device_list: Option<Rect>,
    /// How far the page can scroll.
    pub max_scroll: f32,
    /// The sidebar's area, and how far its pages can scroll.
    pub sidebar: Option<Rect>,
    pub sidebar_max_scroll: f32,
    /// How far the open dropdown's choices can scroll.
    pub popover_max_scroll: f32,
    /// Window px per logical px on the arrangement canvas.
    pub arrange_scale: f32,
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

const MAX_COLUMN: f32 = 680.0;
const GROUP_GAP: f32 = 16.0;
/// Content padding either side of the column.
const SIDE_PAD: f32 = 24.0;

/// Whether groups sit on a filled card; Quiet's are transparent, so a
/// group is just a title and its rows.
pub(crate) fn boxed(t: &Theme) -> bool {
    t.card_background.a > 0.01
}
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
    let radius = if ui.rounded { t.window_radius } else { 0.0 };
    let sw = t.sidebar_width;
    let hh = t.window_header_height;

    if ui.rounded && ui.frosted {
        p.canvas.clear(Color::rgba(0, 0, 0, 0.0));
        // Only the content is solid; the sidebar, drawn over nothing,
        // lets the frost behind the window show.
        // Rounded on the right only: the left edge meets the sidebar.
        p.canvas.rounded_rect_clipped(
            sw - radius,
            0.0,
            w - sw + radius,
            h,
            radius,
            t.window_background,
            (sw, 0.0, w - sw, h),
        );
    } else {
        p.canvas.rounded_fill(radius, t.window_background);
    }

    // The page first: it scrolls under the header bar drawn after it.
    let content = Rect::new(sw, hh, w - sw, h - hh);
    // A new page fades in, sliding a little from the left. The slide stays
    // inside the column's 24px side margin, so it never draws over the
    // sidebar. Set PAGE_SLIDE to 0.0 for a plain fade.
    const PAGE_SLIDE: f32 = 16.0;
    let appear = a.get(AnimKey::Page, 1.0, MEDIUM);
    let slid = Rect {
        x: content.x - PAGE_SLIDE * (1.0 - appear),
        ..content
    };
    page(ui, a, &mut p, &mut out, slid, 0.0);
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
    // The page's title on the left, the window buttons on the right. A
    // sub-page's title has a back button before it.
    let mut title_x = sw + SIDE_PAD;
    let title = if ui.searching() {
        "Search"
    } else {
        ui.sub_page().unwrap_or(ui.page.title())
    };
    if ui.sub_page().is_some() {
        let size = 28.0;
        let back = Rect::new(sw + 16.0, (hh - size) / 2.0, size, size);
        let fill = t
            .button_background
            .mix(t.button_hover, hover(a, ui, Target::Back));
        p.canvas
            .circle(back.x + size / 2.0, back.y + size / 2.0, size / 2.0, fill);
        p.icon(
            "go-previous-symbolic",
            back.x + size / 2.0,
            back.y + size / 2.0,
            16.0,
            t.window_text,
        );
        out.hits.push((back, Target::Back));
        title_x = back.right() + 8.0;
    }
    p.label_v(
        title,
        title_x,
        hh / 2.0,
        w - title_x - 140.0,
        t.window_title_size,
        Weight::Bold,
        t.window_text,
    );
    let size = 22.0;
    let shown_buttons = match ui.config.appearance.window_buttons {
        cnde_config::WindowButtons::All => 3.0,
        cnde_config::WindowButtons::Close => 1.0,
    };
    // The global Advanced toggle, left of the window buttons.
    let label_w = p.measure("Advanced", t.subtitle_size, Weight::Normal);
    let toggle_w = label_w + 8.0 + t.switch_width;
    let toggle = Rect::new(
        w - 10.0 - shown_buttons * (size + 8.0) - 8.0 - toggle_w,
        (hh - t.switch_height) / 2.0,
        toggle_w,
        t.switch_height,
    );
    if toggle.x > title_x + 120.0 {
        p.label_v(
            "Advanced",
            toggle.x,
            hh / 2.0,
            label_w + 2.0,
            t.subtitle_size,
            Weight::Normal,
            t.window_dim,
        );
        let on = a.get(
            AnimKey::Switch(Key::Path("advanced")),
            if ui.advanced { 1.0 } else { 0.0 },
            SHORT,
        );
        p.switch(toggle.right() - t.switch_width, toggle.y, on);
        let hit = Rect::new(toggle.x - 6.0, 0.0, toggle.w + 12.0, hh);
        out.hits.push((hit, Target::Advanced));
    }
    let maximize_icon = if ui.rounded {
        "window-maximize-symbolic"
    } else {
        "window-restore-symbolic"
    };
    // `appearance.window_buttons`, as on the compositor's title bars.
    let shown = match ui.config.appearance.window_buttons {
        cnde_config::WindowButtons::All => 3,
        cnde_config::WindowButtons::Close => 1,
    };
    for (i, (target, icon)) in [
        (Target::Close, "window-close-symbolic"),
        (Target::Maximize, maximize_icon),
        (Target::Minimize, "window-minimize-symbolic"),
    ]
    .into_iter()
    .take(shown)
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
    p.canvas
        .rounded_rect(sw, hh - 1.0, w - sw, 1.0, 0.0, t.card_separator);

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

fn sidebar(ui: &Ui, a: &mut Anim, p: &mut Painter<'_, '_>, out: &mut Painted, h: f32) {
    let t = p.theme;
    let sw = t.sidebar_width;
    let hh = t.window_header_height;
    // Full height, square on the content side, with a rule down its edge.
    let radius = if ui.rounded { t.window_radius } else { 0.0 };
    let fill = if ui.rounded && ui.frosted {
        t.window_sidebar.with_alpha(FROSTED_SIDEBAR)
    } else {
        t.window_sidebar
    };
    // Rounded on the left only, in one piece: overlapping see-through
    // fills would show their seams.
    p.canvas
        .rounded_rect_clipped(0.0, 0.0, sw + radius, h, radius, fill, (0.0, 0.0, sw, h));
    p.canvas
        .rounded_rect(sw - 1.0, 0.0, 1.0, h, 0.0, t.card_separator);
    out.hits.push((Rect::new(0.0, 0.0, sw, hh), Target::Header));
    p.label_v(
        "Settings",
        SIDE_PAD - 4.0,
        hh / 2.0,
        sw,
        t.window_title_size,
        Weight::Bold,
        t.window_text,
    );
    // A pill filled with `sidebar_selected`; when that is the accent the
    // text on it is `on_accent`.
    let on_pill = if t.sidebar_selected == t.accent {
        t.on_accent
    } else {
        t.window_text
    };
    let field = Rect::new(10.0, hh + 2.0, sw - 21.0, t.button_height);
    match &ui.edit {
        Some(edit) if edit.key == Key::Search => {
            p.entry(field, &edit.text, "Search", Some(edit.cursor), false)
        }
        _ => p.entry(field, &ui.search, "Search", None, false),
    }
    out.hits.push((field, Target::Search));
    // The pages scroll under the search field, clipped to the space below it.
    let top = field.bottom() + 8.0;
    let list = Rect::new(0.0, top, sw, (h - top).max(0.0));
    let clip = (list.x, list.y, list.w, list.h);
    out.sidebar = Some(Rect::new(0.0, 0.0, sw, h));
    let mut y = top - ui.sidebar_scroll;
    let mut section = "";
    for page in Page::visible(&ui.config) {
        if page.section() != section {
            // A gap before every section but the first.
            if !section.is_empty() {
                y += 8.0;
            }
            section = page.section();
            // Quiet, like a group title on a page.
            clipped_text(p, section, 20.0, y + 4.0, 11.0, Weight::Bold, t.window_dim, clip);
            y += 22.0;
        }
        let rect = Rect::new(10.0, y, sw - 21.0, t.sidebar_row_height);
        // Rows scrolled out of the list are skipped (their place still counts).
        if rect.bottom() > list.y && rect.y < list.bottom() {
            let selected = a.get(
                AnimKey::Selected(page),
                if ui.page == page { 1.0 } else { 0.0 },
                SHORT,
            );
            let hovered = hover(a, ui, Target::Page(page)) * (1.0 - selected);
            for (color, amount) in [(t.hover, hovered), (t.sidebar_selected, selected)] {
                if amount > 0.0 {
                    let fill = color.with_alpha(color.a * amount);
                    p.canvas.rounded_rect_clipped(
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h,
                        t.button_radius,
                        fill,
                        clip,
                    );
                }
            }
            let cy = rect.y + rect.h / 2.0;
            let ink = t.window_text.mix(on_pill, selected);
            p.icon_clipped(page.icon(), rect.x + 20.0, cy, 16.0, ink, clip);
            p.label_v_clipped(
                page.title(),
                rect.x + 40.0,
                cy,
                rect.w - 48.0,
                t.font_size,
                Weight::Normal,
                ink,
                clip,
            );
            // Only the visible part takes the pointer.
            let shown = Rect::new(
                rect.x,
                rect.y.max(list.y),
                rect.w,
                rect.bottom().min(list.bottom()) - rect.y.max(list.y),
            );
            out.hits.push((shown, Target::Page(page)));
        }
        y += t.sidebar_row_height + 2.0;
    }
    // The last row's gap goes below it, so it isn't flush with the window's edge.
    let content_h = y + ui.sidebar_scroll - top + 8.0;
    out.sidebar_max_scroll = (content_h - list.h).max(0.0);
}

/// Text with its top-left at (`x`, `y`), drawn only inside `clip`.
#[allow(clippy::too_many_arguments)]
fn clipped_text(
    p: &mut Painter<'_, '_>,
    text: &str,
    x: f32,
    y: f32,
    size: f32,
    weight: Weight,
    color: Color,
    clip: (f32, f32, f32, f32),
) {
    let mut layout = p.fonts.layout(text, size, weight, p.canvas.scale());
    p.canvas.text_clipped(p.fonts, &mut layout, x, y, color, clip);
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
    let searching = ui.searching();
    let mut groups = if searching {
        crate::search::results(ui, &ui.search)
    } else {
        pages::build_for(ui, ui.page)
    };
    // Why a captured combo was refused, under its row.
    if let Some((key, note)) = &ui.shortcut_note {
        for row in groups.iter_mut().flat_map(|g| &mut g.rows) {
            if row.key == *key {
                row.subtitle = note.clone();
            }
        }
    }
    let show_advanced = ui.advanced;
    let col_w = (area.w - 2.0 * SIDE_PAD).min(MAX_COLUMN);
    let x = area.x + (area.w - col_w) / 2.0;
    let top = area.y + 12.0;
    let mut y = top - ui.scroll + offset;

    let mut list: Option<(f32, f32)> = None;
    for group in groups {
        let group_top = y;
        let devices = ui.page == Page::Bluetooth && group.title.ends_with(" Devices");
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
                &group.title,
                x + t.row_padding,
                y + 2.0,
                12.0,
                Weight::Bold,
                t.window_dim,
            );
            y += 24.0;
        }
        if !group.description.is_empty() {
            p.label_v(
                &group.description,
                x + t.row_padding,
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
            if devices {
                list = Some((list.map_or(group_top, |l| l.0), y));
            }
            continue;
        }
        let heights: Vec<f32> = rows.iter().map(|r| row_height(ui, r, t, col_w)).collect();
        let card_h: f32 = heights.iter().sum();
        // The mixer draws a card for each device itself.
        let mixer = matches!(rows[0].control, Control::Mixer);
        if boxed(t) && !mixer {
            p.canvas
                .rounded_rect(x, y, col_w, card_h, t.card_radius, t.card_background);
        }
        let mut ry = y;
        let count = rows.len();
        for (i, (row, rh)) in rows.into_iter().zip(heights).enumerate() {
            if i > 0 && !mixer {
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
        if devices {
            list = Some((list.map_or(group_top, |l| l.0), y - GROUP_GAP));
        }
    }
    out.device_list = list.map(|(top, bottom)| Rect::new(x, top, col_w, bottom - top));

    let content_h = y + ui.scroll - offset - top + 24.0 - GROUP_GAP;
    out.max_scroll = (content_h - area.h + 24.0).max(0.0);
}

fn row_height(ui: &Ui, row: &Row, t: &Theme, col_w: f32) -> f32 {
    if let Control::Wallpapers = row.control {
        let (cols, _, tile_h) = wallpaper_grid(col_w, t);
        // The built-in one, the pictures, and "Add".
        let count = ui.thumbs.len() + 2;
        let lines = count.div_ceil(cols);
        return t.row_padding * 2.0 + lines as f32 * (tile_h + 12.0) - 12.0;
    }
    if let Control::Color { .. } = row.control {
        return t.row_subtitle_height + swatches_height() + t.row_padding;
    }
    if let Control::Arrangement = row.control {
        return ARRANGE_HEIGHT;
    }
    if let Control::Mixer = row.control {
        return ui.sound.as_ref().map_or(0.0, |s| {
            crate::mixer::Layout::new(s, 0.0, 0.0, col_w, None).height
        });
    }
    if row.subtitle.is_empty() {
        t.row_height
    } else {
        t.row_subtitle_height
    }
}

const SWATCH: f32 = 26.0;
const SWATCH_GAP: f32 = 8.0;

fn swatches_height() -> f32 {
    let lines = SWATCHES.len() as f32;
    lines * SWATCH + (lines - 1.0) * SWATCH_GAP
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
    let hits_start = out.hits.len();
    if let Control::Wallpapers = row.control {
        wallpapers(ui, a, p, out, row, rect);
        return;
    }
    if let Control::Arrangement = row.control {
        arrangement(ui, a, p, out, rect);
        return;
    }
    if let Control::Mixer = row.control {
        crate::mixer::paint(ui, a, p, out, rect);
        return;
    }
    let activatable = matches!(
        row.control,
        Control::Switch { .. }
            | Control::Dropdown(_)
            | Control::Network { .. }
            | Control::Shortcut
    );
    let row_hover = if activatable {
        hover(a, ui, Target::Row(index))
    } else {
        0.0
    };
    if row_hover > 0.0 && !boxed(t) {
        let Rect { x, y, w, h } = rect;
        let fill = t.hover.with_alpha(t.hover.a * row_hover);
        p.canvas.rounded_rect(x, y, w, h, t.button_radius, fill);
    } else if row_hover > 0.0 {
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
    // Colour rows put their swatches below the usual title line.
    let line_h = match row.control {
        Control::Color { .. } => t.row_subtitle_height,
        _ => rect.h,
    };
    let cy = rect.y + line_h / 2.0;
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
        Control::Text { placeholder, .. } | Control::Color { placeholder } => {
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
        Control::Network {
            signal,
            secured,
            connected,
            details,
        } => {
            // Right to left: an arrow to its details, a check if connected,
            // the lock, the signal (none when out of range).
            let mut x = right;
            if *details {
                x -= 16.0;
                p.icon("go-next-symbolic", x + 8.0, cy, 14.0, t.window_dim);
                x -= 10.0;
            }
            if *connected {
                x -= 18.0;
                p.icon("object-select-symbolic", x + 9.0, cy, 16.0, t.accent);
                x -= 10.0;
            }
            if *secured {
                x -= 16.0;
                p.icon("changes-prevent-symbolic", x + 8.0, cy, 14.0, t.window_dim);
                x -= 8.0;
            }
            if let Some(signal) = signal {
                let bars = match signal {
                    0..=24 => "network-wireless-signal-weak-symbolic",
                    25..=49 => "network-wireless-signal-ok-symbolic",
                    50..=74 => "network-wireless-signal-good-symbolic",
                    _ => "network-wireless-signal-excellent-symbolic",
                };
                x -= 18.0;
                p.icon(bars, x + 9.0, cy, 16.0, t.window_text);
            }
            x
        }
        Control::Password => {
            let r = Rect::new(
                right - ENTRY_WIDTH,
                cy - t.button_height / 2.0,
                ENTRY_WIDTH,
                t.button_height,
            );
            let (text, cursor) = match &ui.edit {
                Some(edit) if edit.key == row.key => (edit.text.as_str(), Some(edit.cursor)),
                _ => (row.value.as_str(), None),
            };
            let dots: String = "•".repeat(text.chars().count());
            p.entry(r, &dots, "Password", cursor, false);
            out.hits.push((r, Target::Entry(index)));
            r.x
        }
        Control::Info => {
            let text = row.value.as_str();
            let max = (rect.w * 0.6).max(120.0);
            let tw = p.measure(text, t.font_size, Weight::Normal).min(max);
            p.label_v(
                text,
                right - tw,
                cy,
                tw + 1.0,
                t.font_size,
                Weight::Normal,
                t.window_dim,
            );
            right - tw
        }
        Control::Shortcut => {
            let waiting = ui.capturing.as_ref() == Some(&row.key);
            let (text, ink) = if waiting {
                ("Press new shortcut\u{2026}", t.accent)
            } else if row.value.as_str().is_empty() {
                ("Off", t.window_dim)
            } else {
                (row.value.as_str(), t.window_dim)
            };
            let max = (rect.w * 0.6).max(120.0);
            let tw = p.measure(text, t.font_size, Weight::Normal).min(max);
            p.label_v(text, right - tw, cy, tw + 1.0, t.font_size, Weight::Normal, ink);
            right - tw
        }
        Control::Wallpapers | Control::Arrangement | Control::Mixer => unreachable!(),
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
    let mut max = (text_right - tx).max(0.0);
    if row.advanced {
        // A small tag after the title.
        let title_w = p.measure(&row.title, t.font_size, Weight::Normal);
        let size = t.subtitle_size - 2.0;
        let tag_w = p.measure("Advanced", size, Weight::Bold) + 12.0;
        let tag_x = tx + title_w.min(max - tag_w - 8.0).max(0.0) + 8.0;
        if tag_x + tag_w <= tx + max {
            let ty = if row.subtitle.is_empty() { cy } else { cy - 10.0 };
            p.canvas.rounded_rect(
                tag_x,
                ty - 8.0,
                tag_w,
                16.0,
                8.0,
                t.accent.with_alpha(0.18),
            );
            p.label_v(
                "Advanced",
                tag_x + 6.0,
                ty,
                tag_w,
                size,
                Weight::Bold,
                t.accent,
            );
            max = (tag_x - 8.0 - tx).max(0.0);
        }
    }
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
    if let Control::Color { .. } = row.control {
        swatches(ui, a, p, out, row, index, rect, line_h);
    }
    if row.disabled {
        // Nothing in it takes clicks; fade it into the background.
        out.hits.truncate(hits_start);
        let base = if boxed(t) {
            over(t.window_background, t.card_background)
        } else {
            t.window_background
        };
        let Rect { x, y, w, h } = rect;
        p.canvas.rounded_rect(x, y, w, h, 0.0, base.with_alpha(0.55));
    }
}

#[allow(clippy::too_many_arguments)]
fn swatches(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    out: &mut Painted,
    row: &Row,
    index: usize,
    rect: Rect,
    line_h: f32,
) {
    let t = p.theme;
    let current = row.value.as_str().to_ascii_lowercase();
    let x0 = rect.x + t.row_padding;
    let y0 = rect.y + line_h;
    if row.key == Key::Path("appearance.accent_color") {
        // Beside the swatches, level with the last line.
        let w = p.measure("Auto", t.font_size, Weight::Normal) + 24.0;
        let x = x0 + SWATCHES[0].len() as f32 * (SWATCH + SWATCH_GAP) + 8.0;
        let y = y0 + (SWATCHES.len() - 1) as f32 * (SWATCH + SWATCH_GAP);
        let button = Rect::new(x, y, w, SWATCH);
        let lit = hover(a, ui, Target::AutoAccent(index));
        p.button(button, lit);
        if current == "auto" {
            p.canvas
                .stroke_rounded_rect(x - 3.5, y - 3.5, w + 7.0, SWATCH + 7.0, 10.0, 2.0, t.window_text);
        }
        p.label_v(
            "Auto",
            x + 12.0,
            y + SWATCH / 2.0,
            w,
            t.font_size,
            Weight::Normal,
            t.window_text,
        );
        out.hits.push((button, Target::AutoAccent(index)));
    }
    for (line, colors) in SWATCHES.iter().enumerate() {
        for (column, hex) in colors.iter().enumerate() {
            let Some((r, g, b, _)) = cnde_config::parse_color(hex) else {
                continue;
            };
            let x = x0 + column as f32 * (SWATCH + SWATCH_GAP);
            let y = y0 + line as f32 * (SWATCH + SWATCH_GAP);
            let swatch = Rect::new(x, y, SWATCH, SWATCH);
            let n = line * colors.len() + column;
            let lit = hover(a, ui, Target::Swatch(index, n));
            if *hex == current {
                p.canvas.stroke_rounded_rect(
                    x - 3.5,
                    y - 3.5,
                    SWATCH + 7.0,
                    SWATCH + 7.0,
                    10.0,
                    2.0,
                    t.window_text,
                );
            } else if lit > 0.0 {
                p.canvas.stroke_rounded_rect(
                    x - 3.5,
                    y - 3.5,
                    SWATCH + 7.0,
                    SWATCH + 7.0,
                    10.0,
                    2.0,
                    t.window_dim.with_alpha(t.window_dim.a * lit),
                );
            }
            p.canvas
                .rounded_rect(x, y, SWATCH, SWATCH, 7.0, Color::rgb(r, g, b));
            out.hits.push((swatch, Target::Swatch(index, n)));
        }
    }
}

const ARRANGE_HEIGHT: f32 = 240.0;

/// The monitors as boxes, scaled to fit the card with room to drag. The
/// scale and centre come from where the compositor has them, so the
/// picture holds still while one's moved.
fn arrangement(ui: &Ui, a: &mut Anim, p: &mut Painter<'_, '_>, out: &mut Painted, rect: Rect) {
    let t = p.theme;
    let ds = &ui.displays;
    let (Some(min_x), Some(min_y), Some(max_x), Some(max_y)) = (
        ds.iter().map(|d| d.x).min(),
        ds.iter().map(|d| d.y).min(),
        ds.iter().map(|d| d.x + d.w).max(),
        ds.iter().map(|d| d.y + d.h).max(),
    ) else {
        return;
    };
    let (bw, bh) = ((max_x - min_x).max(1) as f32, (max_y - min_y).max(1) as f32);
    let pad = 24.0;
    let scale = ((rect.w - pad * 2.0) / bw).min((rect.h - pad * 2.0) / bh) * 0.75;
    out.arrange_scale = scale;
    let ox = rect.x + rect.w / 2.0 - (min_x as f32 + bw / 2.0) * scale;
    let oy = rect.y + rect.h / 2.0 - (min_y as f32 + bh / 2.0) * scale;
    let main = ui
        .config
        .outputs
        .iter()
        .find(|o| o.main)
        .map(|o| o.name.as_str())
        .or_else(|| {
            ds.iter()
                .min_by_key(|d| (d.x, d.y))
                .map(|d| d.name.as_str())
        });

    // The dragged one on top.
    let mut order: Vec<usize> = (0..ds.len()).collect();
    order.sort_by_key(|&i| ui.arranging == Some(i));
    for i in order {
        let d = &ds[i];
        let (x, y) = ui
            .arranged
            .as_ref()
            .and_then(|a| a.get(i).copied())
            .unwrap_or((d.x, d.y));
        let gap = 2.0;
        let r = Rect::new(
            ox + x as f32 * scale + gap,
            oy + y as f32 * scale + gap,
            d.w as f32 * scale - gap * 2.0,
            d.h as f32 * scale - gap * 2.0,
        );
        let lit =
            hover(a, ui, Target::Screen(i)).max(if ui.arranging == Some(i) { 1.0 } else { 0.0 });
        let fill = t.window_sidebar.mix(t.accent, 0.18 + 0.17 * lit);
        p.canvas.rounded_rect(r.x, r.y, r.w, r.h, 8.0, fill);
        let border = if ui.arranging == Some(i) {
            t.accent
        } else {
            t.card_separator
        };
        p.canvas
            .stroke_rounded_rect(r.x, r.y, r.w, r.h, 8.0, 2.0, border);
        // The top bar lives on the main display.
        if main == Some(d.name.as_str()) {
            p.canvas.rounded_rect(
                r.x + 6.0,
                r.y + 6.0,
                r.w - 12.0,
                4.0,
                2.0,
                t.window_text.with_alpha(0.5),
            );
        }
        let label_w = p
            .measure(&d.name, t.font_size, Weight::Bold)
            .min(r.w - 12.0);
        p.label_v(
            &d.name,
            r.x + (r.w - label_w) / 2.0,
            r.y + r.h / 2.0 - 8.0,
            r.w - 12.0,
            t.font_size,
            Weight::Bold,
            t.window_text,
        );
        let (pw, ph) = d.current.map_or((d.w, d.h), |(w, h, _)| (w, h));
        let size = format!("{pw} × {ph}");
        let size_w = p
            .measure(&size, t.subtitle_size, Weight::Normal)
            .min(r.w - 12.0);
        p.label_v(
            &size,
            r.x + (r.w - size_w) / 2.0,
            r.y + r.h / 2.0 + 10.0,
            r.w - 12.0,
            t.subtitle_size,
            Weight::Normal,
            t.window_dim,
        );
        out.hits.push((r, Target::Screen(i)));
        // Turn it a quarter: anticlockwise at the top left, clockwise at
        // the top right (below the top bar's line).
        if r.w >= 72.0 && r.h >= 52.0 {
            for clockwise in [false, true] {
                let size = 22.0;
                let bx = if clockwise { r.x + r.w - 8.0 - size } else { r.x + 8.0 };
                let b = Rect::new(bx, r.y + 14.0, size, size);
                let lit = hover(a, ui, Target::Rotate(i, clockwise));
                p.canvas.circle(
                    b.x + size / 2.0,
                    b.y + size / 2.0,
                    size / 2.0,
                    t.window_text.with_alpha(0.08 + 0.14 * lit),
                );
                let icon = if clockwise {
                    "object-rotate-right-symbolic"
                } else {
                    "object-rotate-left-symbolic"
                };
                p.icon(
                    icon,
                    b.x + size / 2.0,
                    b.y + size / 2.0,
                    14.0,
                    t.window_text.with_alpha(0.6 + 0.4 * lit),
                );
                out.hits.push((b, Target::Rotate(i, clockwise)));
            }
        }
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
        (cnde_config::Palette::Quiet, true) => (
            Color::rgb(0x2e, 0x3d, 0x33),
            Color::rgb(0x6b, 0x4f, 0x2b),
            Color::rgb(0xff, 0xff, 0xff),
        ),
        (cnde_config::Palette::Quiet, false) => (
            Color::rgb(0xd6, 0xe0, 0xd1),
            Color::rgb(0xf2, 0xd6, 0xa3),
            Color::rgb(0x1b, 0x1d, 0x1a),
        ),
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
        // Custom and adaptive: the window's own colours stand in.
        (cnde_config::Palette::Custom | cnde_config::Palette::Adaptive, _) => (
            t.window_sidebar,
            t.window_background.mix(t.accent, 0.25),
            t.window_text,
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

    // "Add": an empty slot with a plus in a square.
    let r = place(ui.thumbs.len() + 1);
    let lit = hover(a, ui, Target::AddWallpaper);
    if lit > 0.0 {
        p.canvas.rounded_rect(
            r.x,
            r.y,
            r.w,
            r.h,
            8.0,
            t.button_hover.with_alpha(t.button_hover.a * lit),
        );
    }
    p.canvas.stroke_rounded_rect(
        r.x + 1.0,
        r.y + 1.0,
        r.w - 2.0,
        r.h - 2.0,
        8.0,
        1.5,
        t.window_dim.with_alpha(t.window_dim.a * 0.5),
    );
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    p.icon("insert-image-symbolic", cx, cy - 8.0, 24.0, t.window_dim);
    let label = "Add";
    let lw = p.measure(label, t.subtitle_size, Weight::Normal);
    p.label_v(
        label,
        cx - lw / 2.0,
        cy + 18.0,
        lw + 1.0,
        t.subtitle_size,
        Weight::Normal,
        t.window_dim,
    );
    out.hits.push((r, Target::AddWallpaper));
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
    let list = dropdown_box(h, anchor.y, anchor.bottom(), labels.len());
    let x = (anchor.right() - width).clamp(8.0, (w - width - 8.0).max(8.0));
    let scroll = ui.popover_scroll.clamp(0.0, list.max_scroll);
    out.popover_max_scroll = list.max_scroll;
    let selected = choices.iter().position(|c| c.value == row.value);
    let hovers: Vec<f32> = (0..labels.len())
        .map(|i| hover(a, ui, Target::Choice(i)))
        .collect();
    let appear = a.get(AnimKey::Popover, 1.0, SHORT);
    let rects = p.popover_scrolled(
        x,
        list.y,
        width,
        list.h,
        scroll,
        &labels,
        selected,
        |i| hovers[i],
        appear,
    );
    // Only the part inside the list takes the pointer.
    let (top, bottom) = (list.y, list.y + list.h);
    for (i, r) in rects.into_iter().enumerate() {
        let (y0, y1) = (r.y.max(top), r.bottom().min(bottom));
        if y1 > y0 {
            out.hits.push((Rect::new(r.x, y0, r.w, y1 - y0), Target::Choice(i)));
        }
    }
}

/// Gap between an open dropdown's list and its button.
const DROPDOWN_GAP: f32 = 4.0;
/// Room kept between an open dropdown's list and the window's edges.
const DROPDOWN_MARGIN: f32 = 8.0;
/// A choice's height, and the padding above the first one, as
/// `Painter::popover_scrolled` lays them out.
const CHOICE_ROW: f32 = 36.0;
const CHOICE_PAD: f32 = 6.0;

/// Where an open dropdown's list goes: its top, its height, and how far its
/// choices can scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropdownBox {
    pub y: f32,
    pub h: f32,
    pub max_scroll: f32,
}

/// The list of `count` choices for a button from `anchor_top` to
/// `anchor_bottom`, in a window `window_h` tall. It goes below the button
/// when all of it fits there, else above when there's more room above, else
/// below. It's cut to the room it gets, and its choices scroll.
pub fn dropdown_box(
    window_h: f32,
    anchor_top: f32,
    anchor_bottom: f32,
    count: usize,
) -> DropdownBox {
    let full = Painter::popover_height(count);
    let below = anchor_bottom + DROPDOWN_GAP;
    let room_below = (window_h - DROPDOWN_MARGIN - below).max(0.0);
    let room_above = (anchor_top - DROPDOWN_GAP - DROPDOWN_MARGIN).max(0.0);
    let above = full > room_below && room_above > room_below;
    let room = if above { room_above } else { room_below };
    let h = full.min(room);
    let y = if above {
        anchor_top - DROPDOWN_GAP - h
    } else {
        below
    };
    DropdownBox {
        y,
        h,
        max_scroll: (full - h).max(0.0),
    }
}

/// The scroll that brings choice `index` into `b`, moving the list no more
/// than it must from `scroll`.
pub fn dropdown_reveal(scroll: f32, index: usize, b: DropdownBox) -> f32 {
    let top = CHOICE_PAD + CHOICE_ROW * index as f32;
    let bottom = top + CHOICE_ROW;
    scroll
        .min(top)
        .max(bottom - b.h)
        .clamp(0.0, b.max_scroll)
}

/// The scroll an open dropdown starts at: its selected choice in view.
pub fn dropdown_open_scroll(
    window_h: f32,
    anchor: Rect,
    count: usize,
    selected: Option<usize>,
) -> f32 {
    let b = dropdown_box(window_h, anchor.y, anchor.bottom(), count);
    dropdown_reveal(0.0, selected.unwrap_or(0), b)
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
                    x: 0,
                    y: 0,
                    w: 2560,
                    h: 1440,
                    vrr_capable: true,
                    modes: vec![(3840, 2160, 60_000), (2560, 1440, 59_951)],
                    current: Some((3840, 2160, 60_000)),
                    preferred: Some((3840, 2160, 60_000)),
                },
                Display {
                    name: "HDMI-A-1".into(),
                    description: String::new(),
                    x: 2560,
                    y: 200,
                    w: 1920,
                    h: 1080,
                    vrr_capable: false,
                    modes: vec![(1920, 1080, 165_000), (1920, 1080, 60_000)],
                    current: Some((1920, 1080, 165_000)),
                    preferred: Some((1920, 1080, 60_000)),
                },
            ],
            thumbs: Vec::new(),
            page,
            advanced: false,
            scroll: 0.0,
            sidebar_scroll: 0.0,
            popover_scroll: 0.0,
            hover: None,
            popover: None,
            edit: None,
            search: String::new(),
            capturing: None,
            shortcut_note: None,
            error: None,
            rounded: true,
            frosted: false,
            dragging: None,
            wifi: None,
            wifi_join: None,
            wifi_details: None,
            bluetooth: None,
            brightness: Some(50),
            power_mode: None,
            default_apps: None,
            sound: None,
            sound_slider: None,
            sound_drag: None,
            sound_menu: None,
            arranged: None,
            arranging: None,
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

    /// Paint time while scrolling, at 2560x1440 and 1.25x (a laptop):
    /// `cargo test --release -p cnde-settings scroll_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn scroll_bench() {
        let (w, h, scale) = (2560u32, 1440u32, 1.25);
        let ui0 = ui(Page::Appearance);
        let theme = Theme::for_appearance(&ui0.config.appearance);
        let mut res = Res {
            fonts: Fonts::new(theme.font_families),
            icons: Icons::new(),
            theme,
            anim: Anim::new(),
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for (page, frosted) in [
            (Page::Appearance, false),
            (Page::Appearance, true),
            (Page::Accessibility, false),
        ] {
            let mut ui = ui(page);
            ui.frosted = frosted;
            let frames = 60;
            let mut total = std::time::Duration::ZERO;
            let mut worst = std::time::Duration::ZERO;
            for i in 0..frames {
                ui.scroll = (i * 12) as f32;
                let started = std::time::Instant::now();
                let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
                let painted = paint(&ui, &mut res, &mut canvas);
                let took = started.elapsed();
                ui.scroll = ui.scroll.min(painted.max_scroll);
                total += took;
                worst = worst.max(took);
            }
            println!(
                "{page:?}{}: {:.1} ms a frame, worst {:.1} ms",
                if frosted { " frosted" } else { "" },
                total.as_secs_f64() * 1000.0 / frames as f64,
                worst.as_secs_f64() * 1000.0
            );
        }
    }

    /// `CNDE_PREVIEW=out.png CNDE_PREVIEW_PAGE=wifi|appearance|power|displays
    /// CNDE_PREVIEW_THEME=quiet|quiet-light|light` renders a page to a PNG.
    #[test]
    #[ignore]
    fn preview() {
        let Some(path) = std::env::var_os("CNDE_PREVIEW") else {
            return;
        };
        let name = std::env::var("CNDE_PREVIEW_PAGE").unwrap_or_default();
        let page = Page::ALL
            .into_iter()
            .find(|p| format!("{p:?}").eq_ignore_ascii_case(&name))
            .unwrap_or(Page::Appearance);
        let mut ui = ui(page);
        ui.hover = Some(Target::Page(Page::Bluetooth));
        if page == Page::Wifi {
            use crate::wifi::{Backend, Network, Security, State};
            let network = |ssid: &str, signal, security, known, connected| Network {
                ssid: ssid.into(),
                signal,
                security,
                known,
                connected,
                in_range: signal > 0,
                autoconnect: known,
            };
            ui.wifi = Some(State {
                backend: Backend::NetworkManager,
                has_device: true,
                enabled: true,
                networks: vec![
                    network("Home", 82, Security::PskSae, true, true),
                    network("Cafe Guest", 55, Security::Open, false, false),
                    network("Neighbour 5G", 30, Security::Sae, false, false),
                ],
                connecting: None,
                error: None,
                details: None,
            });
        }
        if page == Page::DefaultApps {
            let root = std::env::temp_dir().join(format!("cnde-preview-apps-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("cfg")).unwrap();
            std::fs::write(
                root.join("cfg/mimeapps.list"),
                "[Default Applications]\nx-scheme-handler/http=firefox.desktop;\n",
            )
            .unwrap();
            ui.default_apps = Some(crate::defaults::fake_state(&root));
            let _ = std::fs::remove_dir_all(&root);
        }
        if let Ok(v) = std::env::var("CNDE_PREVIEW_SCROLL") {
            ui.scroll = v.parse().unwrap_or(0.0);
        }
        let (palette, scheme) = match std::env::var("CNDE_PREVIEW_THEME").as_deref() {
            Ok("quiet-light") => (cnde_config::Palette::Quiet, cnde_config::ColorScheme::Light),
            Ok("light") => (
                cnde_config::Palette::Adwaita,
                cnde_config::ColorScheme::Light,
            ),
            Ok("adwaita") => (
                cnde_config::Palette::Adwaita,
                cnde_config::ColorScheme::Dark,
            ),
            Ok("dusk") => (cnde_config::Palette::Cnde, cnde_config::ColorScheme::Dark),
            Ok("custom") => (cnde_config::Palette::Custom, cnde_config::ColorScheme::Dark),
            _ => (cnde_config::Palette::Quiet, cnde_config::ColorScheme::Dark),
        };
        ui.config.appearance.palette = palette;
        ui.config.appearance.color_scheme = scheme;
        let dir = std::path::Path::new(&path).parent().unwrap().to_path_buf();
        let stem = std::path::Path::new(&path).file_stem().unwrap().to_owned();
        // render() saves into CNDE_SNAPSHOTS.
        unsafe { std::env::set_var("CNDE_SNAPSHOTS", &dir) };
        render(&ui, stem.to_str().unwrap());
    }

    fn mock_sound() -> crate::sound::State {
        use crate::sound::{App, Availability, Device, Recorder, State};
        let device = |index, name: &str, icon, percent, default| Device {
            index,
            name: name.into(),
            description: name.into(),
            icon,
            percent,
            muted: false,
            default,
            available: true,
            monitor: false,
        };
        let app = |key: &str, name: &str, sink: &str, volume, muted| App {
            key: key.into(),
            name: name.into(),
            ids: vec![1],
            volume,
            muted,
            sink: sink.into(),
            icon: String::new(),
            binary: key.into(),
        };
        State {
            availability: Availability::Ready,
            outputs: vec![
                device(1, "Focusrite Scarlett 2i2 USB", "audio-card-symbolic", 72, true),
                device(2, "Sony WH-1000XM4", "audio-headphones-symbolic", 40, false),
                device(3, "HDMI / DisplayPort 1", "video-display-symbolic", 100, false),
            ],
            inputs: vec![
                device(10, "Scarlett 2i2 Input 1", "audio-input-microphone-symbolic", 65, true),
                device(11, "Webcam C920 Microphone", "audio-input-microphone-symbolic", 90, false),
            ],
            apps: vec![
                app("spotify", "Spotify", "Focusrite Scarlett 2i2 USB", 85, false),
                app("firefox", "Firefox", "Focusrite Scarlett 2i2 USB", 60, true),
                app("mpv", "mpv Media Player", "Sony WH-1000XM4", 100, false),
            ],
            recorders: vec![Recorder {
                source: "Scarlett 2i2 Input 1".into(),
                app: "OBS Studio".into(),
            }],
        }
    }

    /// The Sound page with a mock mixer, plain, mid-drag and with the
    /// "Play on" menu open (`CNDE_SNAPSHOTS=dir` saves them to look at).
    #[test]
    fn sound_page_paints_the_mixer() {
        let mut ui = ui(Page::Sound);
        ui.sound = Some(mock_sound());
        let painted = render(&ui, "Sound");
        let layout = painted.mixer.as_ref().expect("mixer painted");
        assert_eq!(layout.sections.len(), 3);
        assert_eq!(layout.sections[0].apps.len(), 2);
        // Every mixer hit made it to the pointer's list.
        assert!(
            painted
                .hits
                .iter()
                .any(|(_, t)| matches!(t, Target::Mixer(crate::mixer::Hit::AppIcon(0))))
        );
        let state = ui.sound.clone().unwrap();
        let hdmi = layout.sections[2].head;
        ui.sound_drag = Some(crate::mixer::Drag {
            app: state.apps[0].id(),
            origin: (500.0, 300.0),
            pos: (f64::from(hdmi.x + 200.0), f64::from(hdmi.y + 20.0)),
            moved: true,
        });
        let dragging = render(&ui, "Sound-drag");
        assert_eq!(
            dragging.mixer.as_ref().unwrap().drop_target(ui.sound_drag.as_ref().unwrap().pos),
            Some(2)
        );
        ui.sound_drag = None;
        ui.sound_menu = Some(state.apps[0].id());
        let menu = render(&ui, "Sound-menu");
        assert!(menu.mixer.as_ref().unwrap().menu.is_some());
        ui.sound_menu = None;
        ui.config.appearance.palette = cnde_config::Palette::Adwaita;
        ui.config.appearance.color_scheme = cnde_config::ColorScheme::Light;
        render(&ui, "Sound-light");
        ui.sound.as_mut().unwrap().apps.clear();
        render(&ui, "Sound-empty");
    }

    #[test]
    fn every_page_paints_with_targets() {
        for page in Page::ALL {
            let mut ui = ui(page);
            let painted = render(&ui, &format!("{page:?}"));
            assert!(painted.hit((20.0, 130.0)).is_some(), "{page:?}: sidebar");
            ui.advanced = true;
            render(&ui, &format!("{page:?}-advanced"));
        }
    }

    /// Search lists matching rows from every page, with their controls.
    #[test]
    fn search_lists_rows_across_pages() {
        let mut ui = ui(Page::Appearance);
        ui.search = "night light".into();
        let painted = render(&ui, "search");
        assert!(painted.rows.iter().any(|r| r.key == Key::Path("night_light.schedule")));
        assert!(painted.rows.iter().all(|r| matches!(r.key, Key::Path(_))));
        ui.search = "open terminal".into();
        let painted = render(&ui, "search-shortcut");
        assert!(painted.rows.iter().any(|r| r.key == Key::Shortcut("terminal")));
        ui.capturing = Some(Key::Shortcut("terminal"));
        ui.search = "zzzz".into();
        let painted = render(&ui, "search-none");
        assert!(painted.rows.is_empty());
    }

    #[test]
    fn dropdown_popover_and_entry() {
        let mut ui = ui(Page::Panel);
        ui.advanced = true;
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

    #[test]
    fn wifi_page_shows_details_and_join_forms() {
        use crate::wifi::{Backend, Details, Join, Network, Security, State};
        use pages::{JoinField, WifiKey};
        let mut ui = ui(Page::Wifi);
        let network = |ssid: &str, signal, security, known, connected| Network {
            ssid: ssid.into(),
            signal,
            security,
            known,
            connected,
            in_range: signal > 0,
            autoconnect: known,
        };
        ui.wifi = Some(State {
            backend: Backend::NetworkManager,
            has_device: true,
            enabled: true,
            networks: vec![
                network("Home", 82, Security::PskSae, true, true),
                network("Cafe Guest", 55, Security::Open, false, false),
                network("Neighbour 5G", 30, Security::Sae, false, false),
                network("eduroam", 64, Security::Enterprise, false, false),
                network("Office", 0, Security::Psk, true, false),
            ],
            connecting: None,
            error: None,
            details: Some(Details {
                interface: "wlan0".into(),
                security: "WPA3 Personal".into(),
                dbm: Some(-52),
                frequency: Some(5180),
                bitrate: Some(866),
                ipv4: vec!["10.0.0.160/24".into()],
                ipv6: vec!["2001:db8::5/64".into()],
                gateway: Some("10.0.0.1".into()),
                dns: vec!["10.0.0.1".into()],
                mac: Some("3c:a9:f4:00:11:22".into()),
                bssid: Some("aa:bb:cc:dd:ee:ff".into()),
            }),
        });
        let has = |painted: &Painted, key: Key| painted.rows.iter().any(|r| r.key == key);

        ui.wifi_join = Some(Join {
            ssid: "Neighbour 5G".into(),
            security: Security::Sae,
            ..Join::default()
        });
        let painted = render(&ui, "wifi");
        assert!(has(&painted, Key::Wifi(WifiKey::Join(JoinField::Password))));
        assert!(has(&painted, Key::Wifi(WifiKey::Network("Home".into()))));
        assert!(!has(&painted, Key::Wifi(WifiKey::Detail("Band".into()))));
        assert!(!has(
            &painted,
            Key::Wifi(WifiKey::Join(JoinField::Identity))
        ));
        assert!(!painted.hits.iter().any(|(_, t)| *t == Target::Back));
        // Saved networks out of range are listed apart, not to join.
        assert!(has(&painted, Key::Wifi(WifiKey::Saved("Office".into()))));
        assert!(!has(&painted, Key::Wifi(WifiKey::Network("Office".into()))));

        // The connected network's details are a page of their own.
        ui.wifi_details = Some("Home".into());
        let painted = render(&ui, "wifi-details");
        assert!(has(&painted, Key::Wifi(WifiKey::Actions("Home".into()))));
        assert!(has(&painted, Key::Wifi(WifiKey::Detail("Band".into()))));
        assert!(!has(
            &painted,
            Key::Wifi(WifiKey::Network("Cafe Guest".into()))
        ));
        assert!(painted.hits.iter().any(|(_, t)| *t == Target::Back));
        // Not for a network that isn't the connected one.
        ui.wifi_details = Some("Cafe Guest".into());
        let painted = render(&ui, "wifi-not-connected");
        assert!(!painted.hits.iter().any(|(_, t)| *t == Target::Back));
        // A saved one's, out of range: forget it or stop it joining.
        ui.wifi_details = Some("Office".into());
        let painted = render(&ui, "wifi-saved");
        assert!(painted.hits.iter().any(|(_, t)| *t == Target::Back));
        assert!(has(
            &painted,
            Key::Wifi(WifiKey::AutoConnect("Office".into()))
        ));
        assert!(has(&painted, Key::Wifi(WifiKey::Actions("Office".into()))));
        assert!(!has(&painted, Key::Wifi(WifiKey::Detail("Band".into()))));
        ui.wifi_details = None;

        ui.wifi_join = Some(Join {
            ssid: "eduroam".into(),
            security: Security::Enterprise,
            identity: "me@uni.edu".into(),
            password: "secret".into(),
            ..Join::default()
        });
        ui.scroll = 560.0;
        let painted = render(&ui, "wifi-enterprise");
        for field in [
            JoinField::Eap,
            JoinField::Phase2,
            JoinField::Identity,
            JoinField::Password,
            JoinField::CaCert,
            JoinField::Domain,
            JoinField::Submit,
        ] {
            assert!(has(&painted, Key::Wifi(WifiKey::Join(field))), "{field:?}");
        }

        ui.wifi_join = Some(Join {
            hidden: true,
            security: Security::Psk,
            ..Join::default()
        });
        ui.scroll = 540.0;
        let painted = render(&ui, "wifi-hidden");
        assert!(has(&painted, Key::Wifi(WifiKey::Join(JoinField::Ssid))));
        assert!(has(&painted, Key::Wifi(WifiKey::Join(JoinField::Security))));
    }

    #[test]
    fn bluetooth_page_shows_devices_and_pairing() {
        use crate::bluetooth::{Availability, Busy, Device, State};
        use pages::BluetoothKey;
        let mut ui = ui(Page::Bluetooth);
        render(&ui, "bluetooth-looking");
        let device = |address: &str, name: &str, icon: &str, paired, connected| Device {
            address: address.into(),
            name: name.into(),
            icon: icon.into(),
            paired,
            connected,
            battery: None,
            rssi: None,
        };
        let mut state = State {
            availability: Availability::Ready,
            powered: true,
            discovering: true,
            devices: vec![
                Device {
                    battery: Some(70),
                    ..device("01", "Buds", "audio-headphones", true, true)
                },
                device("02", "Trackball", "input-mouse", true, false),
                device("03", "Speaker", "audio-card", false, false),
                device("04", "Keys", "input-keyboard", false, false),
            ],
            busy: Some(("04".into(), Busy::Pairing)),
            code: Some("123456".into()),
            error: None,
        };
        ui.bluetooth = Some(state.clone());
        let has = |painted: &Painted, key: BluetoothKey| {
            painted
                .rows
                .iter()
                .find(|r| r.key == Key::Bluetooth(key.clone()))
                .cloned()
        };
        let painted = render(&ui, "bluetooth");
        assert!(has(&painted, BluetoothKey::Powered).is_some());
        let buds = has(&painted, BluetoothKey::Device("01".into())).unwrap();
        assert_eq!(buds.subtitle, "Connected · Headphones · 70% battery");
        let labels = |row: &Row| match &row.control {
            Control::Segmented(c) => c.iter().map(|c| c.label.clone()).collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert_eq!(labels(&buds), ["Disconnect", "Forget"]);
        let ball = has(&painted, BluetoothKey::Device("02".into())).unwrap();
        assert_eq!(labels(&ball), ["Connect", "Forget"]);
        let speaker = has(&painted, BluetoothKey::Device("03".into())).unwrap();
        assert_eq!(labels(&speaker), ["Pair"]);
        let keys = has(&painted, BluetoothKey::Device("04".into())).unwrap();
        assert!(keys.subtitle.starts_with("Pairing…"));
        let code = has(&painted, BluetoothKey::Code).unwrap();
        assert_eq!(code.value, Value::Str("123456".into()));

        // Off: just the switch.
        state.powered = false;
        ui.bluetooth = Some(state.clone());
        let painted = render(&ui, "bluetooth-off");
        assert_eq!(painted.rows.len(), 1);

        state.availability = Availability::NoAdapter;
        ui.bluetooth = Some(state);
        let painted = render(&ui, "bluetooth-none");
        assert!(painted.rows.is_empty());
    }

    #[test]
    fn custom_palette_paints_both_styles() {
        let mut ui = ui(Page::Appearance);
        ui.config.appearance.palette = cnde_config::Palette::Custom;
        ui.config.appearance.base_color = "#1d4e57".into();
        ui.config.appearance.accent_color = "#ffa94d".into();
        let painted = render(&ui, "custom-dark");
        assert!(
            painted
                .rows
                .iter()
                .any(|r| r.key == Key::Path("appearance.accent_color")),
            "the colour rows show for a custom palette"
        );
        ui.config.appearance.color_scheme = cnde_config::ColorScheme::Light;
        render(&ui, "custom-light");
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    /// A 40px button with its top at `top`.
    fn anchor(top: f32) -> Rect {
        Rect::new(500.0, top, 200.0, 40.0)
    }

    #[test]
    fn short_list_opens_below_unscrolled() {
        // 3 choices fit under a button near the top.
        let b = dropdown_box(680.0, 100.0, 140.0, 3);
        assert_eq!(b.y, 144.0);
        assert_eq!(b.h, Painter::popover_height(3));
        assert_eq!(b.max_scroll, 0.0);
    }

    #[test]
    fn long_list_flips_up_when_there_is_more_room_above() {
        // 35 choices, a button near the bottom of the window.
        let b = dropdown_box(680.0, 600.0, 640.0, 35);
        assert_eq!(b.y + b.h, 600.0 - DROPDOWN_GAP, "ends just above the button");
        assert!(b.y >= DROPDOWN_MARGIN);
        assert_eq!(b.max_scroll, Painter::popover_height(35) - b.h);
    }

    #[test]
    fn long_list_stays_below_when_there_is_more_room_below() {
        // A button near the top of a short window: below, cut to fit.
        let b = dropdown_box(400.0, 60.0, 100.0, 35);
        assert_eq!(b.y, 104.0);
        assert_eq!(b.y + b.h, 400.0 - DROPDOWN_MARGIN);
        assert!(b.max_scroll > 0.0);
    }

    #[test]
    fn list_never_leaves_the_window() {
        for window_h in [200.0, 400.0, 680.0, 1440.0] {
            for top in [0.0, 50.0, 200.0, 350.0, 600.0] {
                // The button itself is in the window.
                if top + 40.0 > window_h {
                    continue;
                }
                for count in [1, 5, 35, 80] {
                    let b = dropdown_box(window_h, top, top + 40.0, count);
                    assert!(b.h >= 0.0 && b.y >= 0.0, "{window_h} {top} {count}: {b:?}");
                    assert!(b.y + b.h <= window_h, "{window_h} {top} {count}: {b:?}");
                    assert!(b.max_scroll >= 0.0);
                }
            }
        }
    }

    #[test]
    fn reveal_moves_the_list_only_as_far_as_needed() {
        let b = DropdownBox {
            y: 0.0,
            h: 200.0,
            max_scroll: 500.0,
        };
        // Already in view: no change.
        assert_eq!(dropdown_reveal(0.0, 0, b), 0.0);
        // Choice 10 is far below: scrolled just far enough to show its bottom.
        let s = dropdown_reveal(0.0, 10, b);
        assert_eq!(s, CHOICE_PAD + CHOICE_ROW * 11.0 - 200.0);
        // Choice 3 is above a list scrolled past it: its top meets the edge.
        assert_eq!(dropdown_reveal(300.0, 3, b), CHOICE_PAD + CHOICE_ROW * 3.0);
        // Never past the ends of the list.
        assert_eq!(dropdown_reveal(0.0, 34, b), 500.0);
    }

    #[test]
    fn opening_scrolls_the_selected_choice_into_view() {
        let s = dropdown_open_scroll(400.0, anchor(60.0), 35, Some(30));
        let b = dropdown_box(400.0, 60.0, 100.0, 35);
        assert!(CHOICE_PAD + CHOICE_ROW * 31.0 - s <= b.h);
        assert!(s <= b.max_scroll);
        assert_eq!(dropdown_open_scroll(400.0, anchor(60.0), 35, None), 0.0);
    }
}
