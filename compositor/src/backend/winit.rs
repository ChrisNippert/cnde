// Adapted from smithay: smallvil/src/winit.rs (dmabuf setup: anvil/src/winit.rs)

use std::time::Instant;

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        egl::EGLDevice,
        renderer::{ImportDma, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitEventLoop, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{calloop::LoopHandle, wayland_server::DisplayHandle},
    utils::Transform,
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
};
use tracing::{trace, warn};

use super::Backend;
use crate::{render, state::State};

const REFRESH_MHZ: i32 = 60_000;
const CLEAR_COLOR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

pub struct WinitBackend {
    graphics: WinitGraphicsBackend<GlesRenderer>,
    pub output: Output,
    damage_tracker: OutputDamageTracker,
    pub dmabuf_state: DmabufState,
    _dmabuf_global: DmabufGlobal,
    /// The named cursor last shown on the host window.
    cursor: smithay::input::pointer::CursorIcon,
}

pub fn init(
    dh: &DisplayHandle,
) -> Result<(WinitBackend, WinitEventLoop), Box<dyn std::error::Error>> {
    let (mut graphics, events) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: graphics.window_size(),
        refresh: REFRESH_MHZ,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "cnde".into(),
            model: "nested".into(),
        },
    );
    let _global = output.create_global::<State>(dh);
    // Why Flipped180: GL's origin is bottom-left; the winit backend expects this.
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(Scale::Fractional(nested_scale(graphics.scale_factor()))),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let damage_tracker = OutputDamageTracker::from_output(&output);
    let (dmabuf_state, dmabuf_global) = init_dmabuf(&mut graphics, dh);

    Ok((
        WinitBackend {
            graphics,
            output,
            damage_tracker,
            dmabuf_state,
            _dmabuf_global: dmabuf_global,
            cursor: smithay::input::pointer::CursorIcon::Default,
        },
        events,
    ))
}

/// linux-dmabuf lets GPU clients hand us buffers without copying. Without it
/// Mesa clients silently fall back to software rendering. v4 (with feedback)
/// needs the render node; v3 is the fallback.
fn init_dmabuf(
    graphics: &mut WinitGraphicsBackend<GlesRenderer>,
    dh: &DisplayHandle,
) -> (DmabufState, DmabufGlobal) {
    let formats = graphics.renderer().dmabuf_formats();
    let mut dmabuf_state = DmabufState::new();

    let render_node = EGLDevice::device_for_display(graphics.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());
    let feedback = match render_node {
        Ok(Some(node)) => DmabufFeedbackBuilder::new(node.dev_id(), formats.clone())
            .build()
            .map_err(|err| warn!(?err, "failed to build dmabuf feedback; using v3"))
            .ok(),
        Ok(None) => {
            warn!("no render node; dmabuf will use v3");
            None
        }
        Err(err) => {
            warn!(?err, "no EGL device; dmabuf will use v3");
            None
        }
    };

    let global = match feedback {
        Some(feedback) => dmabuf_state.create_global_with_default_feedback::<State>(dh, &feedback),
        None => dmabuf_state.create_global::<State>(dh, formats),
    };
    (dmabuf_state, global)
}

pub fn insert_event_source(
    handle: &LoopHandle<'static, State>,
    events: WinitEventLoop,
) -> Result<(), Box<dyn std::error::Error>> {
    handle
        .insert_source(events, |event, _, state| match event {
            WinitEvent::Resized { size, scale_factor } => {
                let Backend::Winit(winit) = &state.backend else {
                    return;
                };
                winit.output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: REFRESH_MHZ,
                    }),
                    None,
                    Some(Scale::Fractional(nested_scale(scale_factor))),
                    None,
                );
                state.relayout_after_output_change();
                state.queue_redraw();
                state.queue_pointer_refresh();
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => state.render_winit(),
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            WinitEvent::Focus(_) => {}
        })
        .map_err(|err| err.error)?;
    Ok(())
}

impl WinitBackend {
    pub fn request_redraw(&self) {
        self.graphics.window().request_redraw();
    }

    pub fn renderer(&mut self) -> &mut GlesRenderer {
        self.graphics.renderer()
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.graphics.renderer().import_dmabuf(dmabuf, None).is_ok()
    }
}

impl State {
    fn render_winit(&mut self) {
        let now = Instant::now();
        self.space.refresh();
        let scene = match &self.backend {
            Backend::Winit(winit) => self.scene(&winit.output, now),
            Backend::Udev(_) => None,
        };
        let output_geo = match &self.backend {
            Backend::Winit(winit) => self.space.output_geometry(&winit.output),
            Backend::Udev(_) => None,
        };
        let bar_tint = match &self.backend {
            Backend::Winit(winit) => self.bar_tint(&winit.output),
            Backend::Udev(_) => None,
        };
        let edge_hint = match &self.backend {
            Backend::Winit(winit) => Some(winit.output.clone()),
            Backend::Udev(_) => None,
        }
        .map(|output| self.edge_hint_elements(&output, now))
        .unwrap_or_default();

        // Why age 0 (full redraw): smithay 0.7's winit `buffer_age()` needs the EGL
        // surface current, which it often isn't between frames, so it fails with
        // EGL_BAD_SURFACE and floods the log (anvil hits this too). A full redraw of
        // the nested window is cheap; we still render only when something changed.
        // The udev backend gets ages from its DRM swapchain instead.
        let age = 0;
        let pointer = self.seat.get_pointer().map(|p| p.current_location());
        let Backend::Winit(backend) = &mut self.backend else {
            return;
        };
        // The host draws the pointer: show it the cursor clients name, so
        // resize arrows and the like can be seen nested too.
        let icon = match &self.cursor_status {
            _ if self.dnd_active => smithay::input::pointer::CursorIcon::Grabbing,
            smithay::input::pointer::CursorImageStatus::Named(icon) => *icon,
            _ => smithay::input::pointer::CursorIcon::Default,
        };
        if icon != backend.cursor {
            backend.cursor = icon;
            backend.graphics.window().set_cursor(icon);
        }
        let damage = {
            let (renderer, mut framebuffer) = match backend.graphics.bind() {
                Ok(bound) => bound,
                Err(err) => {
                    warn!(?err, "failed to bind winit framebuffer");
                    return;
                }
            };
            // First: it uploads a freshly loaded wallpaper, which the frost
            // is cut from (see udev.rs).
            let wallpaper = self.wallpapers.element(renderer, &backend.output);
            let wallpaper = wallpaper.map(render::OutputElement::Wallpaper);
            self.live_blur.set_frost(self.config.appearance.frost);
            let live = self.config.live_blur();
            let backdrop = match output_geo {
                Some(geo) => crate::overview::backdrop(
                    &mut self.overview,
                    &self.wallpapers,
                    renderer,
                    &backend.output,
                    geo,
                    scene.as_ref(),
                ),
                None => Vec::new(),
            };
            let mut over_windows: Vec<_> = match (scene.as_ref().and_then(|s| s.close.as_ref()), output_geo) {
                (Some(close), Some(geo)) => self
                    .decorations
                    .close_button(
                        renderer,
                        close.rect,
                        close.hovered,
                        geo.loc.to_f64(),
                        backend.output.current_scale().fractional_scale().into(),
                    )
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            over_windows.extend(edge_hint);
            let frozen = self.screenshot.frozen_element(&backend.output);
            let mut panels = 0;
            let locked = self.lock.is_locked();
            let mut elements = if locked {
                // Locked: the locker, and nothing else.
                self.lock.elements(renderer, &backend.output)
            } else if let (Some(frozen), Some(geo)) = (frozen, output_geo) {
                let mut elements = render::overlay_elements(
                    renderer,
                    &mut self.popup_anims,
                    &backend.output,
                    geo.loc.to_f64(),
                    now,
                );
                elements.push(render::OutputElement::Wallpaper(frozen));
                elements
            } else {
                let mut frost_at = None;
                let mut elements = render::output_elements(
                    renderer,
                    &self.space,
                    &self.closing,
                    self.snap_preview.as_mut(),
                    scene.map(|s| s.windows),
                    backdrop,
                    over_windows,
                    &mut self.decorations,
                    &mut self.popup_anims,
                    crate::frost::window_frost(
                        &mut self.dock_frost,
                        &self.wallpapers,
                        &backend.output,
                        &self.config,
                        now,
                        live.then_some(&mut self.live_blur),
                    ),
                    wallpaper.as_ref(),
                    &backend.output,
                    now,
                    &mut panels,
                    &mut frost_at,
                );
                // The frost goes in once what's below it is known (live
                // blur blurs that, not the wallpaper).
                if let Some(at) = frost_at {
                    let frost = {
                        let below: Vec<&render::OutputElement> =
                            elements[at..].iter().chain(&wallpaper).collect();
                        self.dock_frost.elements(
                            renderer,
                            &self.wallpapers,
                            &backend.output,
                            &self.config,
                            &self.popup_anims,
                            &crate::frost::Behind::new(&self.space, &backend.output, bar_tint),
                            now,
                            live.then(|| crate::frost::LiveSource {
                                blur: &mut self.live_blur,
                                below: &below,
                            }),
                        )
                    };
                    panels += frost.len();
                    elements.splice(at..at, frost);
                }
                if live {
                    self.live_blur.finish(&backend.output);
                } else {
                    self.live_blur.release();
                }
                elements
            };
            if self.screenshot.frozen_element(&backend.output).is_none()
                && !locked
                && let Some(wallpaper) = wallpaper
            {
                elements.push(wallpaper);
            }
            if !locked {
                // After the wallpaper: the switcher blurs the scene below it.
                let switcher = self.switcher.elements(
                    renderer,
                    &mut self.decorations,
                    &self.space,
                    &backend.output,
                    &elements,
                    crate::frost::window_frost(
                        &mut self.dock_frost,
                        &self.wallpapers,
                        &backend.output,
                        &self.config,
                        now,
                        None,
                    ),
                    now,
                );
                for (i, element) in switcher.under.into_iter().enumerate() {
                    elements.insert(panels + i, element);
                }
                for (i, element) in switcher.over.into_iter().enumerate() {
                    elements.insert(i, element);
                }
            }
            if self.screenshot.frozen_element(&backend.output).is_none()
                && !locked
                && self.screenshot.wants_freeze(&backend.output)
            {
                self.screenshot.freeze(renderer, &backend.output, &elements);
            }
            // A style or wallpaper change: the old look, fading out on top.
            if self.crossfade.wants_capture(&backend.output) && !locked {
                self.crossfade.capture(renderer, &backend.output, &elements);
            }
            let ready = self.wallpapers.ready(&backend.output);
            if let Some(fade) = self.crossfade.element(&backend.output, ready, now)
                && !locked
            {
                elements.insert(0, render::OutputElement::Wallpaper(fade));
            }
            // A drag's icon, over everything but the night light.
            if !locked
                && let (Some(icon), Some(pointer), Some(geo)) =
                    (&self.dnd_icon, pointer, output_geo)
            {
                let scale = backend.output.current_scale().fractional_scale().into();
                for (i, element) in render::dnd_icon_elements(renderer, icon, pointer, geo, scale)
                    .into_iter()
                    .enumerate()
                {
                    elements.insert(i, element);
                }
            }
            // The night light filter goes on top of everything but is
            // left out of screen captures.
            let night = self.night_light.element(renderer, &backend.output, now);
            let filtered = night.is_some();
            if let Some(night) = night {
                elements.insert(0, render::OutputElement::Night(night));
            }
            let damage = match backend.damage_tracker.render_output(
                renderer,
                &mut framebuffer,
                age,
                &elements,
                CLEAR_COLOR,
            ) {
                Ok(result) => result.damage.cloned(),
                Err(err) => {
                    warn!(?err, "rendering failed");
                    return;
                }
            };
            drop(framebuffer);
            if filtered {
                elements.remove(0);
            }
            crate::capture::capture_output(
                &mut self.capture,
                renderer,
                &backend.output,
                &elements,
                0,
                damage.is_some(),
            );
            damage
        };

        if let Some(damage) = damage {
            trace!(rects = damage.len(), "submitting frame");
            if let Err(err) = backend.graphics.submit(Some(&damage)) {
                warn!(?err, "failed to submit frame");
            }
        }

        let output = backend.output.clone();
        State::presentation_feedback(&self.space, &output).presented::<_, smithay::utils::Monotonic>(
            smithay::utils::Clock::<smithay::utils::Monotonic>::new().now(),
            smithay::wayland::presentation::Refresh::Unknown,
            0,
            smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::empty(),
        );
        if self.lock.is_locked() {
            self.lock.frame_drawn();
        }
        self.send_frame_callbacks(&output);

        self.closing.retain(|c| !c.is_done(now));

        // Keep frames coming while anything is animating; otherwise go idle.
        if render::animations_running(
            &self.space,
            &self.closing,
            self.snap_preview.as_ref(),
            self.workspaces.is_moving()
                || self.edge_hint.as_ref().is_some_and(|h| h.is_animating(now))
                || self.overview.is_animating(now)
                || self.switcher.is_animating(now)
                || self.popup_anims.is_animating(now)
                || self.dock_frost.is_animating(now)
                || self.crossfade.is_animating()
                || self.night_light.is_animating(now),
            now,
        ) {
            self.queue_redraw();
        }
    }
}

/// The host window's scale, or `CNDE_NESTED_SCALE` (debug aid: try e.g.
/// 1.25 without changing the host desktop).
fn nested_scale(host: f64) -> f64 {
    std::env::var("CNDE_NESTED_SCALE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(host)
}
