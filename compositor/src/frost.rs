//! Frosted glass behind the dock and the top bar (`appearance.material`,
//! or `dock.background` and `panel.frosted` for just one): the
//! blurred wallpaper, clipped to the dock's rounded shape, drawn just under
//! it. The dock itself paints only a light tint on top.
//!
//! It's the wallpaper that frosts, not the windows: cheap, and a dock on an
//! edge has no windows behind it anyway (they stop at its reserved space).
//! A popup or one of cnde's windows over another window would show the
//! wallpaper through it, so there the frost fades to a solid backing (in
//! the bar's colour, or the theme's).

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, texture::TextureRenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
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

use crate::{render::OutputElement, wallpaper::Wallpapers};

/// A blurred texture and where a frost's area sits in it (x0, y0, x1, y1).
type Cut = (TextureRenderElement<GlesTexture>, [f32; 4]);

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
    /// How solid each backing is heading (from, to, since when).
    covered: HashMap<String, (f32, f32, Instant)>,
    /// The shell's notification cards (and level display), per output
    /// name and layer namespace (see `Request::NotificationFrost`).
    pub cards: HashMap<(String, String), Vec<cnde_ipc::CardFrost>>,
}

/// What's behind the shell's popups: the windows on the output
/// (output-local, title bars included, bottom to top) and, with adaptive
/// colour, the bar's colour (popups hang from it and take it too).
pub struct Behind {
    pub windows: Vec<(Window, Rectangle<i32, Logical>)>,
    pub bar: Option<[u8; 3]>,
}

impl Behind {
    pub fn new(space: &Space<Window>, output: &Output, bar: Option<[u8; 3]>) -> Self {
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
        Self { windows, bar }
    }

    /// The topmost window under `rect`.
    pub fn under(&self, rect: Rectangle<i32, Logical>) -> Option<&Window> {
        self.windows
            .iter()
            .rev()
            .find(|(_, r)| r.overlaps(rect))
            .map(|(w, _)| w)
    }
}

/// The frost element: smithay's texture-shader element under a stable id
/// (it makes a new id every time it's built).
pub struct Frost {
    inner: TextureShaderElement,
    id: Id,
    /// For frost cut from a live blur: which copy of it (0: the wallpaper).
    /// A new copy under the same id is new content to the damage tracker.
    generation: usize,
}

/// What a live blur needs to make the frost on one output: the blur, and
/// everything below where the frost goes, top first.
pub struct LiveSource<'a> {
    pub blur: &'a mut crate::scene_blur::LiveBlur,
    pub below: &'a [&'a OutputElement],
}

impl LiveSource<'_> {
    /// The shell's frost shares one copy of the scene.
    fn shell(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        area: Rectangle<i32, Logical>,
        alpha: f32,
    ) -> Option<(TextureRenderElement<GlesTexture>, [f32; 4], usize)> {
        self.blur
            .source(renderer, output, "shell", self.below, area, alpha)
    }
}

impl DockFrost {
    /// The GPU was reset: both shaders and the backings built on them
    /// belonged to the lost context.
    pub fn clear_gpu(&mut self) {
        self.shader = None;
        self.solid_shader = None;
        self.broken = false;
        self.solids.clear();
    }

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
        behind: &Behind,
        now: Instant,
        // With live blur, what's below the frost to blur instead of the
        // wallpaper.
        mut live: Option<LiveSource<'_>>,
    ) -> Vec<OutputElement> {
        let mut elements = if config.appearance.material == cnde_config::Material::Frosted {
            self.popup_elements(
                renderer,
                wallpapers,
                output,
                config,
                popups,
                behind,
                now,
                live.as_mut(),
            )
        } else {
            Vec::new()
        };
        elements.extend(self.card_elements(
            renderer,
            wallpapers,
            output,
            config,
            behind,
            now,
            live.as_mut(),
        ));
        let dock = &config.dock;
        // As the dock draws itself (the palette sets the shape).
        let dock_radius_full = cnde_ui::Theme::for_appearance(&config.appearance).dock_radius as f64;
        let dock_radius = dock_radius_full;
        let mut wanted = Vec::new();
        if config.dock_frosted() {
            wanted.push((cnde_ipc::dock::NAMESPACE, dock_radius, None));
        }
        // Clear and island bars show the wallpaper, not a frosted strip.
        if config.panel_frosted() && config.panel.style() == cnde_config::PanelStyle::Full {
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
            if let Some(frost) = self.frost(
                renderer,
                wallpapers,
                output,
                namespace,
                area,
                radius,
                1.0,
                live.as_mut(),
            ) {
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
        behind: &Behind,
        now: Instant,
        mut live: Option<&mut LiveSource<'_>>,
    ) -> Vec<OutputElement> {
        use crate::popup_anim::{open_progress, scale_at};
        // Never larger than any popup's own corners, so no frost shows
        // past them (a little missing in a corner doesn't show).
        let theme = cnde_ui::Theme::for_screen(
            &config.appearance,
            screen_colour(config, wallpapers, output),
        );
        let popup = theme.qs_background;
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
            // Over a window: solid, in the bar's colour if it has one (not
            // with live blur: it shows what's really there).
            let target = behind.under(rect).filter(|_| live.is_none()).map(|_| {
                let colour = match behind.bar {
                    Some(bar) => bar.map(|c| c as f32 / 255.0),
                    None => [popup.r, popup.g, popup.b],
                };
                (colour, 1.0)
            });
            let (solid, solidity) = self.backing(
                renderer,
                output,
                &format!("popup/{key}"),
                rect,
                radius,
                target,
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
                live.as_deref_mut(),
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

    /// Frost under the shell's notification cards, and the bar's islands,
    /// wherever they are now.
    #[allow(clippy::too_many_arguments)]
    fn card_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        wallpapers: &Wallpapers,
        output: &Output,
        config: &cnde_config::Config,
        behind: &Behind,
        now: Instant,
        mut live: Option<&mut LiveSource<'_>>,
    ) -> Vec<OutputElement> {
        let prefix = format!("{}/note/", output.name());
        let mut elements = Vec::new();
        let mut drawn = Vec::new();
        let layers: Vec<_> = self
            .cards
            .iter()
            .filter(|((o, _), _)| *o == output.name())
            .map(|((_, layer), cards)| (layer.clone(), cards.clone()))
            .collect();
        let frosted = config.appearance.material == cnde_config::Material::Frosted;
        for (layer, cards) in layers {
            // The bar's islands are frosted whatever the material, when the
            // bar is; the notification cards only when it's frosted.
            if !frosted && layer != "cnde-panel" {
                continue;
            }
            let origin = {
                let map = layer_map_for_output(output);
                map.layers()
                    .find(|l| l.namespace() == layer)
                    .and_then(|l| map.layer_geometry(l))
                    .map(|g| g.loc)
            };
            let Some(origin) = origin else {
                continue;
            };
            let theme = cnde_ui::Theme::for_screen(
                &config.appearance,
                screen_colour(config, wallpapers, output),
            );
            let card_colour = theme.qs_background;
            let radius = theme.qs_radius as f64;
            let scale = output.current_scale().fractional_scale();
            for (i, card) in cards.iter().enumerate() {
                if card.alpha <= 0.0 {
                    continue;
                }
                let rect = Rectangle::<i32, Logical>::new(
                    (origin.x + card.rect[0].round() as i32, origin.y + card.rect[1].round() as i32)
                        .into(),
                    (card.rect[2].round() as i32, card.rect[3].round() as i32).into(),
                );
                let radius = card.radius.unwrap_or(radius);
                let namespace = format!("note/{layer}/{i}");
                drawn.push(format!("{}/{namespace}", output.name()));
                let at = Point::<i32, Physical>::from((
                    (rect.loc.x as f64 * scale).round() as i32,
                    (rect.loc.y as f64 * scale).round() as i32,
                ));
                // Over a window: solid, as the popups are.
                let target = behind.under(rect).filter(|_| live.is_none()).map(|_| {
                    let colour = match behind.bar {
                        Some(bar) => bar.map(|c| c as f32 / 255.0),
                        None => [card_colour.r, card_colour.g, card_colour.b],
                    };
                    (colour, 1.0)
                });
                let (solid, solidity) = self.backing(
                    renderer, output, &namespace, rect, radius, target, card.alpha, now,
                );
                if let Some(solid) = solid {
                    elements.push(OutputElement::PopupSolid(RescaleRenderElement::from_element(
                        solid,
                        at,
                        Scale::from(1.0),
                    )));
                }
                if solidity >= 1.0 {
                    continue;
                }
                if let Some(frost) = self.frost(
                    renderer,
                    wallpapers,
                    output,
                    &namespace,
                    rect,
                    radius,
                    card.alpha,
                    live.as_deref_mut(),
                ) {
                    elements.push(OutputElement::PopupFrost(RescaleRenderElement::from_element(
                        frost,
                        at,
                        Scale::from(1.0),
                    )));
                }
            }
        }
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
        self.covered.values().any(|(_, _, since)| now < *since + FADE)
    }

    /// The backing for the frost at `namespace`: `target` is its colour and
    /// how solid it should be (1 over a window, `None` for none), and the
    /// result how solid it is now (0: all frost, 1: all solid).
    #[allow(clippy::too_many_arguments)]
    fn backing(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        namespace: &str,
        area: Rectangle<i32, Logical>,
        radius: f64,
        target: Option<([f32; 3], f32)>,
        alpha: f32,
        now: Instant,
    ) -> (Option<PixelShaderElement>, f32) {
        let key = format!("{}/{namespace}", output.name());
        let (colour, level) = match target {
            Some((colour, level)) => (Some(colour), level),
            None => (None, 0.0),
        };
        let at = |(from, to, since): (f32, f32, Instant)| {
            let t = ((now - since).as_secs_f32() / FADE.as_secs_f32()).min(1.0);
            from + (to - from) * t
        };
        let fade = self.covered.entry(key.clone()).or_insert((level, level, now));
        if fade.1 != level {
            // Changing course midway starts from where it got to.
            *fade = (at(*fade), level, now);
        }
        let solidity = at(*fade);
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
        live: Option<&mut LiveSource<'_>>,
    ) -> Option<Frost> {
        if let Some((texture, crop, generation)) =
            live.and_then(|live| live.shell(renderer, output, area, alpha))
        {
            return self.frost_from(
                renderer,
                output,
                namespace,
                area,
                radius,
                (texture, crop),
                generation,
            );
        }
        let source = wallpapers.blurred_crop(output, area, alpha)?;
        self.frost_from(renderer, output, namespace, area, radius, source, 0)
    }

    /// Frost shaped from `source`, a blurred texture and where `area` sits
    /// in it (see `Wallpapers::blurred_crop`); `generation` is the live
    /// blur's, 0 for the wallpaper.
    #[allow(clippy::too_many_arguments)]
    fn frost_from(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        namespace: &str,
        area: Rectangle<i32, smithay::utils::Logical>,
        radius: f64,
        source: (TextureRenderElement<GlesTexture>, [f32; 4]),
        generation: usize,
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
        let (texture, crop) = source;
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
        Some(Frost {
            inner,
            id,
            generation,
        })
    }
}

/// Frost under cnde's own windows (Settings), for `render::output_elements`.
pub struct WindowFrost<'a> {
    pub frost: &'a mut DockFrost,
    pub wallpapers: &'a Wallpapers,
    pub output: &'a Output,
    /// Their corners (the theme's window radius).
    pub radius: f64,
    /// Over another window: their sidebars' colour.
    pub solid: cnde_ui::Color,
    /// What popups are tinted (the switcher's panel too), and their corners.
    pub panel: cnde_ui::Color,
    pub panel_radius: f64,
    pub now: Instant,
    /// With live blur: the blur, to cut the frost from what's really below.
    pub live: Option<&'a mut crate::scene_blur::LiveBlur>,
}

impl WindowFrost<'_> {
    /// The blurred wallpaper under `rect` (output-local), faded to
    /// `alpha`; over `under`, a solid backing instead. With live blur, the
    /// blurred scene `below` the window (top first) instead, and no backing.
    /// Top first.
    #[allow(clippy::too_many_arguments)]
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        key: &str,
        rect: Rectangle<f64, Logical>,
        alpha: f32,
        under: Option<&Window>,
        below: &[&OutputElement],
    ) -> Vec<OutputElement> {
        let live = self.live.as_deref_mut().and_then(|live| {
            live.source(
                renderer,
                self.output,
                &format!("window/{key}"),
                below,
                rect.to_i32_round(),
                alpha,
            )
        });
        match live {
            Some((texture, crop, generation)) => self.build(
                renderer,
                key,
                rect,
                self.radius,
                alpha,
                None,
                Some(((texture, crop), generation)),
            ),
            None => self.build(renderer, key, rect, self.radius, alpha, under, None),
        }
    }

    /// The same, with its corners at `radius`, and the frost cut from
    /// `source` (a blurred texture and where `rect` sits in it) instead of
    /// the wallpaper.
    #[allow(clippy::too_many_arguments)]
    pub fn elements_from(
        &mut self,
        renderer: &mut GlesRenderer,
        key: &str,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        alpha: f32,
        under: Option<&Window>,
        source: Option<(TextureRenderElement<GlesTexture>, [f32; 4])>,
    ) -> Vec<OutputElement> {
        self.build(
            renderer,
            key,
            rect,
            radius,
            alpha,
            under,
            source.map(|source| (source, 0)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &mut self,
        renderer: &mut GlesRenderer,
        key: &str,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        alpha: f32,
        under: Option<&Window>,
        source: Option<(Cut, usize)>,
    ) -> Vec<OutputElement> {
        let namespace = format!("window/{key}");
        let rect = rect.to_i32_round();
        let target = under.map(|_| ([self.solid.r, self.solid.g, self.solid.b], 1.0));
        let (solid, solidity) = self.frost.backing(
            renderer,
            self.output,
            &namespace,
            rect,
            radius,
            target,
            alpha,
            self.now,
        );
        let mut elements: Vec<OutputElement> =
            solid.map(OutputElement::Shader).into_iter().collect();
        let frost = if solidity >= 1.0 {
            None
        } else if let Some((source, generation)) = source {
            self.frost.frost_from(
                renderer,
                self.output,
                &namespace,
                rect,
                radius,
                source,
                generation,
            )
        } else {
            self.frost.frost(
                renderer,
                self.wallpapers,
                self.output,
                &namespace,
                rect,
                radius,
                alpha,
                None,
            )
        };
        if let Some(frost) = frost {
            elements.push(OutputElement::Frost(frost));
        }
        elements
    }

    /// Whether `window` gets frost: one of cnde's own, floating free
    /// (snapped, tiled, maximized or fullscreen it's solid; the app sees
    /// the same states and paints itself to match). Tiles are solid,
    /// told or not (`tiled`).
    pub fn wants(window: &smithay::desktop::Window) -> bool {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
        // The states it was sent say how it paints: a tile with
        // `[tiling] rounded` isn't told it's tiled, and stays see-through.
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
    now: Instant,
    live: Option<&'a mut crate::scene_blur::LiveBlur>,
) -> Option<WindowFrost<'a>> {
    let theme = cnde_ui::Theme::for_screen(
        &config.appearance,
        screen_colour(config, wallpapers, output),
    );
    (config.appearance.material == cnde_config::Material::Frosted).then_some(WindowFrost {
        frost,
        wallpapers,
        output,
        radius: theme.window_radius as f64,
        solid: theme.window_sidebar,
        panel: theme.qs_background.with_alpha(0.74),
        panel_radius: theme.qs_radius.max(theme.menu_radius) as f64,
        now,
        live: live.filter(|_| config.live_blur()),
    })
}

/// The colour the adaptive palette takes on `output`, as cnde's apps and
/// shell see it (the edges event): the wallpaper's.
fn screen_colour(
    config: &cnde_config::Config,
    wallpapers: &Wallpapers,
    output: &Output,
) -> Option<cnde_ui::Color> {
    if !config.appearance.adaptive() {
        return None;
    }
    let [r, g, b] = wallpapers.tint(&output.name())?;
    Some(cnde_ui::Color::rgb(r, g, b))
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
            // The client's window geometry, if it set one (cnde's shell
            // doesn't), else the size it was positioned at.
            let mut size = popup.geometry().size;
            if size.is_empty() {
                size = xdg.with_pending_state(|state| state.geometry.size);
            }
            let rect = Rectangle::new(parent.loc + offset, size);
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
        match self.generation {
            0 => self.inner.current_commit(),
            generation => CommitCounter::from(generation),
        }
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
        if self.generation == 0 {
            return self.inner.damage_since(scale, commit);
        }
        // A live copy is new content wholesale.
        if commit == Some(self.current_commit()) {
            DamageSet::default()
        } else {
            self.inner.damage_since(scale, None)
        }
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
