//! Frosted glass behind the dock (`dock.background = "frosted"`) and the
//! top bar (`panel.frosted`): the
//! blurred wallpaper, clipped to the dock's rounded shape, drawn just under
//! it. The dock itself paints only a light tint on top.
//!
//! It's the wallpaper that frosts, not the windows: cheap, and a dock on an
//! edge has no windows behind it anyway (they stop at its reserved space).

use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform,
            element::TextureShaderElement,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    desktop::layer_map_for_output,
    output::Output,
    utils::{Buffer, Physical, Point, Rectangle, Scale, Transform},
};
use tracing::warn;

use crate::{render::OutputElement, wallpaper::Wallpapers};

#[derive(Default)]
pub struct DockFrost {
    shader: Option<GlesTexProgram>,
    /// Shader failed to compile: don't keep trying.
    broken: bool,
    /// Per output: one id for the frost, so damage tracking sees the same
    /// element from frame to frame and only redraws it when it moves.
    ids: HashMap<String, Id>,
}

/// The frost element: smithay's texture-shader element under a stable id
/// (it makes a new id every time it's built).
pub struct Frost {
    inner: TextureShaderElement,
    id: Id,
}

impl DockFrost {
    /// The frost under the dock and the top bar on `output`, where the
    /// config asks for it.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        wallpapers: &Wallpapers,
        output: &Output,
        config: &cnde_config::Config,
    ) -> Vec<OutputElement> {
        let dock = &config.dock;
        let dock_radius = if dock.extend && dock.position != cnde_config::DockPosition::Overview {
            0.0
        } else {
            // As the dock draws itself (the palette sets the shape).
            cnde_ui::Theme::for_appearance(&config.appearance).dock_radius as f64
        };
        let mut wanted = Vec::new();
        if dock.background == cnde_config::DockBackground::Frosted {
            wanted.push((cnde_ipc::dock::NAMESPACE, dock_radius, None));
        }
        if config.panel.frosted {
            // Just the bar, not the shadow a rounded one casts below it.
            wanted.push(("cnde-panel", 0.0, Some(config.panel.height as i32)));
        }
        let mut elements = Vec::new();
        for (namespace, radius, height) in wanted {
            let area = {
                let map = layer_map_for_output(output);
                map.layers()
                    .find(|l| l.namespace() == namespace)
                    .and_then(|l| map.layer_geometry(l))
            };
            let Some(mut area) = area.filter(|a| !a.is_empty()) else {
                continue;
            };
            if let Some(height) = height {
                area.size.h = area.size.h.min(height);
            }
            if let Some(element) =
                self.element(renderer, wallpapers, output, namespace, area, radius)
            {
                elements.push(element);
            }
        }
        elements
    }

    fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        wallpapers: &Wallpapers,
        output: &Output,
        namespace: &str,
        area: Rectangle<i32, smithay::utils::Logical>,
        radius: f64,
    ) -> Option<OutputElement> {
        if self.shader.is_none() && !self.broken {
            match renderer.compile_custom_texture_shader(
                crate::shaders::ROUNDED_TEXTURE,
                &crate::shaders::rounded_texture_uniforms(),
            ) {
                Ok(shader) => self.shader = Some(shader),
                Err(err) => {
                    warn!(?err, "can't compile the frost shader; surfaces stay plain");
                    self.broken = true;
                }
            }
        }
        let shader = self.shader.as_ref()?;
        let scale = output.current_scale().fractional_scale();
        let (texture, crop) = wallpapers.blurred_crop(output, area)?;
        let inner = TextureShaderElement::new(
            texture,
            shader.clone(),
            vec![
                Uniform::new("crop", crop),
                Uniform::new(
                    "rect_size",
                    [
                        (area.size.w as f64 * scale) as f32,
                        (area.size.h as f64 * scale) as f32,
                    ],
                ),
                Uniform::new("radius", (radius * scale) as f32),
            ],
        );
        let key = format!("{}/{namespace}", output.name());
        let id = self.ids.entry(key).or_insert_with(Id::new).clone();
        Some(OutputElement::Frost(Frost { inner, id }))
    }
}

impl Element for Frost {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    /// Never opaque: the rounded corners let the wallpaper through.
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
}

impl RenderElement<GlesRenderer> for Frost {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        self.inner.draw(frame, src, dst, damage, opaque_regions)
    }
}
