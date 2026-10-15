//! Offscreen captures of a window's contents, for animations that must keep
//! showing a window after its buffer is gone (close) or about to change (resize).

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind,
                surface::WaylandSurfaceRenderElement,
                texture::{TextureBuffer, TextureRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    utils::{Logical, Point, Rectangle, Scale, Size, Transform},
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

        let physical_size = bbox.size.to_physical_precise_ceil(scale);
        let buffer_size = physical_size.to_logical(1).to_buffer(1, Transform::Normal);
        let mut texture: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr8888, buffer_size)
            .map_err(|err| warn!(?err, "failed to allocate snapshot texture"))
            .ok()?;

        // Place the bounding box's top-left at the texture origin.
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
            renderer,
            (Point::default() - bbox.loc).to_physical_precise_round(scale),
            scale,
            1.0,
        );
        {
            let mut framebuffer = renderer
                .bind(&mut texture)
                .map_err(|err| warn!(?err, "failed to bind snapshot texture"))
                .ok()?;
            let mut tracker = OutputDamageTracker::new(physical_size, scale, Transform::Normal);
            tracker
                .render_output(renderer, &mut framebuffer, 0, &elements, [0.0; 4])
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
        Point<i32, smithay::utils::Physical>,
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
