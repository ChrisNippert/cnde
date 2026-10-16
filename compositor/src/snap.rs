//! Drag-to-edge snapping (GNOME style): while moving a window, touching the
//! top edge previews a maximize and the side edges preview a half-screen snap.
//! Releasing the button applies it.

use std::time::Instant;

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
    effects::lerp_rect,
    state::State,
    window::{Placement, placement_rect},
};

/// How close (logical px) the pointer must get to an edge. Generous because,
/// nested, the host stops sending motion once the pointer leaves our window.
const EDGE_THRESHOLD: f64 = 10.0;
/// Width of a side zone along the top edge that still means "snap to side".
const CORNER_SIZE: f64 = 48.0;

// TODO: theme tokens (ARCHITECTURE.md §5.1) once the config crate exists.
const PREVIEW_COLOR: [f32; 4] = [0.35, 0.55, 0.95, 1.0];
const PREVIEW_FILL_ALPHA: f32 = 0.25;
const PREVIEW_BORDER_ALPHA: f32 = 0.7;
const PREVIEW_BORDER_WIDTH: i32 = 2;

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

/// Which placement a drag ending at `pointer` would snap to within `area`.
pub fn snap_target(
    pointer: Point<f64, Logical>,
    area: Rectangle<i32, Logical>,
) -> Option<Placement> {
    let area = area.to_f64();
    let left = pointer.x - area.loc.x;
    let right = area.loc.x + area.size.w - pointer.x;
    let top = pointer.y - area.loc.y;

    if left <= EDGE_THRESHOLD || (top <= EDGE_THRESHOLD && left <= CORNER_SIZE) {
        Some(Placement::SnappedLeft)
    } else if right <= EDGE_THRESHOLD || (top <= EDGE_THRESHOLD && right <= CORNER_SIZE) {
        Some(Placement::SnappedRight)
    } else if top <= EDGE_THRESHOLD {
        Some(Placement::Maximized)
    } else {
        None
    }
}

impl State {
    /// Called on every motion of an interactive move.
    pub fn update_snap_preview(&mut self, window: &Window, pointer: Point<f64, Logical>) {
        let output = self.space.output_under(pointer).next().cloned();
        // Snap within what panels leave free, like maximize does.
        let area = output.as_ref().and_then(|o| self.usable_area(o));
        let target = area.and_then(|area| snap_target(pointer, area));

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
        let buffer = || SolidColorBuffer::new(Size::default(), PREVIEW_COLOR);
        self.snap_preview = Some(SnapPreview {
            target,
            output,
            to: placement_rect(target, area).to_f64(),
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
        let at = |x, y| snap_target(Point::from((x, y)), area());
        assert_eq!(at(960.0, 0.0), Some(Placement::Maximized));
        assert_eq!(at(0.0, 500.0), Some(Placement::SnappedLeft));
        assert_eq!(at(1919.0, 500.0), Some(Placement::SnappedRight));
        assert_eq!(at(960.0, 500.0), None);
    }

    #[test]
    fn top_corners_prefer_sides() {
        let at = |x, y| snap_target(Point::from((x, y)), area());
        assert_eq!(at(20.0, 0.0), Some(Placement::SnappedLeft));
        assert_eq!(at(1900.0, 0.0), Some(Placement::SnappedRight));
    }
}
