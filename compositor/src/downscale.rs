//! Windows drawn well below their size (the overview, the dock's preview,
//! Alt+Tab) averaged down instead of point-sampled.
//!
//! The stock texture shader takes one linear sample per pixel: shrunk 13x,
//! a thumbnail pixel sees 4 of the ~170 texels under it, so text breaks up
//! and edges shimmer as it moves. This shader takes a grid of samples
//! across each pixel's footprint and averages them.

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};
use tracing::warn;

/// Shrunk less than this, the stock shader's single sample is fine.
pub const MIN_SHRINK: f64 = 1.25;

/// `shrink`: how many texels wide a pixel is, about; the samples spread
/// over all but the one texel each linear sample already blends.
const SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
#extension GL_OES_standard_derivatives : require

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

uniform float shrink;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    float spread = max(1.0 - 1.0 / shrink, 0.0);
    vec2 across = dFdx(v_coords) * spread;
    vec2 down = dFdy(v_coords) * spread;
    // A 5x5 grid, each sample in the middle of its fifth of the pixel.
    vec4 sum = vec4(0.0);
    for (int i = 0; i < 5; i++) {
        for (int j = 0; j < 5; j++) {
            vec2 at = v_coords
                + across * ((float(i) - 2.0) / 5.0)
                + down * ((float(j) - 2.0) / 5.0);
            sum += texture2D(tex, at);
        }
    }
    vec4 color = sum / 25.0;
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif
    gl_FragColor = color * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        gl_FragColor = vec4(0.0, 0.2, 0.0, 0.2) + gl_FragColor * 0.8;
#endif
}
"#;

/// The shader, compiled on first use.
#[derive(Default)]
pub struct Downscale {
    program: Option<GlesTexProgram>,
    /// It wouldn't compile (no derivatives in this GL): windows draw as
    /// before.
    broken: bool,
}

impl Downscale {
    pub fn program(&mut self, renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
        if self.program.is_none() && !self.broken {
            match renderer.compile_custom_texture_shader(
                SHADER,
                &[UniformName::new("shrink", UniformType::_1f)],
            ) {
                Ok(program) => self.program = Some(program),
                Err(err) => {
                    warn!(
                        ?err,
                        "can't compile the thumbnail shader; thumbnails stay sharp-edged"
                    );
                    self.broken = true;
                }
            }
        }
        self.program.clone()
    }
}

/// `element`, drawn with the averaging shader, `shrink` times smaller than
/// its texture.
#[derive(Debug)]
pub struct Downscaled<E> {
    element: E,
    program: GlesTexProgram,
    shrink: f32,
}

impl<E> Downscaled<E> {
    pub fn new(element: E, program: GlesTexProgram, shrink: f64) -> Self {
        Self {
            element,
            program,
            shrink: shrink as f32,
        }
    }
}

impl<E: Element> Element for Downscaled<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.element.src()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.element.geometry(scale)
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.element.location(scale)
    }
    fn transform(&self) -> Transform {
        self.element.transform()
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.element.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.element.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.element.alpha()
    }
    fn kind(&self) -> Kind {
        self.element.kind()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Downscaled<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(
            self.program.clone(),
            vec![Uniform::new("shrink", self.shrink)],
        );
        let result = self.element.draw(frame, src, dst, damage, opaque_regions);
        frame.clear_tex_program_override();
        result
    }

    // Not handed to a scanout plane: it has to be drawn to be averaged.
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
