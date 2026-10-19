//! The folder as a grid of tiles: a big icon, the name on up to two lines
//! and a muted line of size or item count. Only the visible rows of tiles
//! are painted, and their text is shaped once (see [`TextCache`]).

use cnde_ui::{Fonts, Painter, Rect, TextLayout, Weight};

use crate::{
    model, thumbs,
    view::{self, Colors, Painted, TextCache, Ui},
};

const PAD: f32 = 16.0;
const TOP: f32 = 10.0;
/// A tile is the icon plus room for the name and the size line.
pub fn tile_w(icon: f32) -> f32 {
    icon + 64.0
}

pub fn tile_h(icon: f32) -> f32 {
    icon + 74.0
}

/// The box a thumbnail fits in: a bit bigger than the icon.
fn thumb_box(icon: f32) -> (f32, f32) {
    (icon + 20.0, icon + 8.0)
}

/// Which cache size a thumbnail in a tile needs, at `scale` px per logical px.
pub fn thumb_size(icon: f32, scale: f32) -> u32 {
    if thumb_box(icon).0 * scale > thumbs::NORMAL as f32 {
        thumbs::LARGE
    } else {
        thumbs::NORMAL
    }
}

/// How the tiles lie in a list area `list_w` wide.
#[derive(Clone, Copy, Debug)]
pub struct Geo {
    pub cols: usize,
    pub tile_w: f32,
    pub tile_h: f32,
    /// The icon's size.
    pub icon: f32,
}

pub fn geo(list_w: f32, icon: f32) -> Geo {
    let min_w = tile_w(icon);
    let avail = (list_w - PAD * 2.0).max(min_w);
    let cols = ((avail / min_w).floor() as usize).max(1);
    Geo {
        cols,
        tile_w: avail / cols as f32,
        tile_h: tile_h(icon),
        icon,
    }
}

/// The tile at a point of the list area, if any.
pub fn pos_at(
    list: Rect,
    geo: Geo,
    scroll: f32,
    count: usize,
    (x, y): (f64, f64),
) -> Option<usize> {
    let col = ((x as f32 - list.x - PAD) / geo.tile_w).floor();
    let row = ((y as f32 - list.y - 4.0 + scroll) / geo.tile_h).floor();
    if col < 0.0 || row < 0.0 || col as usize >= geo.cols {
        return None;
    }
    let pos = row as usize * geo.cols + col as usize;
    (pos < count).then_some(pos)
}

/// Up to two lines of `text`, the second ellipsized; the second is empty
/// when the first is enough.
fn two_lines(
    fonts: &mut Fonts,
    text: &str,
    size: f32,
    scale: f32,
    max_w: f32,
) -> (TextLayout, TextLayout) {
    let full = fonts.layout(text, size, Weight::Normal, scale);
    if full.width <= max_w {
        return (full, fonts.layout("", size, Weight::Normal, scale));
    }
    let chars: Vec<char> = text.chars().collect();
    let width = |fonts: &mut Fonts, s: &str| fonts.layout(s, size, Weight::Normal, scale).width;
    // The most characters the first line holds.
    let (mut lo, mut hi) = (1, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let head: String = chars[..mid].iter().collect();
        if width(fonts, &head) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // Prefer breaking after a separator when one is near the end.
    let mut cut = lo;
    if let Some(i) = chars[..lo]
        .iter()
        .rposition(|c| matches!(c, ' ' | '-' | '_' | '.'))
        && i + 1 > lo * 3 / 5
    {
        cut = i + 1;
    }
    let first: String = chars[..cut]
        .iter()
        .collect::<String>()
        .trim_end()
        .to_string();
    let rest: String = chars[cut..].iter().collect();
    let first = fonts.layout(&first, size, Weight::Normal, scale);
    let second = view::fit(fonts, &rest, size, Weight::Normal, scale, max_w);
    (first, second)
}

#[allow(clippy::too_many_arguments)]
pub fn paint(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    cache: &mut TextCache,
    c: &Colors,
    out: &Painted,
    h: f32,
    scale: f32,
) {
    let t = p.theme;
    let list = out.list;
    let folder = &ui.folder;
    let g = geo(list.w, ui.grid_size);
    let name_size = t.font_size - 1.0;
    let small = t.font_size - 2.0;
    // A negative width keeps these apart from the list's cached text.
    cache.sync(folder.version, scale, -g.tile_w, name_size);

    if view::status(ui, p, c, list) {
        return;
    }
    let first_row = ((ui.scroll - 4.0) / g.tile_h).floor().max(0.0) as usize;
    let last_row = ((ui.scroll + list.h) / g.tile_h).ceil() as usize + 1;
    let first = first_row * g.cols;
    let last = (last_row * g.cols).min(folder.visible.len());
    for pos in first..last {
        let Some(entry) = folder.entry(pos) else {
            continue;
        };
        let index = folder.visible[pos];
        let (row, col) = (pos / g.cols, pos % g.cols);
        let x = list.x + PAD + col as f32 * g.tile_w;
        let y = list.y + 4.0 + row as f32 * g.tile_h - ui.scroll;
        if y + g.tile_h < list.y - g.tile_h || y > h {
            continue;
        }
        let (tx, tw, th) = (x + 2.0, g.tile_w - 4.0, g.tile_h - 4.0);
        if folder.is_selected(pos) {
            p.canvas.rounded_rect(tx, y, tw, th, 12.0, c.sel);
        } else if ui.hover_row == Some(pos) {
            p.canvas.rounded_rect(tx, y, tw, th, 12.0, t.hover);
        }
        if ui.keyboard_focus && folder.cursor == Some(pos) {
            p.canvas
                .stroke_rounded_rect(tx, y, tw, th, 12.0, 2.0, t.focus_ring);
        }
        if ui.x.dnd.over == Some(crate::dnd::Over::Row(pos)) {
            p.canvas.rounded_rect(tx, y, tw, th, 12.0, c.sel);
            p.canvas
                .stroke_rounded_rect(tx, y, tw, th, 12.0, 2.0, t.accent);
        }
        let cx = x + g.tile_w / 2.0;
        let color = if entry.is_dir { c.fold } else { c.dim };
        let thumb = if entry.is_dir {
            None
        } else {
            thumbs::get(
                &entry.path,
                entry.modified,
                thumb_size(g.icon, scale),
                false,
            )
        };
        if let Some(pm) = thumb {
            let (bw, bh) = thumb_box(g.icon);
            let bx = (cx - bw / 2.0, y + TOP + g.icon / 2.0 - bh / 2.0, bw, bh);
            thumbs::draw(p.canvas, &pm, bx, 6.0 + g.icon / 24.0);
        } else {
            p.icon(
                view::icon_for(entry),
                cx,
                y + TOP + g.icon / 2.0,
                g.icon,
                color,
            );
        }

        let name_y = y + TOP + g.icon + 6.0;
        let max_w = g.tile_w - 12.0;
        let mut ly = name_y;
        if ui.x.rename.as_ref().is_some_and(|r| r.name == entry.name) {
            crate::ops::paint_rename(
                ui,
                p,
                c,
                Rect::new(x + 2.0, name_y - 1.0, g.tile_w - 4.0, 28.0),
            );
            ly = name_y + 2.0 * name_size * 1.25;
        } else {
            if !cache.map.contains_key(&(index, 0)) {
                let (a, b) = two_lines(p.fonts, &entry.name, name_size, scale, max_w);
                cache.map.insert((index, 0), a);
                cache.map.insert((index, 1), b);
            }
            // The size line follows the last line of the name.
            for col_key in 0..2u8 {
                if let Some(layout) = cache.map.get_mut(&(index, col_key)) {
                    if col_key == 1 && layout.width < 0.5 {
                        continue;
                    }
                    let lx = cx - layout.width / 2.0;
                    let lh = layout.height;
                    p.canvas.text(p.fonts, layout, lx, ly, c.ink);
                    ly += lh.max(name_size * 1.25);
                }
            }
        }
        let sub_y = ly + 2.0;
        let layout = cache.map.entry((index, 2)).or_insert_with(|| {
            let text = match (entry.is_dir, entry.items) {
                (true, Some(n)) => format!("{n} {}", if n == 1 { "item" } else { "items" }),
                (true, None) => String::new(),
                _ => model::human_size(entry.size),
            };
            view::fit(p.fonts, &text, small, Weight::Normal, scale, max_w)
        });
        let lx = cx - layout.width / 2.0;
        p.canvas.text(p.fonts, layout, lx, sub_y, c.dim);
    }

    // A thin scroll indicator when there's more than fits.
    let content = view::content_height(ui, list.w);
    if content > list.h {
        let track_h = list.h - 8.0;
        let thumb_h = (track_h * list.h / content).max(24.0);
        let range = (content - list.h).max(1.0);
        let ty = list.y + 4.0 + (track_h - thumb_h) * (ui.scroll / range).clamp(0.0, 1.0);
        p.canvas.rounded_rect(
            list.right() - 7.0,
            ty,
            3.0,
            thumb_h,
            1.5,
            c.ink.with_alpha(0.25),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_the_width() {
        assert_eq!(geo(100.0, 48.0).cols, 1);
        let g = geo(772.0, 48.0);
        assert_eq!(g.cols, 6);
        assert!((g.tile_w * g.cols as f32 - 740.0).abs() < 0.01);
    }

    #[test]
    fn tile_under_a_point() {
        let list = Rect::new(228.0, 100.0, 772.0, 500.0);
        let g = geo(list.w, 48.0);
        // First tile, then the one below the first row.
        assert_eq!(pos_at(list, g, 0.0, 50, (250.0, 110.0)), Some(0));
        assert_eq!(
            pos_at(list, g, 0.0, 50, (250.0, (110.0 + g.tile_h) as f64)),
            Some(g.cols)
        );
        assert_eq!(
            pos_at(list, g, 0.0, 3, ((250.0 + g.tile_w * 4.0) as f64, 110.0)),
            None
        );
        assert_eq!(pos_at(list, g, 0.0, 50, (230.0, 110.0)), None);
    }

    #[test]
    fn geometry_follows_the_icon_size() {
        // Today's tile at the smallest size.
        let g = geo(772.0, 48.0);
        assert_eq!((tile_w(48.0), g.tile_h), (112.0, 122.0));
        let list = Rect::new(228.0, 100.0, 772.0, 800.0);
        for icon in [48.0, 80.0, 128.0, 192.0] {
            let g = geo(list.w, icon);
            assert!(g.cols >= 1 && g.tile_w >= tile_w(icon).min(list.w - 32.0));
            assert!((g.tile_w * g.cols as f32 - (list.w - 32.0).max(tile_w(icon))).abs() < 0.01);
            // The second row starts one tile height down.
            let y = 110.0 + g.tile_h as f64;
            assert_eq!(pos_at(list, g, 0.0, 50, (250.0, y)), Some(g.cols));
            // And scrolling by a row moves the hit by a row.
            assert_eq!(pos_at(list, g, g.tile_h, 50, (250.0, 110.0)), Some(g.cols));
        }
    }

    #[test]
    fn thumbnails_pick_the_cache_size() {
        assert_eq!(thumb_size(48.0, 1.0), thumbs::NORMAL);
        assert_eq!(thumb_size(192.0, 1.0), thumbs::LARGE);
        assert_eq!(thumb_size(48.0, 2.0), thumbs::LARGE);
    }
}
