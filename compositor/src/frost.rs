//! Frosted glass behind the dock (`dock.background = "frosted"`) and the
//! top bar (`panel.frosted`): the
//! blurred wallpaper, clipped to the dock's rounded shape, drawn just under
//! it. The dock itself paints only a light tint on top.
//!
//! It's the wallpaper that frosts, not the windows: cheap, and a dock on an
//! edge has no windows behind it anyway (they stop at its reserved space).
//! A popup or one of cnde's windows over another window would show the
//! wallpaper through it, so there the frost fades to a solid backing (a
//! tint of that window's colour with `appearance.match_windows`).

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, Uniform,
            element::{PixelShaderElement, TextureShaderElement},
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    backend::renderer::element::utils::RescaleRenderElement,
    desktop::{PopupKind, PopupManager, Space, Window, layer_map_for_output},
    reexports::wayland_server::Resource,
    output::Output,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Transform},
};
use tracing::warn;

use crate::{render::OutputElement, tint::Tints, wallpaper::Wallpapers};

/// How long a frost takes to turn solid, or back.
const FADE: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct DockFrost {
    shader: Option<GlesTexProgram>,
    /// Shader failed to compile: don't keep trying.
    broken: bool,
    /// Per output: one id for the frost, so damage tracking sees the same
    /// element from frame to frame and only redraws it when it moves.
    ids: HashMap<String, Id>,
    solid_shader: Option<GlesPixelProgram>,
    /// Solid backings, by the frost's key, with the colour they were made
    /// with (kept, like `ids`, for damage tracking).
    solids: HashMap<String, (PixelShaderElement, [f32; 4], u32)>,
    /// Whether each frost is covering a window, and since when.
    covered: HashMap<String, (bool, Instant)>,
}

/// What's behind the shell's popups: the windows on the output
/// (output-local, title bars included, bottom to top) and, when matching
/// them, their colours.
pub struct Behind<'a> {
    pub windows: Vec<(Window, Rectangle<i32, Logical>)>,
    pub tints: Option<&'a Tints>,
}

impl<'a> Behind<'a> {
    pub fn new(space: &Space<Window>, output: &Output, tints: Option<&'a Tints>) -> Self {
        let origin = space.output_geometry(output).map(|g| g.loc).unwrap_or_default();
        let windows = space
            .elements_for_output(output)
            .filter(|w| !crate::window::with_state(w, |s| s.minimized))
            .filter_map(|w| {
                let geo = space.element_geometry(w)?;
                let bar = crate::decorations::titlebar_height(w);
                let rect = Rectangle::new(
                    (geo.loc.x - origin.x, geo.loc.y - origin.y - bar).into(),
                    (geo.size.w, geo.size.h + bar).into(),
                );
                Some((w.clone(), rect))
            })
            .collect();
        Self { windows, tints }
    }

    /// The topmost window under `rect`.
    fn under(&self, rect: Rectangle<i32, Logical>) -> Option<&Window> {
        self.windows
            .iter()
            .rev()
            .find(|(_, r)| r.overlaps(rect))
            .map(|(w, _)| w)
    }
}

/// `window`'s colour for a backing, or `fallback`.
fn backing_colour(tints: Option<&Tints>, window: &Window, fallback: cnde_ui::Color) -> [f32; 3] {
    match tints.and_then(|t| t.get(window)) {
        Some(sample) => sample.main.map(|c| c as f32 / 255.0),
        None => [fallback.r, fallback.g, fallback.b],
    }
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
    #[allow(clippy::too_many_arguments)]
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        wallpapers: &Wallpapers,
        output: &Output,
        config: &cnde_config::Config,
        popups: &crate::popup_anim::PopupAnims,
        behind: &Behind<'_>,
        now: Instant,
    ) -> Vec<OutputElement> {
        let mut elements = if config.appearance.material == cnde_config::Material::Frosted {
            self.popup_elements(renderer, wallpapers, output, config, popups, behind, now)
        } else {
            Vec::new()
        };
        let dock = &config.dock;
        // As the dock draws itself (the palette sets the shape).
        let dock_radius_full = cnde_ui::Theme::for_appearance(&config.appearance).dock_radius as f64;
        let dock_radius = dock_radius_full;
        let mut wanted = Vec::new();
        if dock.background == cnde_config::DockBackground::Frosted {
            wanted.push((cnde_ipc::dock::NAMESPACE, dock_radius, None));
        }
        // A clear bar shows the wallpaper itself, not frost.
        if config.panel.frosted && !config.panel.clear {
            // Just the bar, not the shadow a rounded one casts below it.
            wanted.push(("cnde-panel", 0.0, Some(config.panel.height as i32)));
        }
        for (namespace, mut radius, height) in wanted {
            let layer = {
                let map = layer_map_for_output(output);
                map.layers()
                    .find(|l| l.namespace() == namespace)
                    .and_then(|l| {
                        let m = l.cached_state().margin;
                        let sides = match dock.position {
                            cnde_config::DockPosition::Left | cnde_config::DockPosition::Right => {
                                m.top + m.bottom
                            }
                            _ => m.left + m.right,
                        };
                        Some((map.layer_geometry(l)?, m.top, sides))
                    })
            };
            let Some((mut area, margin, sides)) = layer.filter(|(a, _, _)| !a.is_empty()) else {
                continue;
            };
            if namespace == cnde_ipc::dock::NAMESPACE
                && !matches!(
                    dock.position,
                    cnde_config::DockPosition::Overview | cnde_config::DockPosition::Off
                )
            {
                // An edge dock squares off as its side margins close.
                radius = dock_radius_full * cnde_ipc::dock::roundness(sides);
            }
            if namespace == "cnde-panel" && margin > 0 {
                // Floating: its corners grow with its margin (see
                // cnde_ipc::panel::GAP), as the shell draws them.
                let float = (margin as f64 / cnde_ipc::panel::GAP as f64).min(1.0);
                radius = cnde_ui::Theme::for_appearance(&config.appearance).panel_float_radius
                    as f64
                    * float;
            }
            if let Some(height) = height {
                area.size.h = area.size.h.min(height);
            }
            // A hidden (autohide) dock sits mostly off the screen: frost
            // only the part that shows, and nothing for a bare hot strip.
            if let Some(mode) = output.current_mode() {
                let scale = output.current_scale().fractional_scale();
                let size: smithay::utils::Size<i32, smithay::utils::Logical> = output
                    .current_transform()
                    .transform_size(mode.size)
                    .to_f64()
                    .to_logical(scale)
                    .to_i32_round();
                let screen = Rectangle::new((0, 0).into(), size);
                if namespace == cnde_ipc::dock::NAMESPACE && !screen.contains_rect(area) {
                    match area.intersection(screen) {
                        Some(shown) if shown.size.w >= 8 && shown.size.h >= 8 => area = shown,
                        _ => continue,
                    }
                }
            }
            if let Some(frost) =
                self.frost(renderer, wallpapers, output, namespace, area, radius, 1.0)
            {
                elements.push(OutputElement::Frost(frost));
            }
        }
        elements
    }

    /// Frost under the shell's popups (quick settings, the calendar,
    /// menus) on the top layer, opening and closing with them: the same
    /// scale from the middle of their top edge, the same fade.
    #[allow(clippy::too_many_arguments)]
    fn popup_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        wallpapers: &Wallpapers,
        output: &Output,
        config: &cnde_config::Config,
        popups: &crate::popup_anim::PopupAnims,
        behind: &Behind<'_>,
        now: Instant,
    ) -> Vec<OutputElement> {
        use crate::popup_anim::{open_progress, scale_at};
        // Never larger than any popup's own corners, so no frost shows
        // past them (a little missing in a corner doesn't show).
        let theme = cnde_ui::Theme::for_appearance(&config.appearance);
        let radius = theme.qs_radius.max(theme.menu_radius) as f64;
        let scale = output.current_scale().fractional_scale();
        let open = shell_popups(output).into_iter().map(|(surface, rect)| {
            let progress = open_progress(&surface, now);
            (format!("{:?}", surface.id()), rect, progress)
        });
        let Some(output_geo) = output_logical_geo(output) else {
            return Vec::new();
        };
        let closing = popups.closing(now).filter_map(|(rect, progress)| {
            let local = Rectangle::new(rect.loc - output_geo.loc, rect.size).to_i32_round();
            local
                .overlaps(Rectangle::from_size(output_geo.size.to_i32_round()))
                .then(|| (String::from("closing"), local, progress))
        });
        let all: Vec<_> = open.chain(closing).collect();
        let mut elements = Vec::new();
        let mut drawn = Vec::new();
        for (key, rect, progress) in all {
            if progress <= 0.0 {
                continue;
            }
            drawn.push(format!("{}/popup/{key}", output.name()));
            let origin = Point::<i32, Physical>::from((
                ((rect.loc.x as f64 + rect.size.w as f64 / 2.0) * scale).round() as i32,
                (rect.loc.y as f64 * scale).round() as i32,
            ));
            let under = behind.under(rect);
            let colour = under.map(|w| backing_colour(behind.tints, w, theme.qs_background));
            let (solid, solidity) = self.backing(
                renderer,
                output,
                &format!("popup/{key}"),
                rect,
                radius,
                colour,
                progress as f32,
                now,
            );
            if let Some(solid) = solid {
                elements.push(OutputElement::PopupSolid(RescaleRenderElement::from_element(
                    solid,
                    origin,
                    Scale::from(scale_at(progress)),
                )));
            }
            if solidity >= 1.0 {
                continue;
            }
            let Some(frost) = self.frost(
                renderer,
                wallpapers,
                output,
                &format!("popup/{key}"),
                rect,
                radius,
                progress as f32,
            ) else {
                continue;
            };
            elements.push(OutputElement::PopupFrost(RescaleRenderElement::from_element(
                frost,
                origin,
                Scale::from(scale_at(progress)),
            )));
        }
        // Forget popups that are gone, or their ids pile up.
        let prefix = format!("{}/popup/", output.name());
        self.ids
            .retain(|key, _| !key.starts_with(&prefix) || drawn.contains(key));
        self.solids
            .retain(|key, _| !key.starts_with(&prefix) || drawn.contains(key));
        self.covered
            .retain(|key, _| !key.starts_with(&prefix) || drawn.contains(key));
        elements
    }

    /// Whether a frost is still fading to solid or back.
    pub fn is_animating(&self, now: Instant) -> bool {
        self.covered.values().any(|(_, since)| now < *since + FADE)
    }

    /// The solid backing for the frost at `namespace` when it covers a
    /// window (`colour` is that backing's colour, `None` if it covers
    /// none), and how solid it is now (0: all frost, 1: all solid).
    #[allow(clippy::too_many_arguments)]
    fn backing(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        namespace: &str,
        area: Rectangle<i32, Logical>,
        radius: f64,
        colour: Option<[f32; 3]>,
        alpha: f32,
        now: Instant,
    ) -> (Option<PixelShaderElement>, f32) {
        let key = format!("{}/{namespace}", output.name());
        let covered = colour.is_some();
        let (was, since) = self.covered.entry(key.clone()).or_insert((covered, now - FADE));
        if *was != covered {
            // Turning back midway starts from where it got to.
            let done = (now - *since).min(FADE);
            *was = covered;
            *since = now - (FADE - done);
        }
        let t = ((now - *since).as_secs_f32() / FADE.as_secs_f32()).min(1.0);
        let solidity = if covered { t } else { 1.0 - t };
        // Keep the last colour while fading back out.
        let rgb = colour.or_else(|| {
            self.solids
                .get(&key)
                .map(|(_, c, _)| [c[0], c[1], c[2]].map(|v| v / c[3].max(1e-3)))
        });
        let (Some(rgb), true) = (rgb, solidity > 0.0) else {
            return (None, 0.0);
        };
        if self.solid_shader.is_none() {
            match renderer.compile_custom_pixel_shader(
                crate::shaders::ROUNDED_RECT,
                &crate::shaders::rounded_rect_uniforms(),
            ) {
                Ok(shader) => self.solid_shader = Some(shader),
                Err(err) => {
                    warn!(?err, "can't compile the backing shader");
                    return (None, 0.0);
                }
            }
        }
        let Some(shader) = self.solid_shader.clone() else {
            return (None, 0.0);
        };
        let scale = output.current_scale().fractional_scale();
        // Premultiplied, with the fade baked in (the element's own alpha
        // is fixed when it's made).
        let a = solidity * alpha;
        let premultiplied = [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
        let radius_px = (radius * scale).round() as u32;
        let uniforms = |c: [f32; 4]| {
            vec![
                Uniform::new("color", c),
                Uniform::new("border_color", c),
                Uniform::new("radius", (radius * scale) as f32),
                Uniform::new("border_width", 0.0f32),
            ]
        };
        let (element, shown, shown_radius) = self.solids.entry(key).or_insert_with(|| {
            (
                PixelShaderElement::new(
                    shader,
                    area,
                    None,
                    1.0,
                    uniforms(premultiplied),
                    Kind::Unspecified,
                ),
                premultiplied,
                radius_px,
            )
        });
        element.resize(area, None);
        if *shown != premultiplied || *shown_radius != radius_px {
            element.update_uniforms(uniforms(premultiplied));
            *shown = premultiplied;
            *shown_radius = radius_px;
        }
        (Some(element.clone()), solidity)
    }

    #[allow(clippy::too_many_arguments)]
    fn frost(
        &mut self,
        renderer: &mut GlesRenderer,
        wallpapers: &Wallpapers,
        output: &Output,
        namespace: &str,
        area: Rectangle<i32, smithay::utils::Logical>,
        radius: f64,
        alpha: f32,
    ) -> Option<Frost> {
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
        let (texture, crop) = wallpapers.blurred_crop(output, area, alpha)?;
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
        Some(Frost { inner, id })
    }
}

/// Frost under cnde's own windows (Settings), for `render::output_elements`.
pub struct WindowFrost<'a> {
    pub frost: &'a mut DockFrost,
    pub wallpapers: &'a Wallpapers,
    pub output: &'a Output,
    /// Their corners (the theme's window radius).
    pub radius: f64,
    /// Over another window: their sidebars' colour, or that window's.
    pub solid: cnde_ui::Color,
    pub tints: Option<&'a Tints>,
    pub now: Instant,
}

impl WindowFrost<'_> {
    /// The blurred wallpaper under `rect` (output-local), faded to
    /// `alpha`; over `under`, a solid backing instead. Top first.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        key: &str,
        rect: Rectangle<f64, Logical>,
        alpha: f32,
        under: Option<&Window>,
    ) -> Vec<OutputElement> {
        let namespace = format!("window/{key}");
        let rect = rect.to_i32_round();
        let colour = under.map(|w| backing_colour(self.tints, w, self.solid));
        let (solid, solidity) = self.frost.backing(
            renderer,
            self.output,
            &namespace,
            rect,
            self.radius,
            colour,
            alpha,
            self.now,
        );
        let mut elements: Vec<OutputElement> =
            solid.map(OutputElement::Shader).into_iter().collect();
        if solidity < 1.0
            && let Some(frost) = self.frost.frost(
                renderer,
                self.wallpapers,
                self.output,
                &namespace,
                rect,
                self.radius,
                alpha,
            )
        {
            elements.push(OutputElement::Frost(frost));
        }
        elements
    }

    /// Whether `window` gets frost: one of cnde's own, floating free
    /// (snapped, tiled, maximized or fullscreen it's solid; the app sees
    /// the same states and paints itself to match).
    pub fn wants(window: &smithay::desktop::Window) -> bool {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
        if !crate::capture::title_and_app_id(window).1.starts_with("cnde-") {
            return false;
        }
        window.toplevel().is_some_and(|toplevel| {
            let states = toplevel.current_state().states;
            ![
                State::Maximized,
                State::Fullscreen,
                State::TiledLeft,
                State::TiledRight,
                State::TiledTop,
                State::TiledBottom,
            ]
            .into_iter()
            .any(|s| states.contains(s))
        })
    }
}

/// The window frost the config asks for, if any.
pub fn window_frost<'a>(
    frost: &'a mut DockFrost,
    wallpapers: &'a Wallpapers,
    output: &'a Output,
    config: &cnde_config::Config,
    tints: Option<&'a Tints>,
    now: Instant,
) -> Option<WindowFrost<'a>> {
    let theme = cnde_ui::Theme::for_appearance(&config.appearance);
    (config.appearance.material == cnde_config::Material::Frosted).then_some(WindowFrost {
        frost,
        wallpapers,
        output,
        radius: theme.window_radius as f64,
        solid: theme.window_sidebar,
        tints,
        now,
    })
}

/// The shell's open popups on `output` (quick settings, the calendar,
/// menus), output-local.
pub fn shell_popups(
    output: &Output,
) -> Vec<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    Rectangle<i32, Logical>,
)> {
    let mut popups = Vec::new();
    let map = layer_map_for_output(output);
    for layer in map.layers_on(smithay::wayland::shell::wlr_layer::Layer::Top) {
        if !layer.namespace().starts_with("cnde-") {
            continue;
        }
        let Some(parent) = map.layer_geometry(layer) else {
            continue;
        };
        for (popup, offset) in PopupManager::popups_for_surface(layer.wl_surface()) {
            let PopupKind::Xdg(xdg) = &popup else {
                continue;
            };
            let rect = Rectangle::new(parent.loc + offset, popup.geometry().size);
            popups.push((xdg.wl_surface().clone(), rect));
        }
    }
    popups
}

/// `output`'s place and size, logical.
fn output_logical_geo(output: &Output) -> Option<Rectangle<f64, smithay::utils::Logical>> {
    let mode = output.current_mode()?;
    let size = output
        .current_transform()
        .transform_size(mode.size)
        .to_f64()
        .to_logical(output.current_scale().fractional_scale());
    Some(Rectangle::new(output.current_location().to_f64(), size))
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
