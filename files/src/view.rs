//! Painting the window: sidebar, top bar and the folder list. Every paint
//! records where each control ended up ([`Target`]s) for the pointer to hit.
//! The list's rows are not recorded (there can be tens of thousands): they
//! are found from the pointer's y instead.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use cnde_config::Config;
use cnde_ui::{Canvas, Color, Fonts, Icons, Painter, Rect, TextLayout, Theme, Weight};

use crate::{
    folder::{Folder, Location},
    grid, info,
    model::{self, Drive, SortKey},
    ops,
    pages::{self, Pages},
    search::{self, Search},
    transfer, zoom,
};

pub const SIDE_W: f32 = 228.0;
pub const TOP_H: f32 = 48.0;
const TITLE_H: f32 = 56.0;
const HEAD_H: f32 = 28.0;
pub const ROW_H: f32 = 30.0;
/// Inside the list: space either side of the rows.
const LIST_PAD: f32 = 12.0;
const LIST_PAD_RIGHT: f32 = 16.0;
const COL_GAP: f32 = 10.0;
/// A drive nearly full turns amber.
pub const WARN: Color = Color::rgb(0xe5, 0xa5, 0x0a);
pub const WARN_AT: f32 = 0.85;
/// How opaque a frosted sidebar's tint is over the compositor's frost.
const FROSTED_SIDEBAR: f32 = 0.6;
/// More drives than this get one compact line each.
const CARD_DRIVES: usize = 4;
const PLACE_H: f32 = 32.0;
const HEADING_H: f32 = 30.0;
const CARD_H: f32 = 74.0;
const COMPACT_H: f32 = 36.0;
const SEARCH_BOTTOM: f32 = 52.0;
const TRASH_H: f32 = 44.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    List,
    Grid,
}

/// Something on screen the pointer can hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Back,
    Forward,
    /// Index into [`Painted::crumbs`].
    Crumb(usize),
    Mode(ViewMode),
    Info,
    Menu,
    /// The icon size slider in the menu.
    Zoom,
    Close,
    Minimize,
    Maximize,
    /// Empty top bar: drag to move the window.
    Header,
    Search,
    Overview,
    Recent,
    /// A row of the recent list: the file, or the folder it is in.
    RecentFile(usize),
    RecentFolder(usize),
    Place(usize),
    Drive(usize),
    Eject(usize),
    Connect,
    Trash,
    Sort(SortKey),
    /// An info panel button or a context menu row.
    Act(ops::Act),
}

/// A shortcut in the sidebar.
pub struct Place {
    pub name: String,
    pub path: PathBuf,
    pub icon: &'static str,
}

pub struct Ui {
    pub config: Config,
    pub location: Location,
    pub folder: Folder,
    pub mode: ViewMode,
    /// Icon sizes of the grid and of the list.
    pub grid_size: f32,
    pub list_size: f32,
    pub info_panel: bool,
    pub drives: Vec<Drive>,
    pub trash: Option<u64>,
    pub places: Vec<Place>,
    pub home: PathBuf,
    pub can_back: bool,
    pub can_forward: bool,
    pub hover: Option<Target>,
    /// The list row under the pointer, as a position in the folder's rows.
    pub hover_row: Option<usize>,
    pub scroll: f32,
    pub side_scroll: f32,
    pub error: Option<String>,
    /// Square corners when maximized or tiled.
    pub rounded: bool,
    /// The sidebar shows the compositor's frost through it.
    pub frosted: bool,
    /// The keyboard moved the cursor last: show it.
    pub keyboard_focus: bool,
    pub pages: Pages,
    pub search: Search,
    /// Info panel, rename, menu and refresh state.
    pub x: ops::Extra,
}

/// The result of a paint, for hit-testing.
#[derive(Default)]
pub struct Painted {
    pub hits: Vec<(Rect, Target)>,
    pub crumbs: Vec<PathBuf>,
    pub list: Rect,
    /// How many of the last hits are rows of the open context menu; while
    /// there is one, nothing else can be hit.
    pub menu_rows: usize,
    /// The icon size slider of the open menu.
    pub slider: Rect,
}

impl Painted {
    /// The topmost target under `point`.
    pub fn hit(&self, point: (f64, f64)) -> Option<Target> {
        let live = if self.menu_rows > 0 {
            self.menu_rows
        } else {
            self.hits.len()
        };
        self.hits
            .iter()
            .rev()
            .take(live)
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, t)| *t)
    }
}

/// Shaped text kept between paints, so scrolling doesn't shape any.
#[derive(Default)]
pub struct TextCache {
    stamp: (u64, u32, i32, u32),
    pub(crate) map: HashMap<(usize, u8), TextLayout>,
    now: Option<SystemTime>,
}

impl TextCache {
    pub(crate) fn sync(&mut self, version: u64, scale: f32, name_w: f32, size: f32) {
        let stamp = (version, scale.to_bits(), name_w as i32, size.to_bits());
        if stamp != self.stamp || self.map.len() > 6000 {
            self.stamp = stamp;
            self.map.clear();
            self.now = Some(SystemTime::now());
        }
    }

    pub fn clear(&mut self) {
        self.stamp = (u64::MAX, 0, 0, 0);
        self.map.clear();
    }
}

pub struct Res {
    pub fonts: Fonts,
    pub icons: Icons,
    pub theme: Theme,
    pub text: TextCache,
}

impl Res {
    pub fn new(theme: Theme, icons: Icons) -> Self {
        Self {
            fonts: Fonts::new(theme.font_families),
            icons,
            theme,
            text: TextCache::default(),
        }
    }
}

/// Width taken by the info panel (0 when it's off).
pub fn panel_w(ui: &Ui) -> f32 {
    if ui.info_panel && matches!(ui.location, Location::Dir(_)) {
        info::PANEL_W
    } else {
        0.0
    }
}

/// The list's area for a window `w` x `h` (logical). The grid has no
/// column header, so it starts higher.
pub fn list_rect(ui: &Ui, w: f32, h: f32) -> Rect {
    // The pages have no grid: they lie where the list does, in both modes.
    let head = if ui.mode == ViewMode::List || !matches!(ui.location, Location::Dir(_)) {
        HEAD_H + 2.0
    } else {
        0.0
    };
    let top = TOP_H + TITLE_H + head;
    Rect::new(SIDE_W, top, list_w(ui, w), (h - top).max(0.0))
}

/// Width of the list area in a window `w` wide.
pub fn list_w(ui: &Ui, w: f32) -> f32 {
    (w - SIDE_W - panel_w(ui)).max(0.0)
}

/// Items per row and a row's height, in a list area `list_w` wide.
pub fn cells(ui: &Ui, list_w: f32) -> (usize, f32) {
    match ui.mode {
        ViewMode::List => (1, zoom::row_h(ui.list_size)),
        ViewMode::Grid => {
            let g = grid::geo(list_w, ui.grid_size);
            (g.cols, g.tile_h)
        }
    }
}

/// Height of all the rows in a list area `list_w` wide.
pub fn content_height(ui: &Ui, list_w: f32) -> f32 {
    let (cols, row_h) = cells(ui, list_w);
    ui.folder.visible.len().div_ceil(cols) as f32 * row_h + 8.0
}

/// How far the list can scroll in a window `w` x `h`.
pub fn max_scroll(ui: &Ui, w: f32, h: f32) -> f32 {
    let list = list_rect(ui, w, h);
    if !matches!(ui.location, Location::Dir(_)) {
        return (pages::content_height(ui) - list.h).max(0.0);
    }
    (content_height(ui, list_w(ui, w)) - list.h).max(0.0)
}

/// The item under a point of the list area, if any.
pub fn pos_at(ui: &Ui, list: Rect, point: (f64, f64)) -> Option<usize> {
    let n = ui.folder.visible.len();
    match ui.mode {
        ViewMode::Grid => grid::pos_at(list, grid::geo(list.w, ui.grid_size), ui.scroll, n, point),
        ViewMode::List => {
            let row_h = zoom::row_h(ui.list_size);
            let pos = ((point.1 as f32 - list.y - 4.0 + ui.scroll) / row_h).floor();
            (pos >= 0.0 && (pos as usize) < n).then_some(pos as usize)
        }
    }
}

fn side_band(h: f32) -> (f32, f32) {
    (SEARCH_BOTTOM, h - TRASH_H)
}

fn side_content_height(ui: &Ui) -> f32 {
    let places = (2 + ui.places.len()) as f32 * (PLACE_H + 1.0);
    let drives = if ui.drives.len() > CARD_DRIVES {
        COMPACT_H
    } else {
        CARD_H
    } * ui.drives.len() as f32;
    places + HEADING_H + drives + HEADING_H + PLACE_H
}

/// How far the sidebar can scroll in a window `h` tall.
pub fn side_max_scroll(ui: &Ui, h: f32) -> f32 {
    let (top, bottom) = side_band(h);
    (side_content_height(ui) - (bottom - top)).max(0.0)
}

/// The folder's name for the large title.
pub fn title_of(ui: &Ui) -> String {
    match &ui.location {
        Location::Overview => "Overview".into(),
        Location::Recent => "Recent".into(),
        Location::Dir(_) if ui.search.active() => format!("Search \"{}\"", ui.search.query),
        Location::Dir(path) => {
            if *path == ui.home {
                "Home".into()
            } else {
                path.file_name()
                    .map_or_else(|| "/".into(), |n| n.to_string_lossy().into_owned())
            }
        }
    }
}

/// The drive a path is on: the one with the longest matching mount point.
pub fn drive_of<'a>(drives: &'a [Drive], path: &Path) -> Option<&'a Drive> {
    drives
        .iter()
        .filter(|d| path.starts_with(&d.mount))
        .max_by_key(|d| d.mount.as_os_str().len())
}

fn subtitle_of(ui: &Ui) -> String {
    let path = match &ui.location {
        Location::Overview => return pages::overview_subtitle(ui),
        Location::Recent => return pages::recent_subtitle(ui),
        Location::Dir(path) => path,
    };
    let n = ui.folder.visible.len();
    if ui.search.active() {
        let all = ui.folder.entries.iter();
        let total = all.filter(|e| ui.folder.show_hidden || !e.hidden).count();
        return format!("{n} of {total} items");
    }
    let mut s = if ui.folder.loading && n == 0 {
        "Loading…".to_string()
    } else {
        format!("{n} {}", if n == 1 { "item" } else { "items" })
    };
    if let Some(d) = drive_of(&ui.drives, path) {
        s += &format!(" · on {}", d.name);
    }
    if ui.folder.loading && n > 0 {
        s += " · loading…";
    }
    s
}

/// Labels and paths for the parents of `path`, nearest last.
pub fn crumbs(path: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if path == home {
        return vec![("~".into(), home.to_path_buf())];
    }
    let mut cur = path.parent();
    while let Some(p) = cur {
        let label = if p == home {
            "~".to_string()
        } else {
            p.file_name()
                .map_or_else(|| "/".into(), |n| n.to_string_lossy().into_owned())
        };
        out.push((label, p.to_path_buf()));
        if p == home {
            break;
        }
        cur = p.parent();
    }
    out.reverse();
    out
}

pub(crate) fn icon_for(entry: &model::Entry) -> &'static str {
    icon_for_name(&entry.name, entry.is_dir)
}

pub fn icon_for_name(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder-symbolic";
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" | "tiff" | "ico" => {
            "image-x-generic-symbolic"
        }
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" => "audio-x-generic-symbolic",
        "mp4" | "mkv" | "webm" | "mov" | "avi" => "video-x-generic-symbolic",
        "zip" | "tar" | "gz" | "xz" | "zst" | "bz2" | "7z" | "rar" => "package-x-generic-symbolic",
        "ttf" | "otf" | "woff" | "woff2" => "font-x-generic-symbolic",
        "so" | "a" => "application-x-sharedlib-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// The column x positions and widths: name, size, modified, kind. Narrow
/// windows drop kind, then modified.
fn columns(inner_x: f32, inner_w: f32) -> [(f32, f32); 4] {
    let (mut size, mut modified, mut kind) = (84.0, 120.0, 110.0);
    let name_needs = |m: f32, k: f32| inner_w - (84.0 + m + k + COL_GAP * 3.0);
    if name_needs(modified, kind) < 160.0 {
        kind = 0.0;
    }
    if name_needs(modified, kind) < 160.0 {
        modified = 0.0;
    }
    if name_needs(modified, kind) < 100.0 {
        size = 70.0;
    }
    let gaps = [(size, 1.0), (modified, 1.0), (kind, 1.0)]
        .iter()
        .filter(|(w, _)| *w > 0.0)
        .count() as f32;
    let name_w = (inner_w - size - modified - kind - COL_GAP * gaps).max(40.0);
    let mut x = inner_x + name_w + COL_GAP;
    let mut out = [(inner_x, name_w), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)];
    for (i, w) in [size, modified, kind].into_iter().enumerate() {
        out[i + 1] = (x, w);
        if w > 0.0 {
            x += w + COL_GAP;
        }
    }
    out
}

/// Text shortened with "…" to fit `max_w`.
pub(crate) fn fit(
    fonts: &mut Fonts,
    text: &str,
    size: f32,
    weight: Weight,
    scale: f32,
    max_w: f32,
) -> TextLayout {
    let full = fonts.layout(text, size, weight, scale);
    if full.width <= max_w {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    let mut best = None;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cut: String = chars[..mid]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
            + "…";
        let l = fonts.layout(&cut, size, weight, scale);
        if l.width <= max_w {
            best = Some(l);
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    best.unwrap_or_else(|| fonts.layout("…", size, weight, scale))
}

pub(crate) struct Colors {
    pub(crate) ink: Color,
    pub(crate) dim: Color,
    pub(crate) hair: Color,
    pub(crate) on: Color,
    pub(crate) sel: Color,
    pub(crate) fold: Color,
    pub(crate) seg: Color,
    pub(crate) seg_on: Color,
    pub(crate) track: Color,
}

impl Colors {
    pub(crate) fn new(t: &Theme) -> Self {
        let ink = t.window_text;
        let dark = t.dark;
        let a = |dark_a: f32, light_a: f32| ink.with_alpha(if dark { dark_a } else { light_a });
        Self {
            ink,
            dim: t.window_dim,
            hair: a(0.08, 0.09),
            on: a(0.12, 0.07),
            sel: t.accent.with_alpha(if dark { 0.26 } else { 0.16 }),
            fold: if dark {
                t.accent.mix(Color::rgb(255, 255, 255), 0.25)
            } else {
                t.accent
            },
            seg: a(0.07, 0.05),
            seg_on: if dark {
                ink.with_alpha(0.16)
            } else {
                Color::rgb(255, 255, 255)
            },
            track: a(0.14, 0.10),
        }
    }
}

pub fn paint(ui: &Ui, res: &mut Res, canvas: &mut Canvas<'_>) -> Painted {
    let (w, h) = canvas.size();
    let scale = canvas.scale();
    let theme = res.theme.clone();
    let t = &theme;
    let c = Colors::new(t);
    let mut out = Painted::default();
    let mut p = Painter {
        canvas,
        fonts: &mut res.fonts,
        icons: &mut res.icons,
        theme: t,
    };
    let radius = if ui.rounded { t.window_radius } else { 0.0 };
    let list = list_rect(ui, w, h);
    // The info panel takes the right edge; everything else lays out in `wm`.
    let wm = w - panel_w(ui);
    out.list = list;

    if ui.rounded && ui.frosted {
        p.canvas.clear(Color::rgba(0, 0, 0, 0.0));
        // Only the main area is solid; the sidebar, drawn over nothing,
        // lets the frost behind the window show.
        p.canvas.rounded_rect_clipped(
            SIDE_W - radius,
            0.0,
            w - SIDE_W + radius,
            h,
            radius,
            t.window_background,
            (SIDE_W, 0.0, w - SIDE_W, h),
        );
    } else {
        p.canvas.rounded_fill(radius, t.window_background);
    }

    // The rows first: they scroll under the bars drawn after them.
    if matches!(ui.location, Location::Dir(_)) {
        if ui.mode == ViewMode::Grid {
            grid::paint(ui, &mut p, &mut res.text, &c, &out, h, scale);
        } else {
            rows(ui, &mut p, &mut res.text, &c, &mut out, wm, h, scale);
        }
    } else {
        pages::paint(ui, &mut p, &mut res.text, &c, &mut out, w, h, scale);
    }

    // Everything above the rows is solid, so rows scroll under it.
    p.canvas.rounded_rect_clipped(
        SIDE_W - radius,
        0.0,
        w - SIDE_W + radius,
        list.y + radius + 4.0,
        radius,
        t.window_background,
        (SIDE_W, 0.0, w - SIDE_W, list.y),
    );
    if panel_w(ui) > 0.0 {
        info::paint(ui, &mut p, &c, &mut out, w, h);
    }
    top_bar(ui, &mut p, &c, &mut out, w);
    title(ui, &mut p, &c, wm);
    if matches!(ui.location, Location::Dir(_)) && ui.mode == ViewMode::List {
        column_header(ui, &mut p, &c, &mut out, wm, list.y - HEAD_H - 2.0);
    }
    sidebar(ui, &mut p, &c, &mut out, h);

    if let Some(error) = &ui.error {
        let size = t.font_size;
        let tw = p
            .measure(error, size, Weight::Normal)
            .min(w - SIDE_W - 48.0);
        let bw = tw + 32.0;
        let (bx, by) = (
            SIDE_W + (w - SIDE_W - bw) / 2.0,
            h - 24.0 - 40.0 - transfer::strip_room(ui),
        );
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
    transfer::paint_strip(ui, &mut p, &c, &mut out, w, h);
    crate::mainmenu::paint(ui, &mut p, &mut out, w, h);
    ops::paint_menu(ui, &mut p, &mut out, w, h);
    out
}

#[allow(clippy::too_many_arguments)]
fn rows(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    cache: &mut TextCache,
    c: &Colors,
    out: &mut Painted,
    w: f32,
    h: f32,
    scale: f32,
) {
    let t = p.theme;
    let list = out.list;
    let folder = &ui.folder;
    let rx = list.x + LIST_PAD;
    let rw = w - rx - LIST_PAD_RIGHT;
    let cols = columns(rx + 10.0, rw - 20.0);
    let icon = ui.list_size;
    let row_h = zoom::row_h(icon);
    let name_off = zoom::name_offset(icon);
    let name_size = t.font_size - 0.5;
    let small = t.font_size - 1.5;
    cache.sync(folder.version, scale, cols[0].1, name_size);

    if status(ui, p, c, list) {
        return;
    }

    let first = ((ui.scroll - 4.0) / row_h).floor().max(0.0) as usize;
    let last = (((ui.scroll + list.h) / row_h).ceil() as usize + 1).min(folder.visible.len());
    let now = cache.now.unwrap_or_else(SystemTime::now);
    for pos in first..last {
        let Some(entry) = folder.entry(pos) else {
            continue;
        };
        let index = folder.visible[pos];
        let y = list.y + 4.0 + pos as f32 * row_h - ui.scroll;
        if y + row_h < list.y - row_h || y > h {
            continue;
        }
        let selected = folder.is_selected(pos);
        if selected {
            p.canvas.rounded_rect(rx, y, rw, row_h, 8.0, c.sel);
        } else if ui.hover_row == Some(pos) {
            p.canvas.rounded_rect(rx, y, rw, row_h, 8.0, t.hover);
        }
        if ui.keyboard_focus && folder.cursor == Some(pos) {
            p.canvas
                .stroke_rounded_rect(rx, y, rw, row_h, 8.0, 2.0, t.focus_ring);
        }
        if ui.x.dnd.over == Some(crate::dnd::Over::Row(pos)) {
            p.canvas.rounded_rect(rx, y, rw, row_h, 8.0, c.sel);
            p.canvas
                .stroke_rounded_rect(rx, y, rw, row_h, 8.0, 2.0, t.accent);
        }
        let cy = y + row_h / 2.0;
        let ink = c.ink;
        let icon_color = if entry.is_dir { c.fold } else { c.dim };
        let thumb = if entry.is_dir {
            None
        } else {
            crate::thumbs::get(&entry.path, entry.modified, crate::thumbs::NORMAL, true)
        };
        if let Some(pm) = thumb {
            let bx = (cols[0].0, cy - (icon - 2.0) / 2.0, icon + 2.0, icon - 2.0);
            crate::thumbs::draw(p.canvas, &pm, bx, 3.0 + icon / 16.0);
        } else {
            p.icon(
                icon_for(entry),
                cols[0].0 + (icon + 2.0) / 2.0,
                cy,
                icon,
                icon_color,
            );
        }

        let mut draw = |p: &mut Painter<'_, '_>,
                        col: u8,
                        text: &dyn Fn() -> String,
                        x: f32,
                        width: f32,
                        right: bool,
                        size: f32,
                        color: Color| {
            if width <= 0.0 {
                return;
            }
            let key = (index, col);
            let mut layout = match cache.map.remove(&key) {
                Some(l) => l,
                None => fit(p.fonts, &text(), size, Weight::Normal, scale, width),
            };
            let tx = if right { x + width - layout.width } else { x };
            let ty = cy - layout.height / 2.0;
            p.canvas.text(p.fonts, &mut layout, tx, ty, color);
            cache.map.insert(key, layout);
        };
        if ui.x.rename.as_ref().is_some_and(|r| r.name == entry.name) {
            let rect = Rect::new(
                cols[0].0 + name_off - 8.0,
                y + 2.0,
                cols[0].1 - name_off + 8.0,
                row_h - 4.0,
            );
            ops::paint_rename(ui, p, c, rect);
        } else if ui.search.active() {
            search::paint_name(
                p,
                &ui.search.query,
                &entry.name,
                cols[0].0 + name_off,
                cy,
                cols[0].1 - name_off,
                name_size,
                ink,
                c.fold,
            );
        } else {
            draw(
                p,
                0,
                &|| entry.name.clone(),
                cols[0].0 + name_off,
                cols[0].1 - name_off,
                false,
                name_size,
                ink,
            );
        }
        draw(
            p,
            1,
            &|| match (entry.is_dir, entry.items) {
                (true, Some(n)) => format!("{n} {}", if n == 1 { "item" } else { "items" }),
                (true, None) => "—".into(),
                _ => model::human_size(entry.size),
            },
            cols[1].0,
            cols[1].1,
            true,
            small,
            c.dim,
        );
        draw(
            p,
            2,
            &|| {
                entry
                    .modified
                    .map_or_else(String::new, |m| model::human_time(m, now))
            },
            cols[2].0,
            cols[2].1,
            false,
            small,
            c.dim,
        );
        draw(
            p,
            3,
            &|| entry.kind.clone(),
            cols[3].0,
            cols[3].1,
            false,
            small,
            c.dim,
        );
    }

    // A thin scroll indicator when there's more than fits.
    let content = content_height(ui, list.w);
    if content > list.h {
        let track_h = list.h - 8.0;
        let thumb_h = (track_h * list.h / content).max(24.0);
        let range = (content - list.h).max(1.0);
        let ty = list.y + 4.0 + (track_h - thumb_h) * (ui.scroll / range).clamp(0.0, 1.0);
        p.canvas
            .rounded_rect(w - 7.0, ty, 3.0, thumb_h, 1.5, c.ink.with_alpha(0.25));
    }
}

/// A message in place of the rows (an error, an empty folder); true if shown.
pub(crate) fn status(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, list: Rect) -> bool {
    let folder = &ui.folder;
    if let Some(err) = &folder.error {
        message(p, c, list, err);
        return true;
    }
    if folder.visible.is_empty() {
        if !folder.loading {
            let msg = if ui.search.active() {
                "No matches"
            } else if folder.entries.is_empty() {
                "This folder is empty"
            } else {
                "Only hidden files here (Ctrl+H shows them)"
            };
            message(p, c, list, msg);
        }
        return true;
    }
    false
}

fn message(p: &mut Painter<'_, '_>, c: &Colors, list: Rect, text: &str) {
    let size = p.theme.font_size;
    let tw = p.measure(text, size, Weight::Normal).min(list.w - 40.0);
    p.label_v(
        text,
        list.x + (list.w - tw) / 2.0,
        list.y + 80.0,
        list.w - 40.0,
        size,
        Weight::Normal,
        c.dim,
    );
}

fn column_header(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, out: &mut Painted, w: f32, y: f32) {
    let t = p.theme;
    let rx = SIDE_W + LIST_PAD;
    let rw = w - rx - LIST_PAD_RIGHT;
    let cols = columns(rx + 10.0, rw - 20.0);
    let cy = y + HEAD_H / 2.0 - 1.0;
    let size = t.font_size - 1.5;
    let keys = [
        ("Name", SortKey::Name),
        ("Size", SortKey::Size),
        ("Modified", SortKey::Modified),
        ("Kind", SortKey::Kind),
    ];
    for (i, (label, key)) in keys.into_iter().enumerate() {
        let (x, cw) = cols[i];
        if cw <= 0.0 {
            continue;
        }
        let active = ui.folder.sort == key;
        let lw = p.measure(label, size, Weight::Normal);
        let arrow = if active { 16.0 } else { 0.0 };
        let (lx, hit_x) = if i == 1 {
            (x + cw - lw - arrow, x + cw - lw - arrow - 4.0)
        } else {
            (x, x - 4.0)
        };
        let color = if active { c.ink } else { c.dim };
        p.label_v(label, lx, cy, cw, size, Weight::Normal, color);
        if active {
            let name = if ui.folder.descending {
                "pan-down-symbolic"
            } else {
                "pan-up-symbolic"
            };
            p.icon(name, lx + lw + 9.0, cy, 14.0, color);
        }
        let rect = Rect::new(hit_x, y, lw + arrow + 8.0, HEAD_H);
        if ui.hover == Some(Target::Sort(key)) {
            p.canvas
                .rounded_rect(rect.x, y + 2.0, rect.w, HEAD_H - 4.0, 6.0, t.hover);
            p.label_v(label, lx, cy, cw, size, Weight::Normal, c.ink);
        }
        out.hits.push((rect, Target::Sort(key)));
    }
    p.canvas.rounded_rect(
        SIDE_W + LIST_PAD,
        y + HEAD_H,
        w - SIDE_W - LIST_PAD - LIST_PAD_RIGHT,
        1.0,
        0.0,
        c.hair,
    );
}

fn title(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, w: f32) {
    let t = p.theme;
    let x = SIDE_W + 22.0;
    let cy = TOP_H + 26.0;
    let name = title_of(ui);
    let tw = p.measure(&name, 28.0, Weight::Bold).min(w - x - 160.0);
    p.label_v(&name, x, cy, w - x - 160.0, 28.0, Weight::Bold, c.ink);
    let sub = subtitle_of(ui);
    if !sub.is_empty() {
        p.label_v(
            &sub,
            x + tw + 12.0,
            cy + 4.0,
            (w - x - tw - 24.0).max(0.0),
            t.font_size - 1.0,
            Weight::Normal,
            c.dim,
        );
    }
}

fn top_bar(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, out: &mut Painted, w: f32) {
    let t = p.theme;
    let cy = TOP_H / 2.0;
    out.hits
        .push((Rect::new(SIDE_W, 0.0, w - SIDE_W, TOP_H), Target::Header));

    let button = |p: &mut Painter<'_, '_>,
                  out: &mut Painted,
                  x: f32,
                  icon: &str,
                  target: Target,
                  lit: bool,
                  enabled: bool| {
        let rect = Rect::new(x, cy - 16.0, 32.0, 32.0);
        if lit {
            p.canvas.rounded_rect(rect.x, rect.y, 32.0, 32.0, 8.0, c.on);
        } else if enabled && ui.hover == Some(target) {
            p.canvas
                .rounded_rect(rect.x, rect.y, 32.0, 32.0, 8.0, t.hover);
        }
        let ink = if enabled {
            c.ink
        } else {
            c.ink.with_alpha(0.3)
        };
        p.icon(icon, x + 16.0, cy, 16.0, ink);
        if enabled {
            out.hits.push((rect, target));
        }
    };
    let mut x = SIDE_W + 10.0;
    button(
        p,
        out,
        x,
        "go-previous-symbolic",
        Target::Back,
        false,
        ui.can_back,
    );
    x += 34.0;
    button(
        p,
        out,
        x,
        "go-next-symbolic",
        Target::Forward,
        false,
        ui.can_forward,
    );
    x += 38.0;
    let crumbs_x = x;

    // Right side, from the edge in: window buttons, menu, info, switch.
    let shown = match ui.config.appearance.window_buttons {
        cnde_config::WindowButtons::All => 3,
        cnde_config::WindowButtons::Close => 1,
    };
    let maximize_icon = if ui.rounded {
        "window-maximize-symbolic"
    } else {
        "window-restore-symbolic"
    };
    let size = 22.0;
    for (i, (target, icon)) in [
        (Target::Close, "window-close-symbolic"),
        (Target::Maximize, maximize_icon),
        (Target::Minimize, "window-minimize-symbolic"),
    ]
    .into_iter()
    .take(shown)
    .enumerate()
    {
        let bx = w - 10.0 - size - i as f32 * (size + 8.0);
        let fill = t
            .button_background
            .mix(t.button_hover, (ui.hover == Some(target)) as u8 as f32);
        p.canvas.circle(bx + size / 2.0, cy, size / 2.0, fill);
        p.icon(icon, bx + size / 2.0, cy, 14.0, c.ink);
        out.hits
            .push((Rect::new(bx, cy - size / 2.0, size, size), target));
    }
    let mut rx = w - 10.0 - shown as f32 * (size + 8.0) - 4.0;
    rx -= 32.0;
    button(p, out, rx, "view-more-symbolic", Target::Menu, false, true);
    rx -= 34.0;
    button(
        p,
        out,
        rx,
        "dialog-information-symbolic",
        Target::Info,
        ui.info_panel,
        true,
    );
    rx -= 8.0 + 68.0;
    // The list / grid switch.
    p.canvas.rounded_rect(rx, cy - 16.0, 68.0, 32.0, 9.0, c.seg);
    for (i, (mode, icon)) in [
        (ViewMode::List, "view-list-symbolic"),
        (ViewMode::Grid, "view-grid-symbolic"),
    ]
    .into_iter()
    .enumerate()
    {
        let sx = rx + 3.0 + i as f32 * 31.0;
        if ui.mode == mode {
            p.canvas
                .rounded_rect(sx, cy - 13.0, 31.0, 26.0, 7.0, c.seg_on);
        } else if ui.hover == Some(Target::Mode(mode)) {
            p.canvas
                .rounded_rect(sx, cy - 13.0, 31.0, 26.0, 7.0, t.hover);
        }
        p.icon(icon, sx + 15.5, cy, 16.0, c.ink);
        out.hits
            .push((Rect::new(sx, cy - 13.0, 31.0, 26.0), Target::Mode(mode)));
    }

    // Breadcrumbs of the parent folders, as many as fit.
    let Location::Dir(path) = &ui.location else {
        return;
    };
    let all = crumbs(path, &ui.home);
    let avail = rx - 10.0 - crumbs_x;
    let size = t.font_size - 1.0;
    let widths: Vec<f32> = all
        .iter()
        .map(|(l, _)| p.measure(l, size, Weight::Normal) + 12.0)
        .collect();
    const SEP: f32 = 14.0;
    let mut start = 0;
    while start < all.len()
        && widths[start..].iter().map(|w| w + SEP).sum::<f32>() + if start > 0 { 18.0 } else { 0.0 }
            > avail
    {
        start += 1;
    }
    let mut x = crumbs_x;
    if start > 0 {
        p.label_v("…", x + 6.0, cy, 20.0, size, Weight::Normal, c.dim);
        x += 18.0;
    }
    for i in start..all.len() {
        let rect = Rect::new(x, cy - 13.0, widths[i], 26.0);
        let target = Target::Crumb(out.crumbs.len());
        if ui.hover == Some(target) {
            p.canvas
                .rounded_rect(rect.x, rect.y, rect.w, rect.h, 6.0, t.hover);
        }
        let label_w = (widths[i] - 12.0).max(0.0);
        p.label_v(
            &all[i].0,
            x + 6.0,
            cy,
            label_w + 2.0,
            size,
            Weight::Normal,
            c.dim,
        );
        out.hits.push((rect, target));
        out.crumbs.push(all[i].1.clone());
        x += widths[i];
        p.label_v("›", x + 1.0, cy, SEP, size, Weight::Normal, c.dim);
        x += SEP;
    }
}

fn sidebar(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, out: &mut Painted, h: f32) {
    let t = p.theme;
    let radius = if ui.rounded { t.window_radius } else { 0.0 };
    let fill = if ui.rounded && ui.frosted {
        t.window_sidebar.with_alpha(FROSTED_SIDEBAR)
    } else {
        t.window_sidebar
    };
    // Rounded on the left only, in one piece.
    p.canvas.rounded_rect_clipped(
        0.0,
        0.0,
        SIDE_W + radius,
        h,
        radius,
        fill,
        (0.0, 0.0, SIDE_W, h),
    );
    p.canvas
        .rounded_rect(SIDE_W - 1.0, 0.0, 1.0, h, 0.0, c.hair);
    out.hits
        .push((Rect::new(0.0, 0.0, SIDE_W, 12.0), Target::Header));

    let sr = Rect::new(10.0, 12.0, SIDE_W - 20.0, 32.0);
    search::paint_field(p, &ui.search, sr, c.seg, c.dim);
    out.hits.push((sr, Target::Search));

    let (top, bottom) = side_band(h);
    let mut y = top - ui.side_scroll;
    let size = t.font_size;
    let current = match &ui.location {
        Location::Dir(d) => Some(d),
        _ => None,
    };
    // Whole items only: one cut by the band's edge isn't drawn.
    let fits = |y: f32, height: f32| y >= top - 0.5 && y + height <= bottom + 0.5;

    let place_row = |p: &mut Painter<'_, '_>,
                     out: &mut Painted,
                     y: &mut f32,
                     icon: &str,
                     name: &str,
                     on: bool,
                     target: Target,
                     muted: bool| {
        let rect = Rect::new(10.0, *y, SIDE_W - 20.0, PLACE_H);
        if fits(*y, PLACE_H) {
            if on {
                p.canvas
                    .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, c.on);
            } else if ui.hover == Some(target) {
                p.canvas
                    .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, t.hover);
            }
            let ink = if muted { c.dim } else { c.ink };
            p.icon(icon, rect.x + 18.0, rect.y + PLACE_H / 2.0, 16.0, ink);
            let weight = if on { Weight::Bold } else { Weight::Normal };
            p.label_v(
                name,
                rect.x + 37.0,
                rect.y + PLACE_H / 2.0,
                rect.w - 46.0,
                size,
                weight,
                ink,
            );
            out.hits.push((rect, target));
        }
        *y += PLACE_H + 1.0;
    };
    place_row(
        p,
        out,
        &mut y,
        "view-app-grid-symbolic",
        "Overview",
        ui.location == Location::Overview,
        Target::Overview,
        false,
    );
    place_row(
        p,
        out,
        &mut y,
        "document-open-recent-symbolic",
        "Recent",
        ui.location == Location::Recent,
        Target::Recent,
        false,
    );
    for (i, place) in ui.places.iter().enumerate() {
        place_row(
            p,
            out,
            &mut y,
            place.icon,
            &place.name,
            current == Some(&place.path),
            Target::Place(i),
            false,
        );
    }

    let heading = |p: &mut Painter<'_, '_>, y: &mut f32, text: &str| {
        if fits(*y, HEADING_H) {
            p.label_v(
                text,
                20.0,
                *y + HEADING_H - 10.0,
                SIDE_W - 40.0,
                t.font_size - 2.0,
                Weight::Bold,
                c.dim,
            );
        }
        *y += HEADING_H;
    };
    heading(p, &mut y, "Devices");
    let compact = ui.drives.len() > CARD_DRIVES;
    for (i, d) in ui.drives.iter().enumerate() {
        let height = if compact { COMPACT_H } else { CARD_H };
        if fits(y, height) {
            drive(ui, p, c, out, d, i, y, compact);
        }
        y += height;
    }
    heading(p, &mut y, "Other devices");
    place_row(
        p,
        out,
        &mut y,
        "list-add-symbolic",
        "Connect…",
        false,
        Target::Connect,
        true,
    );

    // Trash, pinned at the bottom.
    let rect = Rect::new(10.0, h - TRASH_H + 6.0, SIDE_W - 20.0, PLACE_H);
    let trash = model::trash_dir();
    let on = current == Some(&trash);
    if on {
        p.canvas
            .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, c.on);
    } else if ui.hover == Some(Target::Trash) {
        p.canvas
            .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, t.hover);
    }
    p.icon(
        "user-trash-symbolic",
        rect.x + 18.0,
        rect.y + 16.0,
        16.0,
        c.ink,
    );
    p.label_v(
        "Trash",
        rect.x + 37.0,
        rect.y + 16.0,
        rect.w - 90.0,
        size,
        if on { Weight::Bold } else { Weight::Normal },
        c.ink,
    );
    if let Some(bytes) = ui.trash {
        let text = model::human_size(bytes);
        let tw = p.measure(&text, size - 2.0, Weight::Normal);
        p.label_v(
            &text,
            rect.right() - 10.0 - tw,
            rect.y + 16.0,
            tw + 2.0,
            size - 2.0,
            Weight::Normal,
            c.dim,
        );
    }
    out.hits.push((rect, Target::Trash));
}

#[allow(clippy::too_many_arguments)]
fn drive(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    d: &Drive,
    index: usize,
    y: f32,
    compact: bool,
) {
    let t = p.theme;
    let used = if d.total > 0 {
        (d.total.saturating_sub(d.free)) as f32 / d.total as f32
    } else {
        0.0
    };
    let bar = if used > WARN_AT { WARN } else { t.accent };
    let icon = if d.removable {
        "drive-removable-media-symbolic"
    } else {
        "drive-harddisk-symbolic"
    };
    let on = matches!(&ui.location, Location::Dir(dir) if *dir == d.mount);
    let small = t.font_size - 2.0;
    let size = t.font_size;
    let target = Target::Drive(index);
    let hovered = ui.hover == Some(target) || ui.hover == Some(Target::Eject(index));
    let height = if compact { COMPACT_H } else { CARD_H };
    let rect = Rect::new(
        10.0,
        y,
        SIDE_W - 20.0,
        height - if compact { 0.0 } else { 4.0 },
    );
    let right = rect.right() - 10.0;
    let mut text_right = right;
    if d.removable {
        let ey = if compact { y + 12.0 } else { y + 19.0 };
        let eject = Rect::new(right - 20.0, ey - 10.0, 22.0, 20.0);
        let lit = ui.hover == Some(Target::Eject(index));
        if lit {
            p.canvas
                .rounded_rect(eject.x, eject.y, eject.w, eject.h, 6.0, c.on);
        }
        p.icon(
            "media-eject-symbolic",
            right - 9.0,
            ey,
            14.0,
            if lit { c.ink } else { c.dim },
        );
        text_right = eject.x - 2.0;
        out.hits.push((rect, target));
        out.hits.push((eject, Target::Eject(index)));
    } else {
        out.hits.push((rect, target));
    }
    if compact {
        if on {
            p.canvas
                .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, c.on);
        } else if hovered {
            p.canvas
                .rounded_rect(rect.x, rect.y, rect.w, rect.h, 8.0, t.hover);
        }
        let free = model::human_size(d.free);
        let fw = p.measure(&free, small, Weight::Normal);
        p.label_v(
            &free,
            text_right - fw,
            y + 12.0,
            fw + 2.0,
            small,
            Weight::Normal,
            c.dim,
        );
        p.label_v(
            &d.name,
            rect.x + 10.0,
            y + 12.0,
            text_right - fw - rect.x - 20.0,
            size - 1.0,
            Weight::Normal,
            c.ink,
        );
        p.canvas.slider(
            rect.x + 10.0,
            y + 25.0,
            rect.w - 20.0,
            3.0,
            used,
            c.track,
            bar,
        );
        return;
    }
    // The flat card.
    let card = if on {
        t.card_background.mix(c.on, 0.8)
    } else if hovered {
        t.card_background.mix(t.hover, 0.5)
    } else {
        t.card_background
    };
    p.canvas
        .rounded_rect(rect.x, rect.y, rect.w, rect.h, 12.0, card);
    p.icon(icon, rect.x + 19.0, y + 19.0, 16.0, c.ink);
    let pct = format!("{}%", (used * 100.0).round() as u32);
    let pw = p.measure(&pct, small, Weight::Normal);
    p.label_v(
        &pct,
        text_right - pw,
        y + 19.0,
        pw + 2.0,
        small,
        Weight::Normal,
        c.dim,
    );
    p.label_v(
        &d.name,
        rect.x + 36.0,
        y + 19.0,
        text_right - pw - rect.x - 46.0,
        size,
        Weight::Bold,
        c.ink,
    );
    p.canvas.slider(
        rect.x + 11.0,
        y + 34.0,
        rect.w - 22.0,
        6.0,
        used,
        c.track,
        bar,
    );
    let line = format!(
        "{} free of {}",
        model::human_size(d.free),
        model::human_size(d.total)
    );
    p.label_v(
        &line,
        rect.x + 11.0,
        y + 52.0,
        rect.w - 22.0,
        small,
        Weight::Normal,
        c.dim,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn drives() -> Vec<Drive> {
        let d = |name: &str, mount: &str, total: u64, free: u64, removable| Drive {
            name: name.into(),
            mount: mount.into(),
            device: String::new(),
            fs_type: "ext4".into(),
            total,
            free,
            removable,
        };
        vec![
            d("System", "/", 1_000_000_000_000, 412_000_000_000, false),
            d("Games", "/games", 2_000_000_000_000, 280_000_000_000, false),
            d(
                "KINGSTON",
                "/run/media/k",
                32_000_000_000,
                24_600_000_000,
                true,
            ),
        ]
    }

    fn synthetic(n: usize) -> Vec<model::Entry> {
        (0..n)
            .map(|i| {
                let is_dir = i % 7 == 0;
                let name = format!("entry-{i:05}-with-a-longish-name.txt");
                model::Entry {
                    path: PathBuf::from("/x").join(&name),
                    hidden: false,
                    is_symlink: false,
                    size: (i as u64) * 1234,
                    items: is_dir.then_some((i % 90) as u32),
                    modified: Some(SystemTime::now() - Duration::from_secs(i as u64 * 977)),
                    kind: model::kind_of(&name, is_dir),
                    name,
                    is_dir,
                }
            })
            .collect()
    }

    fn ui(path: &Path) -> Ui {
        let home = PathBuf::from("/home/me");
        let mut folder = Folder::new();
        folder.loading = false;
        let place = |name: &str, p: &str, icon| Place {
            name: name.into(),
            path: format!("/home/me/{p}").into(),
            icon,
        };
        Ui {
            config: Config::default(),
            grid_size: 48.0,
            list_size: 18.0,
            location: Location::Dir(path.to_path_buf()),
            folder,
            mode: ViewMode::List,
            info_panel: false,
            drives: drives(),
            trash: Some(1_400_000_000),
            places: vec![
                Place {
                    name: "Home".into(),
                    path: home.clone(),
                    icon: "user-home-symbolic",
                },
                place("Documents", "Documents", "folder-documents-symbolic"),
                place("Downloads", "Downloads", "folder-download-symbolic"),
                place("Pictures", "Pictures", "folder-pictures-symbolic"),
            ],
            home,
            can_back: true,
            can_forward: false,
            hover: None,
            hover_row: None,
            scroll: 0.0,
            side_scroll: 0.0,
            error: None,
            rounded: true,
            frosted: false,
            keyboard_focus: false,
            pages: Pages::default(),
            search: Search::default(),
            x: ops::Extra::default(),
        }
    }

    fn render(ui: &Ui, res: &mut Res, w: u32, h: u32, scale: f32) -> Vec<u8> {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
        paint(ui, res, &mut canvas);
        pixels
    }

    /// Paint time while scrolling 10,000 entries, at 2560x1440 and 1.25x:
    /// `cargo test --release -p cnde-files scroll_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn scroll_bench() {
        let (w, h, scale) = (2560u32, 1440u32, 1.25);
        let theme = Theme::for_appearance(&Config::default().appearance);
        let mut res = Res::new(theme, Icons::new());
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for (frosted, grid, size) in [
            (false, false, 18.0),
            (true, false, 18.0),
            (false, true, 48.0),
            (false, true, 192.0),
            (false, false, 48.0),
        ] {
            let mut ui = ui(Path::new("/x"));
            ui.frosted = frosted;
            if grid {
                ui.mode = ViewMode::Grid;
                ui.grid_size = size;
            } else {
                ui.list_size = size;
            }
            ui.folder.add(synthetic(10_000));
            let frames = 120;
            let mut total = Duration::ZERO;
            let mut worst = Duration::ZERO;
            for i in 0..frames {
                ui.scroll = (i * 17) as f32;
                ui.hover_row = Some(i % 30 + (ui.scroll / zoom::row_h(ui.list_size)) as usize);
                let started = std::time::Instant::now();
                let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
                paint(&ui, &mut res, &mut canvas);
                let took = started.elapsed();
                total += took;
                worst = worst.max(took);
            }
            println!(
                "10k entries{}{} {size}px: {:.2} ms a frame, worst {:.2} ms",
                if frosted { " frosted" } else { "" },
                if grid { " grid" } else { " list" },
                total.as_secs_f64() * 1000.0 / frames as f64,
                worst.as_secs_f64() * 1000.0
            );
        }
    }

    /// The same with a thumbnail on every entry (list and grid).
    #[test]
    #[ignore]
    fn scroll_bench_thumbs() {
        let (w, h, scale) = (2560u32, 1440u32, 1.25);
        let theme = Theme::for_appearance(&Config::default().appearance);
        let mut res = Res::new(theme, Icons::new());
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let pm = std::sync::Arc::new(cnde_ui::tiny_skia::Pixmap::new(128, 96).unwrap());
        for grid in [false, true] {
            let mut ui = ui(Path::new("/x"));
            if grid {
                ui.mode = ViewMode::Grid;
            }
            let mut entries = synthetic(10_000);
            for e in &mut entries {
                e.is_dir = false;
                e.path.set_extension("png");
                crate::thumbs::put(
                    &e.path,
                    e.modified.unwrap(),
                    crate::thumbs::NORMAL,
                    pm.clone(),
                );
            }
            ui.folder.add(entries);
            let frames = 120;
            let mut total = Duration::ZERO;
            for i in 0..frames {
                ui.scroll = (i * 17) as f32;
                let started = std::time::Instant::now();
                let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
                paint(&ui, &mut res, &mut canvas);
                total += started.elapsed();
            }
            println!(
                "10k thumbnails{}: {:.2} ms a frame",
                if grid { " grid" } else { " list" },
                total.as_secs_f64() * 1000.0 / frames as f64
            );
        }
    }

    /// `CNDE_PREVIEW=out.png CNDE_PREVIEW_THEME=quiet|quiet-light|light|adwaita`
    /// renders the window on the repo's root folder to a PNG.
    #[test]
    #[ignore]
    fn preview() {
        let Some(path) = std::env::var_os("CNDE_PREVIEW") else {
            return;
        };
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
            _ => (cnde_config::Palette::Quiet, cnde_config::ColorScheme::Dark),
        };
        // CNDE_PREVIEW_THUMBS=<dir of images> shows that folder, with thumbnails.
        let thumbs_dir = std::env::var_os("CNDE_PREVIEW_THUMBS").map(PathBuf::from);
        let root = thumbs_dir.clone().unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .canonicalize()
                .unwrap()
        });
        let mut ui = ui(&root);
        ui.config.appearance.palette = palette;
        ui.config.appearance.color_scheme = scheme;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut entries = Vec::new();
        model::list_dir(&root, &cancel, |b| entries.extend(b)).unwrap();
        if thumbs_dir.is_some() {
            for e in entries.iter().filter(|e| !e.is_dir) {
                crate::thumbs::prime(&e.path, crate::thumbs::NORMAL);
                crate::thumbs::prime(&e.path, crate::thumbs::LARGE);
            }
        }
        ui.folder.add(entries);
        ui.folder
            .select_only(if thumbs_dir.is_some() { 1 } else { 5 });
        ui.hover_row = Some(2);
        ui.folder.cursor = Some(5);
        ui.keyboard_focus = std::env::var_os("CNDE_PREVIEW_FOCUS").is_some();
        if std::env::var_os("CNDE_PREVIEW_MANY").is_some() {
            let mut d = drives();
            for i in 0..4 {
                d.push(Drive {
                    name: format!("Disk {i}"),
                    ..d[0].clone()
                });
            }
            ui.drives = d;
        }
        // CNDE_PREVIEW_PAGE=overview|recent|search shows that page instead.
        let page = std::env::var("CNDE_PREVIEW_PAGE").unwrap_or_default();
        if page == "overview" || page == "recent" {
            ui.location = if page == "recent" {
                Location::Recent
            } else {
                Location::Overview
            };
            ui.hover = Some(Target::RecentFile(1));
            ui.pages.recent = Some(
                [
                    ("notes.md", "~/Documents"),
                    ("cnde-files design.html", "~/Downloads"),
                    ("photo-0412.jpg", "~/Pictures/Trip"),
                    ("compositor", "~/Sync/programming/cnde"),
                    ("report.pdf", "~/Documents/Work"),
                    ("song.flac", "~/Music"),
                ]
                .iter()
                .cycle()
                .take(if page == "recent" { 30 } else { 8 })
                .enumerate()
                .map(|(i, (name, folder))| pages::RecentRow {
                    path: PathBuf::from("/home/me").join(name),
                    name: name.to_string(),
                    folder: folder.to_string(),
                    when: SystemTime::now() - Duration::from_secs(900 * (i as u64 + 1).pow(2)),
                    modified: None,
                    is_dir: *name == "compositor",
                })
                .collect(),
            );
        } else if page == "search" {
            ui.search.query = "co".into();
            ui.search.cursor = 2;
            ui.search.focused = true;
            ui.folder.set_query("co");
            ui.keyboard_focus = true;
        }
        ui.pages.width = 1100.0;
        if std::env::var_os("CNDE_PREVIEW_GRID").is_some() {
            ui.mode = ViewMode::Grid;
        }
        if std::env::var_os("CNDE_PREVIEW_INFO").is_some() {
            ui.info_panel = true;
            // `CNDE_PREVIEW_INFO=folder` shows the open folder, else the selection.
            if std::env::var("CNDE_PREVIEW_INFO").as_deref() == Ok("folder") {
                ui.folder.clear_selection();
            }
            let paths: Vec<PathBuf> = match info::subject(&ui) {
                Some(info::Subject::One(e)) => vec![e.path.clone()],
                _ => vec![root.clone()],
            };
            ui.x.info.scan = Some(info::scan(&paths, &cancel));
        }
        if std::env::var_os("CNDE_PREVIEW_RENAME").is_some() {
            ui.x.rename = Some(ops::Rename {
                name: ui.folder.entry(5).unwrap().name.clone(),
                text: ui.folder.entry(5).unwrap().name.clone(),
                cursor: 3,
                fresh: false,
            });
        }
        // CNDE_PREVIEW_PROGRESS=1 shows a copy under way.
        if std::env::var_os("CNDE_PREVIEW_PROGRESS").is_some() {
            ui.x.xfer.strip = Some(crate::transfer::Strip {
                name: "Holiday photos 2025".into(),
                done: 1_200_000_000,
                total: 4_000_000_000,
                since: std::time::Instant::now() - std::time::Duration::from_secs(2),
                cancel: Default::default(),
            });
            if std::env::var("CNDE_PREVIEW_PROGRESS").as_deref() == Ok("toast") {
                ui.error = Some("Can't copy “x”: Permission denied".into());
            }
        }
        // CNDE_PREVIEW_ZOOM=<px> sets the icon size of the shown view.
        if let Some(px) = std::env::var("CNDE_PREVIEW_ZOOM")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
        {
            match ui.mode {
                ViewMode::Grid => ui.grid_size = zoom::clamp(ViewMode::Grid, px),
                ViewMode::List => ui.list_size = zoom::clamp(ViewMode::List, px),
            }
        }
        // CNDE_PREVIEW_MAINMENU=1 opens the ⋮ menu.
        if std::env::var_os("CNDE_PREVIEW_MAINMENU").is_some() {
            ui.x.main_menu = Some(crate::mainmenu::MainMenu::default());
        }
        // CNDE_PREVIEW_MENU=file|open-with|empty|new|recent|recent-open-with
        if let Ok(kind) = std::env::var("CNDE_PREVIEW_MENU") {
            let mut menu = ops::Menu::at(420.0, 250.0);
            let fake = |id: &str, name: &str| crate::apps::App {
                id: id.into(),
                name: name.into(),
                exec: name.into(),
                icon: None,
                mimes: vec![],
                categories: vec![],
                terminal: false,
                no_display: false,
                path: PathBuf::new(),
            };
            crate::apps::install(crate::apps::Index {
                apps: vec![
                    fake("a", "Text Editor"),
                    fake("b", "Code"),
                    fake("c", "Terminal"),
                    fake("d", "Web Browser"),
                ],
                env: crate::apps::Env::from_env(),
            });
            if kind.starts_with("recent") {
                menu.recent = Some(1);
                menu.x = 520.0;
                menu.y = 400.0;
            }
            if kind == "empty" || kind == "new" {
                menu.empty = true;
            }
            if kind.ends_with("open-with") {
                menu.sub = Some(ops::Sub::OpenWith);
                menu.apps = vec![0, 1, 2];
                menu.desc = ".md files".into();
                menu.always = true;
            }
            if kind == "new" {
                menu.sub = Some(ops::Sub::New);
                menu.templates = vec![
                    ("Document.odt".into(), PathBuf::new()),
                    ("Notes.md".into(), PathBuf::new()),
                ];
            }
            ui.x.menu = Some(menu);
        }
        let theme = Theme::for_appearance(&ui.config.appearance);
        let mut res = Res::new(theme, Icons::new());
        let (w, h) = (1100u32, 680u32);
        let pixels = render(&ui, &mut res, w, h, 1.0);
        image::RgbaImage::from_raw(w, h, pixels)
            .unwrap()
            .save(path)
            .unwrap();
    }

    #[test]
    fn rows_follow_the_icon_size() {
        let mut ui = ui(Path::new("/x"));
        ui.folder.add(synthetic(100));
        let list_w = list_w(&ui, 1000.0);
        for size in [16.0, 18.0, 32.0, 48.0] {
            ui.list_size = size;
            let (cols, row_h) = cells(&ui, list_w);
            assert_eq!((cols, row_h), (1, zoom::row_h(size)));
            assert_eq!(content_height(&ui, list_w), 100.0 * row_h + 8.0);
            // The row under a point is found with the taller rows.
            let list = list_rect(&ui, 1000.0, 600.0);
            let y = list.y + 4.0 + row_h * 3.5;
            assert_eq!(
                pos_at(&ui, list, ((list.x + 20.0) as f64, y as f64)),
                Some(3)
            );
        }
        ui.mode = ViewMode::Grid;
        for size in [48.0, 96.0, 192.0] {
            ui.grid_size = size;
            let (cols, row_h) = cells(&ui, list_w);
            assert_eq!(row_h, grid::tile_h(size));
            let rows = 100usize.div_ceil(cols) as f32;
            assert_eq!(content_height(&ui, list_w), rows * row_h + 8.0);
        }
    }

    #[test]
    fn paints_with_targets() {
        let mut ui = ui(Path::new("/home/me/proj"));
        ui.folder.add(synthetic(50));
        let theme = Theme::for_appearance(&ui.config.appearance);
        let mut res = Res::new(theme, Icons::new());
        let mut pixels = vec![0u8; 1000 * 600 * 4];
        let mut canvas = Canvas::new(&mut pixels, 1000, 600, 1.0).unwrap();
        let painted = paint(&ui, &mut res, &mut canvas);
        assert!(painted.hits.iter().any(|(_, t)| *t == Target::Back));
        assert!(painted.hits.iter().any(|(_, t)| *t == Target::Eject(2)));
        assert!(
            painted
                .hits
                .iter()
                .any(|(_, t)| *t == Target::Sort(SortKey::Size))
        );
        assert_eq!(painted.crumbs.last().unwrap(), Path::new("/home/me"));
        assert_eq!(
            drive_of(&ui.drives, Path::new("/games/x")).unwrap().name,
            "Games"
        );
    }
}
