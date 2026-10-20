//! A blurred copy of what an output shows, for the Alt+Tab switcher's
//! backdrop (and its panel's frost): the scene is drawn once into a small
//! texture when the switcher appears, blurred there by a couple of cheap
//! separable passes, and then held until it closes. The windows behind are
//! dimmed and not about to change, so it never needs redrawing.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            damage::OutputDamageTracker,
            element::{
                Kind, RenderElement,
                texture::{TextureBuffer, TextureRenderElement},
            },
            gles::{
                GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
                element::TextureShaderElement,
            },
        },
    },
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};
use tracing::{debug, warn};

use crate::render::OutputElement;

/// The scene is drawn this many times smaller than the screen; the GPU's
/// filtering scales the blur back up.
const DOWN: i32 = 6;
/// Blur passes (each one horizontal, then vertical).
const PASSES: usize = 2;
/// How far apart the taps sit, in texels: wider is blurrier.
const SPREAD: f32 = 1.5;

/// A 9-tap Gaussian along `step`, with tap pairs folded into bilinear
/// fetches.
const BLUR: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision mediump float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

uniform vec2 step;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec4 c = texture2D(tex, v_coords) * 0.2270270270;
    c += (texture2D(tex, v_coords + step * 1.3846153846)
        + texture2D(tex, v_coords - step * 1.3846153846)) * 0.3162162162;
    c += (texture2D(tex, v_coords + step * 3.2307692308)
        + texture2D(tex, v_coords - step * 3.2307692308)) * 0.0702702703;
#if defined(NO_ALPHA)
    c = vec4(c.rgb, 1.0);
#endif
    gl_FragColor = c * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        gl_FragColor = vec4(0.0, 0.2, 0.0, 0.2) + gl_FragColor * 0.8;
#endif
}
"#;

struct Held {
    output: String,
    buffer: TextureBuffer<GlesTexture>,
    size: Size<i32, Buffer>,
    /// The output's size, logical.
    logical: Size<i32, Logical>,
}

#[derive(Default)]
pub struct SceneBlur {
    shader: Option<GlesTexProgram>,
    /// Shader failed to compile: don't keep trying.
    broken: bool,
    held: Option<Held>,
}

/// Draw `elements` into a new texture of `size` (not waiting for it: the
/// next pass uses it from the same context).
fn draw<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> Option<GlesTexture> {
    let mut texture: GlesTexture = renderer
        .create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, Buffer>::from((size.w, size.h)),
        )
        .ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        tracker
            .render_output(renderer, &mut target, 0, elements, [0.0, 0.0, 0.0, 1.0])
            .ok()?;
    }
    Some(texture)
}

impl SceneBlur {
    /// Whether a blurred copy of `output` is held.
    pub fn has(&self, output: &Output) -> bool {
        self.held
            .as_ref()
            .is_some_and(|h| h.output == output.name())
    }

    /// Let go of the copy (the switcher has closed).
    pub fn release(&mut self) {
        self.held = None;
    }

    /// Draw `scene` (top first, as the renderer takes it), small, and blur it.
    pub fn capture(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        scene: &[OutputElement],
    ) {
        let started = std::time::Instant::now();
        if self.shader.is_none() && !self.broken {
            match renderer
                .compile_custom_texture_shader(BLUR, &[UniformName::new("step", UniformType::_2f)])
            {
                Ok(shader) => self.shader = Some(shader),
                Err(err) => {
                    warn!(
                        ?err,
                        "can't compile the blur shader; the switcher only dims"
                    );
                    self.broken = true;
                }
            }
        }
        let (Some(shader), Some(mode)) = (self.shader.clone(), output.current_mode()) else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        let full = output.current_transform().transform_size(mode.size);
        let size =
            Size::<i32, Physical>::from(((full.w + DOWN - 1) / DOWN, (full.h + DOWN - 1) / DOWN));
        let Some(mut texture) = draw(renderer, scene, size, Scale::from(scale / DOWN as f64))
        else {
            warn!("can't draw the scene for the switcher's blur");
            return;
        };
        let texel = [1.0 / size.w as f32, 1.0 / size.h as f32];
        for pass in 0..PASSES * 2 {
            let step = if pass % 2 == 0 {
                [texel[0] * SPREAD, 0.0]
            } else {
                [0.0, texel[1] * SPREAD]
            };
            let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
            let source = TextureRenderElement::from_texture_buffer(
                (0.0, 0.0),
                &buffer,
                Some(1.0),
                None,
                None,
                Kind::Unspecified,
            );
            let element =
                TextureShaderElement::new(source, shader.clone(), vec![Uniform::new("step", step)]);
            let Some(next) = draw(renderer, &[element], size, Scale::from(1.0)) else {
                warn!("can't blur the scene for the switcher");
                return;
            };
            texture = next;
        }
        let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
        let logical = full.to_f64().to_logical(scale).to_i32_round();
        self.held = Some(Held {
            output: output.name(),
            buffer,
            size: Size::from((size.w, size.h)),
            logical,
        });
        debug!(
            ms = started.elapsed().as_secs_f64() * 1000.0,
            width = size.w,
            height = size.h,
            "switcher backdrop blurred (cpu side)"
        );
    }

    /// The blurred scene over the whole of `output`, at `alpha`.
    pub fn element(
        &self,
        output: &Output,
        alpha: f32,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        let held = self.held.as_ref().filter(|h| h.output == output.name())?;
        Some(crate::wallpaper::full_screen(
            &held.buffer,
            held.size.w,
            held.size.h,
            held.logical,
            Some(alpha),
        ))
    }

    /// The part under `area` (logical, output-local), and where it sits in
    /// the texture (x0, y0, x1, y1), for a frosted panel.
    pub fn crop(
        &self,
        output: &Output,
        area: Rectangle<i32, Logical>,
        alpha: f32,
    ) -> Option<(TextureRenderElement<GlesTexture>, [f32; 4])> {
        let held = self.held.as_ref().filter(|h| h.output == output.name())?;
        let scale = output.current_scale().fractional_scale();
        let (fx, fy) = (
            held.size.w as f64 / held.logical.w.max(1) as f64,
            held.size.h as f64 / held.logical.h.max(1) as f64,
        );
        let area = area.to_f64();
        let src = Rectangle::new(
            Point::from((area.loc.x * fx, area.loc.y * fy)),
            Size::from((area.size.w * fx, area.size.h * fy)),
        );
        let (w, h) = (held.size.w as f64, held.size.h as f64);
        let crop = [
            (src.loc.x / w) as f32,
            (src.loc.y / h) as f32,
            ((src.loc.x + src.size.w) / w) as f32,
            ((src.loc.y + src.size.h) / h) as f32,
        ];
        let element = TextureRenderElement::from_texture_buffer(
            area.loc.to_physical(scale),
            &held.buffer,
            Some(alpha),
            Some(src),
            Some(area.size.to_i32_round()),
            Kind::Unspecified,
        );
        Some((element, crop))
    }
}
