//! Switching the light/dark style or the wallpaper fades from the old look
//! to the new one instead of snapping.
//!
//! When the config changes either, the next frame of each monitor is copied
//! to a texture and drawn on top of everything. It stays opaque while the
//! new wallpaper loads and the shell and apps redraw in the new style, then
//! fades out, uncovering the new look underneath.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{
            RenderElement,
            texture::{TextureBuffer, TextureRenderElement},
        },
        gles::{GlesRenderer, GlesTexture},
    },
    output::Output,
    utils::{Physical, Size, Transform},
};

use crate::animation::{Curve, Params};

/// How long the old look is held at least (the shell and apps redraw in
/// that time), and at most (a wallpaper that won't load).
const HOLD_MIN: Duration = Duration::from_millis(80);
const HOLD_MAX: Duration = Duration::from_millis(1500);
const FADE: Params = Params {
    duration: Duration::from_millis(350),
    curve: Curve::EaseOutCubic,
};

#[derive(Default)]
pub struct Crossfade {
    /// Monitors whose next frame is to be copied.
    pending: Vec<String>,
    fades: HashMap<String, Fade>,
}

struct Fade {
    texture: TextureBuffer<GlesTexture>,
    size: Size<i32, Physical>,
    scale: f64,
    captured: Instant,
    /// When fading out began (once the new look is ready).
    fading: Option<Instant>,
}

impl Crossfade {
    /// Fade every monitor from what it shows now.
    pub fn start<'a>(&mut self, outputs: impl Iterator<Item = &'a Output>) {
        if FADE.scaled_duration().is_zero() {
            return;
        }
        self.pending = outputs.map(|o| o.name()).collect();
    }

    pub fn wants_capture(&self, output: &Output) -> bool {
        self.pending.contains(&output.name())
    }

    /// Copy what `output` shows (`elements`, without the pointer).
    pub fn capture<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        elements: &[E],
    ) {
        let name = output.name();
        self.pending.retain(|n| n != &name);
        let Some(mode) = output.current_mode() else {
            return;
        };
        let scale = output.current_scale().fractional_scale();
        let size = output.current_transform().transform_size(mode.size);
        let Some(raw) =
            crate::screenshot::render_to_texture(renderer, elements, size, scale.into())
        else {
            return;
        };
        let texture = TextureBuffer::from_texture(renderer, raw, 1, Transform::Normal, None);
        self.fades.insert(
            name,
            Fade {
                texture,
                size,
                scale,
                captured: Instant::now(),
                fading: None,
            },
        );
    }

    /// The old look over `output`, if it's still fading. `ready`: the new
    /// wallpaper is on screen.
    pub fn element(
        &mut self,
        output: &Output,
        ready: bool,
        now: Instant,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        let name = output.name();
        let fade = self.fades.get_mut(&name)?;
        let held = now.saturating_duration_since(fade.captured);
        if fade.fading.is_none() && ((ready && held >= HOLD_MIN) || held >= HOLD_MAX) {
            fade.fading = Some(now);
            tracing::debug!(output = %name, ?held, ready, "style crossfade: fading");
        }
        let alpha = match fade.fading {
            None => 1.0,
            Some(start) => {
                let duration = FADE.scaled_duration();
                let t = now.saturating_duration_since(start).as_secs_f64()
                    / duration.as_secs_f64().max(1e-6);
                if t >= 1.0 {
                    self.fades.remove(&name);
                    return None;
                }
                1.0 - FADE.curve.apply(t)
            }
        };
        let logical = fade.size.to_f64().to_logical(fade.scale).to_i32_round();
        Some(crate::wallpaper::full_screen(
            &fade.texture,
            fade.size.w,
            fade.size.h,
            logical,
            Some(alpha as f32),
        ))
    }

    /// Whether frames are needed to keep it going.
    pub fn is_animating(&self) -> bool {
        !self.pending.is_empty() || !self.fades.is_empty()
    }

    /// A monitor is gone.
    pub fn forget(&mut self, output: &str) {
        self.pending.retain(|n| n != output);
        self.fades.remove(output);
    }
}
