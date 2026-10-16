//! Offscreen captures of a window's contents, for animations that must keep
//! showing a window after its buffer is gone (close) or about to change (resize).

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind, RenderElement,
                surface::WaylandSurfaceRenderElement,
                texture::{TextureBuffer, TextureRenderElement},
                utils::{Relocate, RelocateRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};
use tracing::warn;

pub struct Snapshot {
    buffer: TextureBuffer<GlesTexture>,
    /// Where the bounding box (including client-side shadows) starts relative
    /// to the window geometry. Usually negative.
    bbox_offset: Point<i32, Logical>,
    bbox_size: Size<i32, Logical>,
    /// Size of the window geometry when captured.
    pub geometry_size: Size<i32, Logical>,
}

/// The surfaces of a window as they were last drawn, kept so a window
/// whose client was killed can still animate out: by the time we learn the
/// toplevel is gone, its surfaces (and their buffers) already are.
pub struct LastFrame {
    /// Rendered with the surface origin at (0, 0).
    elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    scale: Scale<f64>,
    geometry: Rectangle<i32, Logical>,
    bbox: Rectangle<i32, Logical>,
    /// Where the window geometry was on screen (space coordinates).
    pub rect: Rectangle<f64, Logical>,
}

impl LastFrame {
    pub fn record(
        renderer: &mut GlesRenderer,
        window: &Window,
        scale: Scale<f64>,
        rect: Rectangle<f64, Logical>,
    ) -> Self {
        Self {
            elements: window.render_elements(renderer, Point::default(), scale, 1.0),
            scale,
            geometry: window.geometry(),
            bbox: window.bbox(),
            rect,
        }
    }
}

impl Snapshot {
    /// Render `window`'s current surfaces into a texture at `scale`.
    pub fn capture(
        renderer: &mut GlesRenderer,
        window: &Window,
        scale: Scale<f64>,
    ) -> Option<Self> {
        let geometry = window.geometry();
        let bbox = window.bbox();
        if bbox.is_empty() || geometry.is_empty() {
            return None;
        }
        // Place the bounding box's top-left at the texture origin.
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
            renderer,
            (Point::default() - bbox.loc).to_physical_precise_round(scale),
            scale,
            1.0,
        );
        Self::render(renderer, &elements, scale, geometry, bbox)
    }

    /// Like [`capture`](Self::capture), from a window's remembered last frame.
    pub fn from_last_frame(renderer: &mut GlesRenderer, frame: LastFrame) -> Option<Self> {
        if frame.bbox.is_empty() || frame.geometry.is_empty() {
            return None;
        }
        let bbox_origin: Point<i32, Physical> =
            frame.bbox.loc.to_physical_precise_round(frame.scale);
        let elements: Vec<_> = frame
            .elements
            .into_iter()
            .map(|element| {
                RelocateRenderElement::from_element(
                    element,
                    Point::default() - bbox_origin,
                    Relocate::Relative,
                )
            })
            .collect();
        Self::render(renderer, &elements, frame.scale, frame.geometry, frame.bbox)
    }

    /// From surface elements already drawn with the bounding box's top-left
    /// at the origin (`geometry` and `bbox` relative to the surface).
    pub fn from_elements(
        renderer: &mut GlesRenderer,
        elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
        scale: Scale<f64>,
        geometry: Rectangle<i32, Logical>,
        bbox: Rectangle<i32, Logical>,
    ) -> Option<Self> {
        if bbox.is_empty() || geometry.is_empty() {
            return None;
        }
        Self::render(renderer, &elements, scale, geometry, bbox)
    }

    fn render<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        elements: &[E],
        scale: Scale<f64>,
        geometry: Rectangle<i32, Logical>,
        bbox: Rectangle<i32, Logical>,
    ) -> Option<Self> {
        let physical_size = bbox.size.to_physical_precise_ceil(scale);
        let buffer_size = physical_size.to_logical(1).to_buffer(1, Transform::Normal);
        let mut texture: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, buffer_size)
            .map_err(|err| warn!(?err, "failed to allocate snapshot texture"))
            .ok()?;
        {
            let mut framebuffer = renderer
                .bind(&mut texture)
                .map_err(|err| warn!(?err, "failed to bind snapshot texture"))
                .ok()?;
            let mut tracker = OutputDamageTracker::new(physical_size, scale, Transform::Normal);
            tracker
                .render_output(renderer, &mut framebuffer, 0, elements, [0.0; 4])
                .map_err(|err| warn!(?err, "failed to render snapshot"))
                .ok()?;
        }
        Some(Self {
            buffer: TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None),
            bbox_offset: bbox.loc - geometry.loc,
            bbox_size: bbox.size,
            geometry_size: geometry.size,
        })
    }

    /// An element drawing the snapshot so its window geometry lands on `rect`
    /// (output-relative, logical), stretched if the sizes differ.
    pub fn render_element(
        &self,
        rect: Rectangle<f64, Logical>,
        output_scale: Scale<f64>,
        alpha: f32,
    ) -> (
        TextureRenderElement<GlesTexture>,
        Point<i32, Physical>,
        Scale<f64>,
    ) {
        let location = (rect.loc + self.bbox_offset.to_f64()).to_physical(output_scale);
        let element = TextureRenderElement::from_texture_buffer(
            location,
            &self.buffer,
            Some(alpha),
            None,
            Some(self.bbox_size),
            Kind::Unspecified,
        );
        let origin = rect.loc.to_physical(output_scale).to_i32_round();
        let stretch = Scale {
            x: rect.size.w / self.geometry_size.w.max(1) as f64,
            y: rect.size.h / self.geometry_size.h.max(1) as f64,
        };
        (element, origin, stretch)
    }
}
