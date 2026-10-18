//! Night light (`[night_light]`): warmer colours at night.
//!
//! The last thing drawn on each monitor is a full-screen quad that
//! multiplies every pixel by the colour of a warm light (blending
//! `dst * src` instead of the usual "over"), so the red channel stays and
//! green and blue are turned down. Doing it in the renderer rather than
//! with the monitor's gamma table works the same nested and on real
//! hardware, needs no DRM support, and keeps it out of screenshots and
//! screen sharing (it's applied after they're copied), like gamma would.
//! The hardware cursor isn't tinted.
//!
//! Turning on and off, or a new temperature, fades over a few seconds.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName,
            UniformType, element::PixelShaderElement, ffi,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};
use tracing::warn;

use crate::animation::{Animation, Curve, Params};

const FADE: Params = Params {
    duration: std::time::Duration::from_secs(3),
    curve: Curve::Linear,
};

/// Idle dimming: how much of full brightness is left once dimmed.
pub const DIMMED: f32 = 0.5;

/// Startup: how long to hold the screen black for the shell's panel before
/// fading in anyway, and how long that fade takes.
const INTRO_TIMEOUT: Duration = Duration::from_millis(1500);
const INTRO_FADE: Duration = Duration::from_millis(300);

/// Outputs the filter's colour; blending does the multiplying.
const SHADER: &str = r#"
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform vec3 filter_color;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    gl_FragColor = vec4(filter_color, 1.0);
}
"#;

/// No change.
const WHITE: [f32; 3] = [1.0, 1.0, 1.0];

/// The idle dimming (power.rs): a brightness factor multiplied into the
/// filter's colour, so dimming is the same full-screen quad as the night
/// light and is left out of screen captures the same way.
struct Dim {
    /// What it fades from.
    from: f32,
    target: f32,
    fade: Option<(Instant, Duration)>,
}

impl Default for Dim {
    fn default() -> Self {
        Self {
            from: 1.0,
            target: 1.0,
            fade: None,
        }
    }
}

impl Dim {
    fn current(&self, now: Instant) -> f32 {
        let Some((start, duration)) = self.fade else {
            return self.target;
        };
        let params = Params {
            duration,
            curve: Curve::Linear,
        };
        let t = Animation::progress_since(start, params, now) as f32;
        self.from + (self.target - self.from) * t
    }
}

/// The startup fade-in, a brightness factor like the idle dimming and
/// multiplied with it, so the two stay independent.
#[derive(Default, Clone, Copy)]
enum Intro {
    /// Black until the panel draws. Holds the time of the first frame.
    Hold(Option<Instant>),
    Fade(Instant),
    #[default]
    Done,
}

impl Intro {
    fn brightness(self, now: Instant) -> f32 {
        match self {
            Self::Hold(_) => 0.0,
            Self::Fade(start) => {
                let params = Params {
                    duration: INTRO_FADE,
                    curve: Curve::Linear,
                };
                Animation::progress_since(start, params, now) as f32
            }
            Self::Done => 1.0,
        }
    }
}

#[derive(Default)]
pub struct NightLight {
    dim: Dim,
    intro: Intro,
    /// The temperature it's on at, or `None` when off.
    target: Option<u32>,
    /// What's on screen: fading from `from` towards the target's colour.
    from: [f32; 3],
    fade: Option<(Instant, [f32; 3])>,
    shader: Option<GlesPixelProgram>,
    /// Shader failed to compile: don't keep trying.
    broken: bool,
    /// One element per monitor, kept so damage tracking sees the same one
    /// from frame to frame (a new uniform value damages it all).
    elements: HashMap<String, (PixelShaderElement, [f32; 3])>,
}

/// The filter over one monitor: a pixel-shader quad drawn with a
/// multiplying blend.
pub struct Filter(PixelShaderElement);

impl NightLight {
    /// Turn on at `temperature` kelvin, or off with `None`. Whether that's
    /// a change (needing frames to fade).
    pub fn set(&mut self, target: Option<u32>) -> bool {
        if self.target == target {
            return false;
        }
        let now = Instant::now();
        self.from = self.current(now);
        self.target = target;
        self.fade = Some((now, Self::color_of(target)));
        true
    }

    /// Fade to `brightness` (1.0 is no dimming) over `fade`; a zero `fade`
    /// jumps. Whether that's a change (needing frames).
    pub fn set_dim(&mut self, brightness: f32, fade: Duration) -> bool {
        if self.dim.target == brightness {
            return false;
        }
        let now = Instant::now();
        self.dim.from = self.dim.current(now);
        self.dim.target = brightness;
        self.dim.fade = (!fade.is_zero()).then_some((now, fade));
        true
    }

    /// Hold the screen black until `release_intro` (or a timeout after the
    /// first frame), then fade in. For compositor startup only.
    pub fn hold_intro(&mut self) {
        self.intro = Intro::Hold(None);
    }

    /// The shell has drawn (or isn't coming): fade in. Whether that's a
    /// change (needing frames).
    pub fn release_intro(&mut self) -> bool {
        if !matches!(self.intro, Intro::Hold(_)) {
            return false;
        }
        self.intro = Intro::Fade(Instant::now());
        true
    }

    /// Never hold the screen black over the lock screen.
    pub fn skip_intro(&mut self) {
        self.intro = Intro::Done;
    }

    /// Move the intro along: start the timeout on the first frame, and
    /// end it once faded.
    fn advance_intro(&mut self, now: Instant) {
        match &mut self.intro {
            Intro::Hold(first) => {
                let first = *first.get_or_insert(now);
                if now.saturating_duration_since(first) >= INTRO_TIMEOUT {
                    self.intro = Intro::Fade(now);
                }
            }
            Intro::Fade(start) => {
                if now.saturating_duration_since(*start) >= INTRO_FADE {
                    self.intro = Intro::Done;
                }
            }
            Intro::Done => {}
        }
    }

    fn color_of(target: Option<u32>) -> [f32; 3] {
        target.map_or(WHITE, |kelvin| {
            let (r, g, b) = color_temperature(kelvin as f64);
            [r as f32, g as f32, b as f32]
        })
    }

    /// The colour every pixel is multiplied by at `now`.
    fn current(&self, now: Instant) -> [f32; 3] {
        let dim = self.dim.current(now) * self.intro.brightness(now);
        let color = self.temperature_color(now);
        color.map(|c| c * dim)
    }

    fn temperature_color(&self, now: Instant) -> [f32; 3] {
        let Some((start, to)) = self.fade else {
            return Self::color_of(self.target);
        };
        let t = Animation::progress_since(start, FADE, now) as f32;
        std::array::from_fn(|i| self.from[i] + (to[i] - self.from[i]) * t)
    }

    pub fn is_animating(&mut self, now: Instant) -> bool {
        // Still holding or fading in: frames until it's done.
        let mut animating = !matches!(self.intro, Intro::Done);
        self.advance_intro(now);
        if let Some((start, _)) = self.fade {
            if Animation::progress_since(start, FADE, now) >= 1.0 {
                self.fade = None;
            }
            // Past the end, one more frame to draw the end of it.
            animating = true;
        }
        if let Some((start, duration)) = self.dim.fade {
            let params = Params {
                duration,
                curve: Curve::Linear,
            };
            if Animation::progress_since(start, params, now) >= 1.0 {
                self.dim.fade = None;
            }
            animating = true;
        }
        animating
    }

    /// The filter over `output`, top of everything (or `None` when off).
    pub fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        now: Instant,
    ) -> Option<Filter> {
        self.advance_intro(now);
        let color = self.current(now);
        if color == WHITE {
            self.elements.remove(&output.name());
            return None;
        }
        if self.shader.is_none() && !self.broken {
            match renderer.compile_custom_pixel_shader(
                SHADER,
                &[UniformName::new("filter_color", UniformType::_3f)],
            ) {
                Ok(shader) => self.shader = Some(shader),
                Err(err) => {
                    warn!(?err, "can't compile the night light shader; it stays off");
                    self.broken = true;
                }
            }
        }
        let shader = self.shader.as_ref()?;
        // The whole monitor, rounded up so fractional scales leave no edge.
        let mode = output.current_mode()?;
        let scale = output.current_scale().fractional_scale();
        let size = output.current_transform().transform_size(mode.size);
        let area = Rectangle::<i32, Logical>::from_size(Size::from((
            (size.w as f64 / scale).ceil() as i32,
            (size.h as f64 / scale).ceil() as i32,
        )));
        let (element, shown) = self.elements.entry(output.name()).or_insert_with(|| {
            (
                PixelShaderElement::new(
                    shader.clone(),
                    area,
                    None,
                    1.0,
                    vec![Uniform::new("filter_color", color)],
                    Kind::Unspecified,
                ),
                color,
            )
        });
        element.resize(area, None);
        if *shown != color {
            element.update_uniforms(vec![Uniform::new("filter_color", color)]);
            *shown = color;
        }
        Some(Filter(element.clone()))
    }

    /// A monitor is gone.
    pub fn forget(&mut self, output: &str) {
        self.elements.remove(output);
    }
}

/// The colour of a black body at `kelvin`, scaled so 6500 K (daylight,
/// the screen's own white point) is (1, 1, 1): what to multiply a white
/// pixel by to make it look lit by that light. Tanner Helland's fit to
/// the CIE 1964 blackbody colours, in sRGB.
pub fn color_temperature(kelvin: f64) -> (f64, f64, f64) {
    fn raw(kelvin: f64) -> (f64, f64, f64) {
        let t = kelvin.clamp(1000.0, 40000.0) / 100.0;
        let r = if t <= 66.0 {
            255.0
        } else {
            329.698727446 * (t - 60.0).powf(-0.1332047592)
        };
        let g = if t <= 66.0 {
            99.4708025861 * t.ln() - 161.1195681661
        } else {
            288.1221695283 * (t - 60.0).powf(-0.0755148492)
        };
        let b = if t >= 66.0 {
            255.0
        } else if t <= 19.0 {
            0.0
        } else {
            138.5177312231 * (t - 10.0).ln() - 305.0447927307
        };
        (r, g, b)
    }
    let (r, g, b) = raw(kelvin);
    let (wr, wg, wb) = raw(6500.0);
    (
        (r / wr).clamp(0.0, 1.0),
        (g / wg).clamp(0.0, 1.0),
        (b / wb).clamp(0.0, 1.0),
    )
}

impl Element for Filter {
    fn id(&self) -> &Id {
        self.0.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.0.current_commit()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(scale)
    }
    fn transform(&self) -> Transform {
        self.0.transform()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.0.src()
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.0.damage_since(scale, commit)
    }
    /// Never opaque: everything under it must still be drawn.
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        1.0
    }
    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.0.location(scale)
    }
}

impl RenderElement<GlesRenderer> for Filter {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        // result = framebuffer * filter colour. Smithay leaves blending on
        // with (ONE, ONE_MINUS_SRC_ALPHA) between draws; put that back.
        frame.with_context(|gl| unsafe {
            gl.Enable(ffi::BLEND);
            gl.BlendFunc(ffi::ZERO, ffi::SRC_COLOR);
        })?;
        let result = self.0.draw(frame, src, dst, damage, opaque_regions);
        frame.with_context(|gl| unsafe {
            gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
        })?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.02
    }

    #[test]
    fn daylight_is_no_change() {
        let (r, g, b) = color_temperature(6500.0);
        assert!(close(r, 1.0) && close(g, 1.0) && close(b, 1.0));
    }

    #[test]
    fn warmer_is_redder() {
        // Redshift's table, for comparison: 4000 K is about (1.0, 0.81, 0.65).
        let (r, g, b) = color_temperature(4000.0);
        assert!(close(r, 1.0));
        assert!(close(g, 0.81), "green {g}");
        assert!(close(b, 0.66), "blue {b}");
        // Lower temperatures take more blue, then more green, away.
        let mut last = color_temperature(6500.0);
        for kelvin in (1000..6500).rev().step_by(500) {
            let (r, g, b) = color_temperature(kelvin as f64);
            assert_eq!(r, 1.0);
            assert!(g < last.1 && b <= last.2, "{kelvin} K");
            assert!(b <= g, "{kelvin} K");
            last = (r, g, b);
        }
        // Candlelight: no blue left at all.
        assert_eq!(color_temperature(1800.0).2, 0.0);
    }

    #[test]
    fn values_stay_in_range() {
        for kelvin in [0.0, 1000.0, 2500.0, 6600.0, 10000.0, 1e9] {
            let (r, g, b) = color_temperature(kelvin);
            for c in [r, g, b] {
                assert!((0.0..=1.0).contains(&c), "{kelvin} K");
            }
        }
    }

    #[test]
    fn fades_between_settings() {
        crate::animation::set_speed(1.0);
        let mut light = NightLight::default();
        assert!(!light.set(None), "off is the start");
        assert!(light.set(Some(4000)));
        let (start, to) = light.fade.unwrap();
        assert_eq!(light.current(start), WHITE);
        let end = start + FADE.duration;
        assert_eq!(light.current(end), to);
        let mid = light.current(start + FADE.duration / 2);
        assert!(mid[2] < 1.0 && mid[2] > to[2]);
        assert!(!light.set(Some(4000)), "no change");
    }

    #[test]
    fn dims_and_undims() {
        let mut light = NightLight::default();
        assert!(
            !light.set_dim(1.0, Duration::from_secs(1)),
            "already bright"
        );
        assert!(light.set_dim(DIMMED, Duration::from_secs(1)));
        let (start, duration) = light.dim.fade.unwrap();
        assert_eq!(light.current(start), WHITE);
        let mid = light.current(start + duration / 2)[0];
        assert!(mid < 1.0 && mid > DIMMED);
        assert_eq!(light.current(start + duration), [DIMMED; 3]);
        // Waking jumps straight back.
        assert!(light.set_dim(1.0, Duration::ZERO));
        assert_eq!(light.current(Instant::now()), WHITE);
        assert!(!light.set_dim(1.0, Duration::ZERO));
    }

    #[test]
    fn intro_holds_then_fades() {
        let mut light = NightLight::default();
        assert!(!light.release_intro(), "not holding by default");
        light.hold_intro();
        let t0 = Instant::now();
        light.advance_intro(t0);
        assert_eq!(light.current(t0), [0.0; 3]);
        // Times out without the panel.
        light.advance_intro(t0 + INTRO_TIMEOUT);
        assert!(matches!(light.intro, Intro::Fade(_)));
        light.advance_intro(t0 + INTRO_TIMEOUT + INTRO_FADE);
        assert_eq!(light.current(Instant::now()), WHITE);
        // The panel drawing ends the hold early, and stacks with dimming.
        light.hold_intro();
        assert!(light.release_intro());
        assert!(!light.release_intro());
        light.skip_intro();
        light.set_dim(DIMMED, Duration::ZERO);
        assert_eq!(light.current(Instant::now()), [DIMMED; 3]);
    }
}
