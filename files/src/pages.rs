//! The two pages that aren't a folder: Overview (drive rings over a short
//! recent list) and Recent (the full list). Painting and hit targets only;
//! the recent files arrive from a background thread.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use cnde_ui::{Color, Painter, Rect, Weight};

use crate::{
    folder::Location,
    grid,
    model::{self, Drive},
    thumbs,
    view::{
        Colors, Painted, SIDE_W, Target, TextCache, Ui, ViewMode, WARN, WARN_AT, icon_for_name,
    },
};

/// How many recents the Recent page loads.
pub const RECENT_LIMIT: usize = 200;
/// How many show on the Overview.
const OVERVIEW_RECENT: usize = 12;

const PAD: f32 = 22.0;
const TOP: f32 = 0.0;
/// The ring grid starts above the list's edge: the cells have air at their top.
const RINGS_TOP: f32 = -8.0;
const CELL_MIN: f32 = 208.0;
const CELL_H: f32 = 188.0;
const RING: f32 = 84.0;
const SECTION_H: f32 = 40.0;
const ROW_H: f32 = crate::view::ROW_H;
const LIST_PAD: f32 = 12.0;
const LIST_PAD_RIGHT: f32 = 16.0;
const TIME_W: f32 = 112.0;

/// A recently used file, ready to paint.
pub struct RecentRow {
    pub path: PathBuf,
    pub name: String,
    /// The folder it is in, with the home folder as `~`.
    pub folder: String,
    pub when: SystemTime,
    /// The file's own modified time, for its thumbnail.
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
}

/// State of the Overview and Recent pages.
#[derive(Default)]
pub struct Pages {
    /// `None` until the first load finishes.
    pub recent: Option<Vec<RecentRow>>,
    /// Window width in logical px, for the page's layout.
    pub width: f32,
    /// Changes whenever `recent` does, for the tiles' cached text.
    pub version: u64,
}

/// Load the recents (call on a thread); paths are shown relative to `home`.
pub fn load_recent(home: &Path) -> Vec<RecentRow> {
    model::recent(RECENT_LIMIT)
        .into_iter()
        .map(|r| {
            let name = r
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let parent = r.path.parent().unwrap_or(Path::new("/"));
            let meta = std::fs::metadata(&r.path).ok();
            RecentRow {
                is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                modified: meta.and_then(|m| m.modified().ok()),
                folder: tilde(parent, home),
                name,
                when: r.when,
                path: r.path,
            }
        })
        .collect()
}

/// `dir` with `home` shortened to `~`.
pub fn tilde(dir: &Path, home: &Path) -> String {
    match dir.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// Columns of drive rings that fit in a window `w` wide.
fn ring_columns(w: f32) -> usize {
    (((w - SIDE_W - PAD * 2.0) / CELL_MIN).floor() as usize).max(1)
}

/// The recents as tiles (the grid view) rather than rows.
fn tiled(ui: &Ui) -> bool {
    ui.mode == ViewMode::Grid
}

/// How the recent tiles lie on the page.
fn tile_geo(ui: &Ui) -> grid::Geo {
    grid::geo((ui.pages.width - SIDE_W).max(0.0), ui.grid_size)
}

fn recent_count(ui: &Ui) -> usize {
    let n = ui.pages.recent.as_ref().map_or(0, Vec::len);
    match ui.location {
        Location::Overview => n.min(OVERVIEW_RECENT),
        _ => n,
    }
}

/// Height of the page's content in logical px.
pub fn content_height(ui: &Ui) -> f32 {
    let rows = if tiled(ui) {
        let g = tile_geo(ui);
        recent_count(ui).div_ceil(g.cols) as f32 * g.tile_h
    } else {
        recent_count(ui) as f32 * ROW_H
    };
    match ui.location {
        Location::Overview => {
            let cols = ring_columns(ui.pages.width);
            let grid_rows = ui.drives.len().div_ceil(cols) as f32;
            RINGS_TOP + grid_rows * CELL_H + SECTION_H + rows + 8.0
        }
        _ => TOP + rows + 8.0,
    }
}

/// "412 GB free across 3 drives".
pub fn overview_subtitle(ui: &Ui) -> String {
    let free: u64 = ui.drives.iter().map(|d| d.free).sum();
    let n = ui.drives.len();
    format!(
        "{} free across {n} {}",
        model::human_size(free),
        if n == 1 { "drive" } else { "drives" }
    )
}

/// "37 files".
pub fn recent_subtitle(ui: &Ui) -> String {
    match &ui.pages.recent {
        None => "Loading…".into(),
        Some(r) => format!(
            "{} {}",
            r.len(),
            if r.len() == 1 { "file" } else { "files" }
        ),
    }
}

/// Paint the page for the current location into the list area.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    cache: &mut TextCache,
    c: &Colors,
    out: &mut Painted,
    w: f32,
    h: f32,
    scale: f32,
) {
    let list = out.list;
    let mut y = list.y + TOP - ui.scroll;
    if ui.location == Location::Overview {
        y = list.y + RINGS_TOP - ui.scroll;
        y = rings(ui, p, c, out, w, h, y);
        p.label_v(
            "Recent",
            SIDE_W + PAD,
            y + SECTION_H - 14.0,
            w - SIDE_W - PAD * 2.0,
            p.theme.font_size - 0.5,
            Weight::Bold,
            c.dim,
        );
        y += SECTION_H;
    }
    if tiled(ui) && ui.pages.recent.as_ref().is_some_and(|r| !r.is_empty()) {
        recent_tiles(ui, p, cache, c, out, h, y, scale);
    } else {
        recent_rows(ui, p, c, out, w, h, y);
    }

    let content = content_height(ui);
    if content > list.h {
        let track_h = list.h - 8.0;
        let thumb_h = (track_h * list.h / content).max(24.0);
        let range = (content - list.h).max(1.0);
        let ty = list.y + 4.0 + (track_h - thumb_h) * (ui.scroll / range).clamp(0.0, 1.0);
        p.canvas
            .rounded_rect(w - 7.0, ty, 3.0, thumb_h, 1.5, c.ink.with_alpha(0.25));
    }
}

/// The ring grid starting at `y`; returns the y below it.
fn rings(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    w: f32,
    h: f32,
    y: f32,
) -> f32 {
    let cols = ring_columns(w);
    let cell_w = (w - SIDE_W - PAD * 2.0) / cols as f32;
    for (i, d) in ui.drives.iter().enumerate() {
        let (col, row) = (i % cols, i / cols);
        let cell = Rect::new(
            SIDE_W + PAD + col as f32 * cell_w,
            y + row as f32 * CELL_H,
            cell_w,
            CELL_H,
        );
        if cell.bottom() < out.list.y || cell.y > h {
            continue;
        }
        ring_cell(ui, p, c, out, d, i, cell);
    }
    y + ui.drives.len().div_ceil(cols) as f32 * CELL_H
}

fn ring_cell(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    d: &Drive,
    index: usize,
    cell: Rect,
) {
    let t = p.theme;
    let used = if d.total > 0 {
        d.total.saturating_sub(d.free) as f32 / d.total as f32
    } else {
        0.0
    };
    let color = if used > WARN_AT { WARN } else { t.accent };
    let cx = cell.x + cell.w / 2.0;
    let inner = Rect::new(cell.x + 4.0, cell.y + 10.0, cell.w - 8.0, CELL_H - 16.0);
    if ui.hover == Some(Target::Drive(index)) {
        p.canvas
            .rounded_rect(inner.x, inner.y, inner.w, inner.h, 14.0, t.hover);
    }
    out.hits.push((inner, Target::Drive(index)));

    let ring_cy = cell.y + 16.0 + RING / 2.0;
    p.canvas
        .ring(cx, ring_cy, RING / 2.0 - 2.0, 3.4, used, c.track, color);
    let pct = format!("{}%", (used * 100.0).round() as u32);
    centered(
        p,
        &pct,
        cx,
        ring_cy,
        cell.w - 16.0,
        17.0,
        Weight::Bold,
        c.ink,
    );

    let size = t.font_size;
    let small = t.font_size - 1.5;
    let max = cell.w - 24.0;
    let mut cy = cell.y + 16.0 + RING + 18.0;
    centered(p, &d.name, cx, cy, max, size, Weight::Bold, c.ink);
    cy += 19.0;
    let free = format!(
        "{} free of {}",
        model::human_size(d.free),
        model::human_size(d.total)
    );
    centered(p, &free, cx, cy, max, small, Weight::Normal, c.dim);
    cy += 17.0;
    let meta = format!("{} · {}", d.fs_type, d.mount.display());
    centered(
        p,
        &meta,
        cx,
        cy,
        max,
        small - 0.5,
        Weight::Normal,
        c.dim.with_alpha(0.7),
    );
    if d.removable {
        cy += 24.0;
        let label_w = p.measure("Eject", small, Weight::Normal);
        let bw = label_w + 18.0 + 20.0;
        let btn = Rect::new(cx - bw / 2.0, cy - 12.0, bw, 24.0);
        let lit = ui.hover == Some(Target::Eject(index));
        if lit {
            p.canvas.rounded_rect(btn.x, btn.y, btn.w, btn.h, 8.0, c.on);
        }
        p.icon("media-eject-symbolic", btn.x + 16.0, cy, 13.0, t.accent);
        p.label_v(
            "Eject",
            btn.x + 29.0,
            cy,
            label_w + 2.0,
            small,
            Weight::Normal,
            t.accent,
        );
        out.hits.push((btn, Target::Eject(index)));
    }
}

/// Text centred on `cx` at vertical centre `cy`.
#[allow(clippy::too_many_arguments)]
fn centered(
    p: &mut Painter<'_, '_>,
    text: &str,
    cx: f32,
    cy: f32,
    max: f32,
    size: f32,
    weight: Weight,
    color: Color,
) {
    let tw = p.measure(text, size, weight).min(max);
    p.label_v(text, cx - tw / 2.0, cy, max, size, weight, color);
}

/// The recent rows from `y` on; only those in view are painted or hit.
fn recent_rows(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    w: f32,
    h: f32,
    y: f32,
) {
    let t = p.theme;
    let list = out.list;
    let Some(rows) = &ui.pages.recent else {
        return;
    };
    if rows.is_empty() {
        let msg = "No recent files yet";
        let tw = p.measure(msg, t.font_size, Weight::Normal);
        p.label_v(
            msg,
            list.x + (list.w - tw) / 2.0,
            y + 40.0,
            list.w,
            t.font_size,
            Weight::Normal,
            c.dim,
        );
        return;
    }
    let rx = SIDE_W + LIST_PAD;
    let rw = w - rx - LIST_PAD_RIGHT;
    let time_x = rx + rw - 10.0 - TIME_W;
    let name_x = rx + 10.0;
    let text_w = time_x - name_x - 20.0;
    let name_w = (text_w / 2.0).max(60.0);
    let folder_x = name_x + name_w + 20.0;
    let folder_w = (time_x - 20.0 - folder_x).max(0.0);
    let name_size = t.font_size - 0.5;
    let small = t.font_size - 1.5;
    let now = SystemTime::now();
    for (i, r) in rows.iter().enumerate().take(recent_count(ui)) {
        let ry = y + i as f32 * ROW_H;
        if ry + ROW_H < list.y || ry > h {
            continue;
        }
        let row = Rect::new(rx, ry, rw, ROW_H);
        let over_file = ui.hover == Some(Target::RecentFile(i));
        let over_folder = ui.hover == Some(Target::RecentFolder(i));
        if over_file || over_folder {
            p.canvas
                .rounded_rect(row.x, row.y, row.w, row.h, 8.0, t.hover);
        }
        out.hits.push((row, Target::RecentFile(i)));
        let cy = ry + ROW_H / 2.0;
        let icon = icon_for_name(&r.name, r.is_dir);
        let icon_color = if r.is_dir { c.fold } else { c.dim };
        p.icon(icon, name_x + 10.0, cy, 18.0, icon_color);
        p.label_v(
            &r.name,
            name_x + 28.0,
            cy,
            name_w - 28.0,
            name_size,
            Weight::Normal,
            c.ink,
        );
        if folder_w > 0.0 {
            let color = if over_folder { c.ink } else { c.dim };
            let fw = p.label_v(
                &r.folder,
                folder_x,
                cy,
                folder_w,
                small,
                Weight::Normal,
                color,
            );
            out.hits.push((
                Rect::new(folder_x - 2.0, ry, fw + 4.0, ROW_H),
                Target::RecentFolder(i),
            ));
        }
        let when = model::human_time(r.when, now);
        let tw = p.measure(&when, small, Weight::Normal).min(TIME_W);
        p.label_v(
            &when,
            time_x + TIME_W - tw,
            cy,
            TIME_W,
            small,
            Weight::Normal,
            c.dim,
        );
    }
}

/// The recents as tiles like the folder grid's, from `y` on; only the
/// rows of tiles in view are painted or hit.
#[allow(clippy::too_many_arguments)]
fn recent_tiles(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    cache: &mut TextCache,
    c: &Colors,
    out: &mut Painted,
    h: f32,
    y: f32,
    scale: f32,
) {
    let t = p.theme;
    let list = out.list;
    let Some(rows) = &ui.pages.recent else {
        return;
    };
    let g = tile_geo(ui);
    let name_size = t.font_size - 1.0;
    let small = t.font_size - 2.0;
    // Apart from the folder's text, and the list's.
    cache.sync(
        ui.pages.version | 1 << 62,
        scale,
        -g.tile_w - 0.5,
        name_size,
    );
    let first_row = ((list.y - y) / g.tile_h).floor().max(0.0) as usize;
    let last_row = ((h - y) / g.tile_h).ceil().max(0.0) as usize;
    let first = first_row * g.cols;
    let last = (last_row * g.cols).min(recent_count(ui));
    for (i, r) in rows.iter().enumerate().take(last).skip(first) {
        let (row, col) = (i / g.cols, i % g.cols);
        let x = list.x + grid::PAD + col as f32 * g.tile_w;
        let ty = y + row as f32 * g.tile_h;
        let (bx, bw, bh) = (x + 2.0, g.tile_w - 4.0, g.tile_h - 4.0);
        if ui.hover == Some(Target::RecentFile(i)) || ui.hover == Some(Target::RecentFolder(i)) {
            p.canvas.rounded_rect(bx, ty, bw, bh, 12.0, t.hover);
        }
        out.hits
            .push((Rect::new(bx, ty, bw, bh), Target::RecentFile(i)));
        let cx = x + g.tile_w / 2.0;
        let thumb = if r.is_dir {
            None
        } else {
            thumbs::get(&r.path, r.modified, grid::thumb_size(g.icon, scale), false)
        };
        if let Some(pm) = thumb {
            let (tw, th) = grid::thumb_box(g.icon);
            let rect = (
                cx - tw / 2.0,
                ty + grid::TOP + g.icon / 2.0 - th / 2.0,
                tw,
                th,
            );
            thumbs::draw(p.canvas, &pm, rect, 6.0 + g.icon / 24.0);
        } else {
            let color = if r.is_dir { c.fold } else { c.dim };
            p.icon(
                icon_for_name(&r.name, r.is_dir),
                cx,
                ty + grid::TOP + g.icon / 2.0,
                g.icon,
                color,
            );
        }
        let max_w = g.tile_w - 12.0;
        let mut ly = ty + grid::TOP + g.icon + 6.0;
        if !cache.map.contains_key(&(i, 0)) {
            let (a, b) = grid::two_lines(p.fonts, &r.name, name_size, scale, max_w);
            cache.map.insert((i, 0), a);
            cache.map.insert((i, 1), b);
        }
        for key in 0..2u8 {
            if let Some(layout) = cache.map.get_mut(&(i, key)) {
                if key == 1 && layout.width < 0.5 {
                    continue;
                }
                let (lx, lh) = (cx - layout.width / 2.0, layout.height);
                p.canvas.text(p.fonts, layout, lx, ly, c.ink);
                ly += lh.max(name_size * 1.25);
            }
        }
        // The folder it is in; clicking it shows the file there.
        let layout = cache.map.entry((i, 2)).or_insert_with(|| {
            crate::view::fit(p.fonts, &r.folder, small, Weight::Normal, scale, max_w)
        });
        let (lw, lx, sy) = (layout.width, cx - layout.width / 2.0, ly + 2.0);
        let over = ui.hover == Some(Target::RecentFolder(i));
        p.canvas
            .text(p.fonts, layout, lx, sy, if over { c.ink } else { c.dim });
        out.hits.push((
            Rect::new(lx - 2.0, sy - 2.0, lw + 4.0, small * 1.4 + 4.0),
            Target::RecentFolder(i),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_paths() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me"), home), "~");
        assert_eq!(tilde(Path::new("/home/me/a/b"), home), "~/a/b");
        assert_eq!(tilde(Path::new("/etc"), home), "/etc");
        assert_eq!(tilde(Path::new("/home/mex"), home), "/home/mex");
    }

    #[test]
    fn columns_wrap() {
        assert_eq!(ring_columns(500.0), 1);
        assert_eq!(ring_columns(1100.0), 3);
    }
}
