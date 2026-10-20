//! Where everything sits: a panel centred on the screen with a title, a
//! grid of monitor cards and a Cancel button. Logical pixels; pure
//! functions, so hit-testing is testable without a compositor.

use cnde_ui::Rect;

pub const CARD_W: f32 = 264.0;
pub const CARD_H: f32 = 206.0;
/// Inside a card: space around the thumbnail.
pub const CARD_PAD: f32 = 10.0;
pub const THUMB_H: f32 = 136.0;
pub const GAP: f32 = 12.0;
const PAD: f32 = 26.0;
const TITLE_H: f32 = 60.0;
/// The tab control's row, below the title.
const TABS_H: f32 = 48.0;
const FOOTER_H: f32 = 34.0;
const CANCEL_W: f32 = 96.0;
/// Cards per row at most.
const MAX_COLUMNS: usize = 3;
/// Space kept free either side of the panel.
const MARGIN: f32 = 48.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub panel: Rect,
    pub title: (f32, f32),
    /// Top-left of the Screens / Windows control.
    pub tabs: (f32, f32),
    pub cards: Vec<Rect>,
    pub cancel: Rect,
    pub columns: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Card(usize),
    /// A segment of the tab control.
    Tab(usize),
    Cancel,
    /// On the panel but nothing in particular.
    Panel,
    /// The dimmed screen around it: clicking cancels.
    Outside,
}

#[cfg(test)]
pub fn layout(size: (f32, f32), count: usize) -> Layout {
    layout_for(size, count, count)
}

/// `count` cards in a panel big enough for `slots` (so it keeps its size
/// when the tab changes).
pub fn layout_for((sw, sh): (f32, f32), count: usize, slots: usize) -> Layout {
    let slots = slots.max(count);
    let fit = ((sw - 2.0 * (PAD + MARGIN) + GAP) / (CARD_W + GAP)).floor();
    let columns = (fit.max(1.0) as usize).min(MAX_COLUMNS).min(slots.max(1));
    let rows = slots.max(1).div_ceil(columns);
    let grid_w = columns as f32 * CARD_W + (columns - 1) as f32 * GAP;
    let grid_h = rows as f32 * CARD_H + (rows - 1) as f32 * GAP;
    let w = grid_w.max(CANCEL_W + 120.0) + 2.0 * PAD;
    let h = PAD + TITLE_H + TABS_H + grid_h + GAP + FOOTER_H + PAD;
    let x = ((sw - w) / 2.0).round();
    let y = ((sh - h) / 2.0).round().max(0.0);
    let grid_x = x + (w - grid_w) / 2.0;
    let grid_y = y + PAD + TITLE_H + TABS_H;
    let cards = (0..count)
        .map(|i| {
            let (col, row) = (i % columns, i / columns);
            Rect::new(
                grid_x + col as f32 * (CARD_W + GAP),
                grid_y + row as f32 * (CARD_H + GAP),
                CARD_W,
                CARD_H,
            )
        })
        .collect();
    Layout {
        panel: Rect::new(x, y, w, h),
        title: (x + PAD, y + PAD),
        tabs: (x + PAD, y + PAD + TITLE_H),
        cards,
        cancel: Rect::new(
            x + w - PAD - CANCEL_W,
            y + h - PAD - FOOTER_H,
            CANCEL_W,
            FOOTER_H,
        ),
        columns,
    }
}

impl Layout {
    pub fn hit(&self, pos: (f64, f64)) -> Hit {
        if let Some(i) = self.cards.iter().position(|c| c.contains(pos)) {
            Hit::Card(i)
        } else if self.cancel.contains(pos) {
            Hit::Cancel
        } else if self.panel.contains(pos) {
            Hit::Panel
        } else {
            Hit::Outside
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn centre(r: &Rect) -> (f64, f64) {
        ((r.x + r.w / 2.0) as f64, (r.y + r.h / 2.0) as f64)
    }

    #[test]
    fn cards_are_centred_and_inside_the_panel() {
        let l = layout((1920.0, 1080.0), 2);
        assert_eq!(l.columns, 2);
        assert_eq!(l.cards.len(), 2);
        let mid = l.panel.x + l.panel.w / 2.0;
        assert!((mid - 960.0).abs() <= 1.0);
        for c in &l.cards {
            assert!(c.x >= l.panel.x && c.right() <= l.panel.right());
            assert!(c.y >= l.panel.y && c.bottom() <= l.cancel.y);
        }
    }

    #[test]
    fn rows_wrap_after_three() {
        let l = layout((2560.0, 1440.0), 4);
        assert_eq!(l.columns, 3);
        assert!(l.cards[3].y > l.cards[0].y);
        assert_eq!(l.cards[3].x, l.cards[0].x);
    }

    #[test]
    fn narrow_screens_get_fewer_columns() {
        assert_eq!(layout((600.0, 900.0), 3).columns, 1);
    }

    #[test]
    fn window_grids_wrap_and_empty_lists_keep_a_row() {
        let l = layout((1920.0, 1080.0), 7);
        assert_eq!((l.columns, l.cards.len()), (3, 7));
        assert!(l.cards[6].y > l.cards[3].y && l.cards[6].x == l.cards[0].x);
        let none = layout((1920.0, 1080.0), 0);
        assert!(none.cards.is_empty() && none.panel.h > CARD_H);
        // The tab control sits above the first card.
        assert!(l.tabs.1 + 40.0 <= l.cards[0].y);
    }

    #[test]
    fn hits() {
        let l = layout((1920.0, 1080.0), 2);
        assert_eq!(l.hit(centre(&l.cards[1])), Hit::Card(1));
        assert_eq!(l.hit(centre(&l.cancel)), Hit::Cancel);
        assert_eq!(
            l.hit((l.panel.x as f64 + 2.0, l.panel.y as f64 + 2.0)),
            Hit::Panel
        );
        assert_eq!(l.hit((2.0, 2.0)), Hit::Outside);
        // The gap between cards belongs to the panel, not a card.
        let gap = (l.cards[0].right() as f64 + 2.0, centre(&l.cards[0]).1);
        assert_eq!(l.hit(gap), Hit::Panel);
    }
}
