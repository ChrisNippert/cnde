//! Scrollbars: the thumb's geometry, painting, and what a press or a drag
//! on one does. A thin line at rest; it widens under the pointer.

use cnde_ui::{Color, Painter, Rect};

use crate::{
    App,
    view::{Painted, Target},
};

/// Which scrollable area a bar belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// The folder list, the grid and the pages.
    List,
    Side,
}

/// Shortest thumb, so it stays easy to grab in long folders.
const MIN_THUMB: f32 = 32.0;
const REST_W: f32 = 4.0;
const HOVER_W: f32 = 10.0;
/// Gap between the bar and the area's edge.
const MARGIN: f32 = 3.0;
/// Width of the strip that takes the pointer.
const HIT_W: f32 = 14.0;
/// Space kept above and below the track.
const INSET: f32 = 4.0;

/// Where a bar's thumb is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geo {
    pub track_y: f32,
    pub track_h: f32,
    pub thumb_y: f32,
    pub thumb_h: f32,
    /// How far the area scrolls.
    pub range: f32,
    /// The visible height: a click on the track moves one page.
    pub page: f32,
}

impl Geo {
    /// The bar for a view showing `content` px of height at `scroll`;
    /// `None` when it all fits.
    pub fn new(view: Rect, content: f32, scroll: f32) -> Option<Geo> {
        if content <= view.h + 0.5 {
            return None;
        }
        let track_h = (view.h - INSET * 2.0).max(1.0);
        let thumb_h = (track_h * view.h / content).clamp(MIN_THUMB.min(track_h), track_h);
        let range = (content - view.h).max(1.0);
        let thumb_y = view.y + INSET + (track_h - thumb_h) * (scroll / range).clamp(0.0, 1.0);
        Some(Geo {
            track_y: view.y + INSET,
            track_h,
            thumb_y,
            thumb_h,
            range,
            page: view.h,
        })
    }

    pub fn on_thumb(&self, y: f64) -> bool {
        y >= self.thumb_y as f64 && y <= (self.thumb_y + self.thumb_h) as f64
    }

    /// The scroll offset that puts the thumb's top at `top`.
    pub fn scroll_for(&self, top: f32) -> f32 {
        let free = (self.track_h - self.thumb_h).max(1.0);
        ((top - self.track_y) / free).clamp(0.0, 1.0) * self.range
    }
}

/// A drag of a thumb in progress.
#[derive(Debug, Clone, Copy)]
pub struct Drag {
    pub bar: Bar,
    /// Pointer y minus the thumb's top when it was grabbed.
    pub grab: f32,
}

/// Draw the bar of `view` (the area's visible rectangle) and register what
/// takes the pointer. `lit` is hover or an ongoing drag.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    p: &mut Painter<'_, '_>,
    out: &mut Painted,
    bar: Bar,
    view: Rect,
    content: f32,
    scroll: f32,
    ink: Color,
    lit: bool,
) {
    let Some(geo) = Geo::new(view, content, scroll) else {
        return;
    };
    let w = if lit { HOVER_W } else { REST_W };
    let x = view.right() - MARGIN - w;
    if lit {
        p.canvas.rounded_rect(
            x,
            geo.track_y,
            w,
            geo.track_h,
            w / 2.0,
            ink.with_alpha(0.06),
        );
    }
    let alpha = if lit { 0.5 } else { 0.25 };
    p.canvas.rounded_rect(
        x,
        geo.thumb_y,
        w,
        geo.thumb_h,
        w / 2.0,
        ink.with_alpha(alpha),
    );
    let hit = Rect::new(view.right() - HIT_W, view.y, HIT_W, view.h);
    out.hits.push((hit, Target::Scroll(bar)));
    out.bars.push((bar, geo));
}

impl App {
    fn bar_geo(&self, bar: Bar) -> Option<Geo> {
        self.painted
            .bars
            .iter()
            .find(|(b, _)| *b == bar)
            .map(|(_, g)| *g)
    }

    /// A press on a bar: grab the thumb, or page toward the click.
    pub(crate) fn bar_press(&mut self, bar: Bar, at: (f64, f64)) {
        let Some(geo) = self.bar_geo(bar) else {
            return;
        };
        if geo.on_thumb(at.1) {
            let grab = at.1 as f32 - geo.thumb_y;
            self.ui.x.bar_drag = Some(Drag { bar, grab });
        } else {
            let dir = if at.1 < geo.thumb_y as f64 { -1.0 } else { 1.0 };
            let step = dir * geo.page * 0.9;
            match bar {
                Bar::List => self.scroll_by(step, true),
                Bar::Side => {
                    self.ui.side_scroll = (self.ui.side_scroll + step).clamp(0.0, geo.range);
                }
            }
        }
        self.redraw();
    }

    /// Pointer motion; true while a thumb is held.
    pub(crate) fn bar_motion(&mut self, at: (f64, f64)) -> bool {
        let Some(drag) = self.ui.x.bar_drag else {
            return false;
        };
        let Some(geo) = self.bar_geo(drag.bar) else {
            self.ui.x.bar_drag = None;
            return false;
        };
        let scroll = geo.scroll_for(at.1 as f32 - drag.grab);
        match drag.bar {
            Bar::List => {
                self.ui.scroll = scroll;
                self.scroll_target = scroll;
            }
            Bar::Side => self.ui.side_scroll = scroll,
        }
        self.redraw();
        true
    }

    pub(crate) fn bar_release(&mut self) {
        if self.ui.x.bar_drag.take().is_some() {
            self.redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_follows_scroll_and_back() {
        let view = Rect::new(0.0, 100.0, 300.0, 500.0);
        assert!(Geo::new(view, 400.0, 0.0).is_none());
        let g = Geo::new(view, 5000.0, 0.0).unwrap();
        assert_eq!(g.thumb_y, g.track_y);
        assert!(g.thumb_h >= MIN_THUMB);
        let end = Geo::new(view, 5000.0, g.range).unwrap();
        assert!((end.thumb_y + end.thumb_h - (g.track_y + g.track_h)).abs() < 0.01);
        let mid = Geo::new(view, 5000.0, 1500.0).unwrap();
        assert!((g.scroll_for(mid.thumb_y) - 1500.0).abs() < 0.5);
        assert_eq!(g.scroll_for(-50.0), 0.0);
        assert_eq!(g.scroll_for(1e6), g.range);
    }

    #[test]
    fn tiny_thumb_has_a_floor() {
        let view = Rect::new(0.0, 0.0, 300.0, 500.0);
        let g = Geo::new(view, 1_000_000.0, 0.0).unwrap();
        assert_eq!(g.thumb_h, MIN_THUMB);
    }
}
