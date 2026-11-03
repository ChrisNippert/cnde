//! A blurred copy of what an output shows, for the Alt+Tab switcher's
//! backdrop (and its panel's frost): the scene is drawn once into a small
//! texture when the switcher appears, blurred there by a couple of cheap
//! separable passes, and then held until it closes. The windows behind are
//! dimmed and not about to change, so it never needs redrawing.
//!
//! `LiveBlur` is the continuous kind, for the frost under the bar, dock,
//! popups and cards when `live_blur` is on: the same shader, but the copy is
//! kept up to date, and only redrawn and reblurred when something in the
//! scene below it has changed.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen,
            damage::OutputDamageTracker,
            element::{
                Kind, RenderElement,
                utils::RescaleRenderElement,
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
use std::collections::HashMap;

use tracing::{debug, warn};

use crate::render::OutputElement;

/// The scene is first drawn at half size, then halved by plain bilinear
/// draws (each one averaging 2x2 texels, so nothing sparkles or goes
/// blocky) down to 1/`DEEP`, blurred there, and drawn back up to 1/`KEEP`,
/// which is what's held: smooth enough to stretch, and small.
const DEEP: i32 = 8;
const KEEP: i32 = 4;
/// Blur passes at the smallest size (each one horizontal, then vertical).
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
    /// One per output.
    held: Vec<Held>,
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

/// `texture` (of `from` texels) drawn stretched to `to`, through `shader`
/// with `uniforms` if given.
fn resample(
    renderer: &mut GlesRenderer,
    texture: GlesTexture,
    to: Size<i32, Physical>,
    shader: Option<(&GlesTexProgram, [f32; 2])>,
) -> Option<GlesTexture> {
    let from = smithay::backend::renderer::Texture::size(&texture);
    let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
    // An explicit source, or the size override crops instead of scaling.
    let src = Rectangle::from_size(Size::<f64, Logical>::from((from.w as f64, from.h as f64)));
    let source = TextureRenderElement::from_texture_buffer(
        (0.0, 0.0),
        &buffer,
        Some(1.0),
        Some(src),
        Some(Size::from((to.w, to.h))),
        Kind::Unspecified,
    );
    match shader {
        Some((shader, step)) => {
            let element =
                TextureShaderElement::new(source, shader.clone(), vec![Uniform::new("step", step)]);
            draw(renderer, &[element], to, Scale::from(1.0))
        }
        None => draw(renderer, &[source], to, Scale::from(1.0)),
    }
}

/// A Gaussian pass each way over `texture` (`size` texels), `spread` apart.
fn blur(
    renderer: &mut GlesRenderer,
    shader: &GlesTexProgram,
    mut texture: GlesTexture,
    size: Size<i32, Physical>,
    spread: f32,
) -> Option<GlesTexture> {
    let texel = [1.0 / size.w as f32, 1.0 / size.h as f32];
    for step in [[texel[0] * spread, 0.0], [0.0, texel[1] * spread]] {
        texture = resample(renderer, texture, size, Some((shader, step)))?;
    }
    Some(texture)
}

fn shrink(full: Size<i32, Physical>, by: i32) -> Size<i32, Physical> {
    Size::from((((full.w + by - 1) / by).max(1), ((full.h + by - 1) / by).max(1)))
}

/// Compile the blur shader into `shader` unless it's there already (or
/// failed before): `what` says what goes without it.
fn compile(
    renderer: &mut GlesRenderer,
    shader: &mut Option<GlesTexProgram>,
    broken: &mut bool,
    what: &str,
) {
    if shader.is_some() || *broken {
        return;
    }
    match renderer.compile_custom_texture_shader(BLUR, &[UniformName::new("step", UniformType::_2f)])
    {
        Ok(program) => *shader = Some(program),
        Err(err) => {
            warn!(?err, "can't compile the blur shader; {what}");
            *broken = true;
        }
    }
}

impl SceneBlur {
    /// The GPU was reset: the shader and the held copies belonged to the
    /// lost context. The next switcher or dock preview blurs afresh.
    pub fn clear_gpu(&mut self) {
        self.shader = None;
        self.broken = false;
        self.held.clear();
    }

    /// Whether a blurred copy of `output` is held.
    pub fn has(&self, output: &Output) -> bool {
        self.find(output).is_some()
    }

    fn find(&self, output: &Output) -> Option<&Held> {
        let name = output.name();
        self.held.iter().find(|h| h.output == name)
    }

    /// Let go of the copy (the switcher has closed).
    pub fn release(&mut self) {
        self.held.clear();
    }

    /// Compile the shader now, not on first use (where it shows as a
    /// stutter).
    pub fn prepare(&mut self, renderer: &mut GlesRenderer) {
        compile(renderer, &mut self.shader, &mut self.broken, "the switcher only dims");
    }

    /// Draw `scene` (top first, as the renderer takes it), small, and blur it.
    pub fn capture(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        scene: &[OutputElement],
    ) {
        let started = std::time::Instant::now();
        self.prepare(renderer);
        let (Some(shader), Some(mode)) = (self.shader.clone(), output.current_mode()) else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        let full = output.current_transform().transform_size(mode.size);
        let mut size = shrink(full, 2);
        // Halved about the corner: a lower render scale alone would shrink
        // each window but leave it where it was (their places are physical).
        let halved: Vec<_> = scene
            .iter()
            .map(|e| RescaleRenderElement::from_element(e, Point::default(), 0.5))
            .collect();
        let Some(mut texture) = draw(renderer, &halved, size, Scale::from(scale)) else {
            warn!("can't draw the scene for the switcher's blur");
            return;
        };
        let mut by = 2;
        let result = (|| {
            while by < DEEP {
                by *= 2;
                size = shrink(full, by);
                texture = resample(renderer, texture.clone(), size, None)?;
            }
            for _ in 0..PASSES {
                texture = blur(renderer, &shader, texture.clone(), size, SPREAD)?;
            }
            while by > KEEP {
                by /= 2;
                size = shrink(full, by);
                texture = resample(renderer, texture.clone(), size, None)?;
            }
            // Smooth out the stretch's bilinear creases.
            blur(renderer, &shader, texture.clone(), size, 1.0)
        })();
        let Some(texture) = result else {
            warn!("can't blur the scene for the switcher");
            return;
        };
        let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
        let logical = full.to_f64().to_logical(scale).to_i32_round();
        let name = output.name();
        self.held.retain(|h| h.output != name);
        self.held.push(Held {
            output: name,
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
        let held = self.find(output)?;
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
        let held = self.find(output)?;
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

/// How much smaller than the output a live copy is drawn. It's sampled
/// straight down, so it can't be much smaller before text sparkles.
const LIVE_BY: i32 = 4;
/// Blur passes over a live copy, and how far apart their taps sit: wider
/// than the held copy's, since this one is bigger.
const LIVE_PASSES: usize = 2;
const LIVE_SPREAD: f32 = 2.5;

/// The size a live copy of an output of `full` physical pixels is drawn at.
fn live_size(full: Size<i32, Physical>) -> Size<i32, Physical> {
    shrink(full, LIVE_BY)
}

/// Whether the scene drawn into a slot has to be blurred again: the first
/// time, after its size changed, or when the damage tracker redrew any of it.
fn needs_blur(have: bool, resized: bool, redrawn: bool) -> bool {
    !have || resized || redrawn
}

/// One blurred copy of the scene below some frost.
struct Slot {
    /// The scene, drawn small, kept between frames so the damage tracker
    /// only redraws what changed in it.
    scene: GlesTexture,
    tracker: OutputDamageTracker,
    size: Size<i32, Physical>,
    blurred: Option<TextureBuffer<GlesTexture>>,
    /// Changes every time `blurred` does: frost elements put it in their
    /// commit counter, so the damage tracker redraws them.
    generation: usize,
    /// Asked for in this frame, so already up to date.
    fresh: bool,
}

/// Live blurred copies of the scene, per output and place (the shell's
/// frost, or one cnde window's), made when frost first asks for one in a
/// frame. Nothing is drawn while no frost asks.
#[derive(Default)]
pub struct LiveBlur {
    shader: Option<GlesTexProgram>,
    broken: bool,
    slots: HashMap<String, Slot>,
    /// Counts up over all slots, so a generation is never reused.
    generations: usize,
}

impl LiveBlur {
    /// The GPU was reset: the shader and the textures belonged to the lost
    /// context. They're made again on the next frame that wants them.
    pub fn clear_gpu(&mut self) {
        self.shader = None;
        self.broken = false;
        self.slots.clear();
    }

    /// The frame for `output` is built: let go of the copies nothing asked
    /// for (frost that's gone, or the setting turned off), and make the rest
    /// ask again next frame.
    pub fn finish(&mut self, output: &Output) {
        let prefix = format!("{}/", output.name());
        self.slots
            .retain(|key, slot| !key.starts_with(&prefix) || slot.fresh);
        for (key, slot) in &mut self.slots {
            if key.starts_with(&prefix) {
                slot.fresh = false;
            }
        }
    }

    /// Let go of every copy (the setting is off).
    pub fn release(&mut self) {
        self.slots.clear();
    }

    /// The blurred scene under `area` (logical, output-local) for `place`,
    /// where it sits in the texture (as `SceneBlur::crop` gives it), and
    /// the copy's generation. `below` is everything under the frost, top
    /// first. The copy is brought up to date the first time it's asked for
    /// in a frame.
    pub fn source(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        place: &str,
        below: &[&OutputElement],
        area: Rectangle<i32, Logical>,
        alpha: f32,
    ) -> Option<(TextureRenderElement<GlesTexture>, [f32; 4], usize)> {
        let key = format!("{}/{place}", output.name());
        let fresh = self.slots.get(&key).is_some_and(|slot| slot.fresh);
        if !fresh {
            crate::perf::time("frame.live_blur", || {
                self.update(renderer, output, &key, below)
            });
        }
        let slot = self.slots.get(&key)?;
        let buffer = slot.blurred.as_ref()?;
        let (element, crop) = crop_of(output, buffer, slot.size, area, alpha);
        Some((element, crop, slot.generation))
    }

    /// Draw `below` small into the slot for `key` and, if any of it
    /// changed, blur it afresh.
    fn update(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        key: &str,
        below: &[&OutputElement],
    ) {
        let started = std::time::Instant::now();
        compile(renderer, &mut self.shader, &mut self.broken, "frost stays on the wallpaper");
        let (Some(shader), Some(mode)) = (self.shader.clone(), output.current_mode()) else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        let full = output.current_transform().transform_size(mode.size);
        let size = live_size(full);
        let resized = self.slots.get(key).is_none_or(|slot| slot.size != size);
        if resized {
            let Ok(scene) = renderer.create_buffer(
                Fourcc::Abgr8888,
                Size::<i32, Buffer>::from((size.w, size.h)),
            ) else {
                warn!("can't make a texture for the live blur");
                self.slots.remove(key);
                return;
            };
            self.slots.insert(
                key.to_string(),
                Slot {
                    scene,
                    tracker: OutputDamageTracker::new(size, Scale::from(scale), Transform::Normal),
                    size,
                    blurred: None,
                    generation: 0,
                    fresh: false,
                },
            );
        }
        let Some(slot) = self.slots.get_mut(key) else {
            return;
        };
        slot.fresh = true;
        // Shrunk about the corner: a lower render scale alone would shrink
        // each element but leave it where it was (their places are physical).
        let small: Vec<_> = below
            .iter()
            .map(|e| {
                RescaleRenderElement::from_element(*e, Point::default(), 1.0 / LIVE_BY as f64)
            })
            .collect();
        // Age 1: the texture still holds the last frame, so only what
        // changed is drawn; no damage at all means nothing is.
        let redrawn = {
            let Ok(mut target) = renderer.bind(&mut slot.scene) else {
                return;
            };
            let age = if resized { 0 } else { 1 };
            match slot
                .tracker
                .render_output(renderer, &mut target, age, &small, [0.0, 0.0, 0.0, 1.0])
            {
                Ok(result) => result.damage.is_some_and(|d| !d.is_empty()),
                Err(_) => {
                    warn!("can't draw the scene for the live blur");
                    return;
                }
            }
        };
        if !needs_blur(slot.blurred.is_some(), resized, redrawn) {
            return;
        }
        let blurred = (|| {
            let mut texture = slot.scene.clone();
            for _ in 0..LIVE_PASSES {
                texture = blur(renderer, &shader, texture, size, LIVE_SPREAD)?;
            }
            // Smooth out the sampling's creases.
            blur(renderer, &shader, texture, size, 1.0)
        })();
        let Some(texture) = blurred else {
            warn!("can't blur the scene for the live blur");
            return;
        };
        self.generations += 1;
        slot.generation = self.generations;
        slot.blurred = Some(TextureBuffer::from_texture(
            renderer,
            texture,
            1,
            Transform::Normal,
            None,
        ));
        debug!(
            ms = started.elapsed().as_secs_f64() * 1000.0,
            width = size.w,
            height = size.h,
            key,
            "live blur captured (cpu side)"
        );
    }
}

/// The part of `buffer` (a blurred copy of the whole output) under `area`
/// (logical, output-local), and where it sits in the texture (x0, y0, x1,
/// y1).
fn crop_of(
    output: &Output,
    buffer: &TextureBuffer<GlesTexture>,
    texels: Size<i32, Physical>,
    area: Rectangle<i32, Logical>,
    alpha: f32,
) -> (TextureRenderElement<GlesTexture>, [f32; 4]) {
    let scale = output.current_scale().fractional_scale();
    let logical = output
        .current_mode()
        .map(|m| {
            output
                .current_transform()
                .transform_size(m.size)
                .to_f64()
                .to_logical(scale)
        })
        .unwrap_or_default();
    let (fx, fy) = (
        texels.w as f64 / logical.w.max(1.0),
        texels.h as f64 / logical.h.max(1.0),
    );
    let area = area.to_f64();
    let src = Rectangle::new(
        Point::from((area.loc.x * fx, area.loc.y * fy)),
        Size::from((area.size.w * fx, area.size.h * fy)),
    );
    let (w, h) = (texels.w as f64, texels.h as f64);
    let crop = [
        (src.loc.x / w) as f32,
        (src.loc.y / h) as f32,
        ((src.loc.x + src.size.w) / w) as f32,
        ((src.loc.y + src.size.h) / h) as f32,
    ];
    let element = TextureRenderElement::from_texture_buffer(
        area.loc.to_physical(scale),
        buffer,
        Some(alpha),
        Some(src),
        Some(area.size.to_i32_round()),
        Kind::Unspecified,
    );
    (element, crop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_copy_is_a_quarter_size_and_never_empty() {
        assert_eq!(live_size((1920, 1080).into()), (480, 270).into());
        assert_eq!(live_size((1366, 768).into()), (342, 192).into());
        assert_eq!(live_size((2, 1).into()), (1, 1).into());
    }

    #[test]
    fn blurs_again_only_when_something_changed() {
        assert!(needs_blur(false, false, false));
        assert!(needs_blur(true, true, false));
        assert!(needs_blur(true, false, true));
        assert!(!needs_blur(true, false, false));
    }
}
