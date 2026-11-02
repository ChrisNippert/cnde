//! Drag-to-edge snapping (GNOME style): while moving a window, touching the
//! top edge previews a maximize and the side edges preview a half-screen snap.
//! Releasing the button applies it.
//!
//! Two windows snapped to opposite sides of a monitor form a pair: the
//! second takes the space the first leaves, and dragging the edge they share
//! resizes both. Each keeps `WindowState::snap_split` while linked.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::element::{
        Kind,
        solid::{SolidColorBuffer, SolidColorRenderElement},
    },
    desktop::Window,
    output::Output,
    utils::{Logical, Point, Rectangle, Scale, Size},
};

use crate::{
    animation::{Animation, presets},
    decorations::{content_rect, titlebar_height},
    effects::lerp_rect,
    state::State,
    window::{Placement, placement_rect_split, window_placement_rect, with_state},
};

/// Width of a side zone along the top edge that still means "snap to side"
/// (at least the snap edge, `[snap] edge`).
const CORNER_SIZE: f64 = 48.0;

/// An existing snap narrower than this fraction of the usable width isn't
/// built around: a new snap beside it takes a normal half.
const MIN_PAIRED_FRACTION: f64 = 0.25;
/// How far a dragged shared edge may go towards either side of the monitor.
const MIN_SPLIT: f64 = 0.1;

// TODO: theme tokens (ARCHITECTURE.md §5.1) once the config crate exists.
const PREVIEW_COLOR: [f32; 4] = [0.35, 0.55, 0.95, 1.0];
const PREVIEW_FILL_ALPHA: f32 = 0.25;
const PREVIEW_BORDER_ALPHA: f32 = 0.7;
const PREVIEW_BORDER_WIDTH: i32 = 2;

/// How long the snap zones stay up after `[snap] edge` changes (Settings'
/// slider), then how long they fade.
const HINT_HOLD: Duration = Duration::from_millis(1200);
const HINT_FADE: Duration = Duration::from_millis(300);

/// Every monitor's snap zones, lit for a moment when their size changes, so
/// the slider shows what it does.
pub struct EdgeHint {
    since: Instant,
    /// Left, right, top.
    zones: [SolidColorBuffer; 3],
}

impl EdgeHint {
    pub fn new(now: Instant) -> Self {
        Self {
            since: now,
            zones: std::array::from_fn(|_| SolidColorBuffer::new(Size::default(), PREVIEW_COLOR)),
        }
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        now < self.since + HINT_HOLD + HINT_FADE
    }

    /// The zones within `area` (a monitor's usable area, global), `edge` px
    /// deep, with `output_loc` subtracted.
    pub fn render_elements(
        &mut self,
        area: Rectangle<i32, Logical>,
        edge: i32,
        output_loc: Point<i32, Logical>,
        scale: Scale<f64>,
        now: Instant,
    ) -> Vec<SolidColorRenderElement> {
        let gone = now.saturating_duration_since(self.since + HINT_HOLD);
        let level = 1.0 - (gone.as_secs_f32() / HINT_FADE.as_secs_f32()).min(1.0);
        if level <= 0.0 {
            return Vec::new();
        }
        let a = Rectangle::new(area.loc - output_loc, area.size);
        let edge = edge.min(a.size.w / 2).min(a.size.h);
        let rects = [
            Rectangle::new(a.loc, Size::from((edge, a.size.h))),
            Rectangle::new(
                (a.loc.x + a.size.w - edge, a.loc.y).into(),
                Size::from((edge, a.size.h)),
            ),
            // Between the sides, so the corners aren't lit twice.
            Rectangle::new(
                (a.loc.x + edge, a.loc.y).into(),
                Size::from((a.size.w - 2 * edge, edge)),
            ),
        ];
        self.zones
            .iter_mut()
            .zip(rects)
            .map(|(buffer, rect)| {
                buffer.resize(rect.size);
                SolidColorRenderElement::from_buffer(
                    buffer,
                    rect.loc.to_physical_precise_round(scale),
                    scale,
                    PREVIEW_BORDER_ALPHA * level,
                    Kind::Unspecified,
                )
            })
            .collect()
    }
}

impl State {
    /// The snap zones on `output`, while they're lit (see `EdgeHint`).
    pub fn edge_hint_elements(
        &mut self,
        output: &Output,
        now: Instant,
    ) -> Vec<crate::render::OutputElement> {
        let (Some(area), Some(geo)) = (self.usable_area(output), self.space.output_geometry(output))
        else {
            return Vec::new();
        };
        let edge = self.config.snap.edge as i32;
        let scale = Scale::from(output.current_scale().fractional_scale());
        let Some(hint) = &mut self.edge_hint else {
            return Vec::new();
        };
        hint.render_elements(area, edge, geo.loc, scale, now)
            .into_iter()
            .map(crate::render::OutputElement::Solid)
            .collect()
    }
}

pub struct SnapPreview {
    pub target: Placement,
    /// The monitor the preview is on; the snap lands there.
    pub output: Output,
    to: Rectangle<f64, Logical>,
    from: Rectangle<f64, Logical>,
    anim: Animation,
    fill: SolidColorBuffer,
    /// Top, bottom, left, right.
    borders: [SolidColorBuffer; 4],
}

impl SnapPreview {
    pub fn is_animating(&self, now: Instant) -> bool {
        !self.anim.is_done(now)
    }

    /// Fill then border elements (top first), with `output_loc` subtracted.
    pub fn render_elements(
        &mut self,
        output_loc: Point<f64, Logical>,
        scale: Scale<f64>,
        now: Instant,
    ) -> Vec<SolidColorRenderElement> {
        let rect = lerp_rect(self.from, self.to, self.anim.value_at(now));
        let rect = Rectangle::new(rect.loc - output_loc, rect.size).to_i32_round::<i32>();
        let w = PREVIEW_BORDER_WIDTH;
        let border_rects = [
            Rectangle::new(rect.loc, Size::from((rect.size.w, w))),
            Rectangle::new(
                (rect.loc.x, rect.loc.y + rect.size.h - w).into(),
                Size::from((rect.size.w, w)),
            ),
            Rectangle::new(rect.loc, Size::from((w, rect.size.h))),
            Rectangle::new(
                (rect.loc.x + rect.size.w - w, rect.loc.y).into(),
                Size::from((w, rect.size.h)),
            ),
        ];

        let mut elements = Vec::with_capacity(5);
        for (buffer, border) in self.borders.iter_mut().zip(border_rects) {
            buffer.resize(border.size);
            elements.push(SolidColorRenderElement::from_buffer(
                buffer,
                border.loc.to_physical_precise_round(scale),
                scale,
                PREVIEW_BORDER_ALPHA,
                Kind::Unspecified,
            ));
        }
        self.fill.resize(rect.size);
        elements.push(SolidColorRenderElement::from_buffer(
            &self.fill,
            rect.loc.to_physical_precise_round(scale),
            scale,
            PREVIEW_FILL_ALPHA,
            Kind::Unspecified,
        ));
        elements
    }
}

/// Which placement a drag ending at `pointer` would snap to within `area`,
/// snapping within `edge` logical px of its sides and top.
pub fn snap_target(
    pointer: Point<f64, Logical>,
    area: Rectangle<i32, Logical>,
    edge: f64,
) -> Option<Placement> {
    let area = area.to_f64();
    let left = pointer.x - area.loc.x;
    let right = area.loc.x + area.size.w - pointer.x;
    let top = pointer.y - area.loc.y;
    let corner = CORNER_SIZE.max(edge);

    if left <= edge || (top <= edge && left <= corner) {
        Some(Placement::SnappedLeft)
    } else if right <= edge || (top <= edge && right <= corner) {
        Some(Placement::SnappedRight)
    } else if top <= edge {
        Some(Placement::Maximized)
    } else {
        None
    }
}

/// The other side of a monitor, for the sides a window can snap to.
fn opposite(placement: Placement) -> Option<Placement> {
    match placement {
        Placement::SnappedLeft => Some(Placement::SnappedRight),
        Placement::SnappedRight => Some(Placement::SnappedLeft),
        _ => None,
    }
}

/// Where the shared edge goes when a window snaps to `target` beside
/// `other`, a snap on the opposite side occupying `other` within `area`: as
/// a fraction of the width, so the new window takes the remaining space. None
/// when `other` is too narrow to build around.
fn remaining_split(
    target: Placement,
    other: Rectangle<i32, Logical>,
    area: Rectangle<i32, Logical>,
) -> Option<f64> {
    if area.size.w <= 0 || (other.size.w as f64) < area.size.w as f64 * MIN_PAIRED_FRACTION {
        return None;
    }
    let edge = match target {
        Placement::SnappedLeft => other.loc.x,
        Placement::SnappedRight => other.loc.x + other.size.w,
        _ => return None,
    };
    Some((edge - area.loc.x) as f64 / area.size.w as f64)
}

/// The split a shared edge dragged to global `x` makes within `area`.
fn split_at(x: i32, area: Rectangle<i32, Logical>) -> f64 {
    ((x - area.loc.x) as f64 / area.size.w.max(1) as f64).clamp(MIN_SPLIT, 1.0 - MIN_SPLIT)
}

impl State {
    /// If snapping `window` to `target` on `output` (usable `area`) lands
    /// beside a window snapped to the other side: the split between them and
    /// that window.
    pub(crate) fn snap_pair_for(
        &self,
        window: &Window,
        target: Placement,
        area: Rectangle<i32, Logical>,
        output: &Output,
    ) -> Option<(f64, Window)> {
        let opposite = opposite(target)?;
        // Topmost first; minimized windows aren't in the space.
        let other = self.space.elements().rev().find(|w| {
            *w != window
                && with_state(w, |s| s.placement) == opposite
                && self.output_for(w).as_ref() == Some(output)
        })?;
        let split = with_state(other, |s| s.snap_split);
        let rect = placement_rect_split(opposite, area, split);
        Some((remaining_split(target, rect, area)?, other.clone()))
    }

    /// The window linked to `window` by a snap pair, if any.
    pub fn snap_partner(&self, window: &Window) -> Option<Window> {
        let (placement, linked) = with_state(window, |s| (s.placement, s.snap_split.is_some()));
        let opposite = opposite(placement).filter(|_| linked)?;
        let output = self.output_for(window)?;
        self.space
            .elements()
            .find(|w| {
                *w != window
                    && with_state(w, |s| s.placement == opposite && s.snap_split.is_some())
                    && self.output_for(w).as_ref() == Some(&output)
            })
            .cloned()
    }

    /// Global x of the edge a snap pair shares.
    pub fn snap_edge_x(&self, window: &Window) -> Option<i32> {
        let placement = with_state(window, |s| s.placement);
        let area = self.area_for(window, placement)?;
        let left = placement_rect_split(
            Placement::SnappedLeft,
            area,
            with_state(window, |s| s.snap_split),
        );
        Some(left.loc.x + left.size.w)
    }

    /// Move the edge `window` shares with its partner to global `x`,
    /// refitting both right away (called on every motion of the drag).
    pub fn drag_snap_edge(&mut self, window: &Window, x: i32) {
        let Some(partner) = self.snap_partner(window) else {
            return;
        };
        let placement = with_state(window, |s| s.placement);
        let Some(area) = self.area_for(window, placement) else {
            return;
        };
        let split = split_at(x, area);
        for w in [window, &partner] {
            with_state(w, |s| s.snap_split = Some(split));
            let placement = with_state(w, |s| s.placement);
            let rect = content_rect(window_placement_rect(w, placement, area), titlebar_height(w));
            if let Some(toplevel) = w.toplevel() {
                toplevel.with_pending_state(|s| s.size = Some(rect.size));
                toplevel.send_pending_configure();
            }
            self.move_window(w, rect.loc);
        }
        self.queue_redraw();
    }

    /// Called on every motion of an interactive move.
    /// A window dragged out of a tile (`was_tile`) never maximizes: dropped
    /// at the top edge it rejoins the tiles.
    pub fn update_snap_preview(
        &mut self,
        window: &Window,
        pointer: Point<f64, Logical>,
        was_tile: bool,
    ) {
        let output = self.space.output_under(pointer).next().cloned();
        // Snap within what panels leave free, like maximize does.
        let area = output.as_ref().and_then(|o| self.usable_area(o));
        let target = area
            .and_then(|area| snap_target(pointer, area, self.config.snap.edge as f64))
            .filter(|t| !(was_tile && *t == Placement::Maximized));

        let current = self
            .snap_preview
            .as_ref()
            .map(|p| (p.target, p.output.clone()));
        if current == target.zip(output.clone()) {
            return;
        }
        self.queue_redraw();
        let (Some(target), Some(area), Some(output)) = (target, area, output) else {
            self.snap_preview = None;
            return;
        };

        // Grow out of the window, or morph from the previous preview.
        let now = Instant::now();
        let from = match &self.snap_preview {
            Some(previous) => lerp_rect(previous.from, previous.to, previous.anim.value_at(now)),
            None => match self.space.element_geometry(window) {
                Some(geometry) => geometry.to_f64(),
                None => return,
            },
        };
        // The same geometry the snap will use, beside a snapped window or not.
        let split = self
            .snap_pair_for(window, target, area, &output)
            .map(|(split, _)| split);
        let buffer = || SolidColorBuffer::new(Size::default(), PREVIEW_COLOR);
        self.snap_preview = Some(SnapPreview {
            target,
            output,
            to: placement_rect_split(target, area, split).to_f64(),
            from,
            anim: Animation::new(0.0, 1.0, presets::SNAP_PREVIEW),
            fill: buffer(),
            borders: [buffer(), buffer(), buffer(), buffer()],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (1920, 1080).into())
    }

    #[test]
    fn edges_map_to_placements() {
        let at = |x, y| snap_target(Point::from((x, y)), area(), 10.0);
        assert_eq!(at(960.0, 0.0), Some(Placement::Maximized));
        assert_eq!(at(0.0, 500.0), Some(Placement::SnappedLeft));
        assert_eq!(at(1919.0, 500.0), Some(Placement::SnappedRight));
        assert_eq!(at(960.0, 500.0), None);
    }

    #[test]
    fn top_corners_prefer_sides() {
        let at = |x, y| snap_target(Point::from((x, y)), area(), 10.0);
        assert_eq!(at(20.0, 0.0), Some(Placement::SnappedLeft));
        assert_eq!(at(1900.0, 0.0), Some(Placement::SnappedRight));
    }

    #[test]
    fn new_snap_takes_the_remaining_space() {
        let rect = |x, w| Rectangle::new((x, 0).into(), (w, 1080).into());
        // Left is 60%: a right snap starts where it ends.
        let split = remaining_split(Placement::SnappedRight, rect(0, 1152), area()).unwrap();
        assert_eq!(
            placement_rect_split(Placement::SnappedRight, area(), Some(split)),
            rect(1152, 768)
        );
        // Right is 70%: a left snap ends where it starts.
        let split = remaining_split(Placement::SnappedLeft, rect(576, 1344), area()).unwrap();
        assert_eq!(
            placement_rect_split(Placement::SnappedLeft, area(), Some(split)),
            rect(0, 576)
        );
        // A plain half beside a plain half is the other half, odd widths included.
        let odd = Rectangle::new((0, 0).into(), (1921, 1080).into());
        let half = placement_rect_split(Placement::SnappedLeft, odd, None);
        let split = remaining_split(Placement::SnappedRight, half, odd).unwrap();
        assert_eq!(
            placement_rect_split(Placement::SnappedRight, odd, Some(split)),
            placement_rect_split(Placement::SnappedRight, odd, None)
        );
        // Offset areas (a panel on the left, a second monitor).
        let offset = Rectangle::new((100, 30).into(), (1000, 500).into());
        let left = placement_rect_split(Placement::SnappedLeft, offset, Some(0.3));
        let split = remaining_split(Placement::SnappedRight, left, offset).unwrap();
        assert_eq!(
            placement_rect_split(Placement::SnappedRight, offset, Some(split)).loc.x,
            400
        );
    }

    #[test]
    fn narrow_snaps_are_not_built_around() {
        let narrow = |w| Rectangle::new((0, 0).into(), (w, 1080).into());
        assert!(remaining_split(Placement::SnappedRight, narrow(479), area()).is_none());
        assert!(remaining_split(Placement::SnappedRight, narrow(480), area()).is_some());
        let right = |w| Rectangle::new((1920 - w, 0).into(), (w, 1080).into());
        assert!(remaining_split(Placement::SnappedLeft, right(400), area()).is_none());
        assert!(remaining_split(Placement::SnappedLeft, right(500), area()).is_some());
    }

    #[test]
    fn dragged_edge_stays_on_screen() {
        assert_eq!(split_at(960, area()), 0.5);
        assert_eq!(split_at(0, area()), MIN_SPLIT);
        assert_eq!(split_at(5000, area()), 1.0 - MIN_SPLIT);
    }
}
