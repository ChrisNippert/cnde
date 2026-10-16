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
        Some(Scale::Fractional(graphics.scale_factor())),
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
                    Some(Scale::Fractional(scale_factor)),
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

        // Why age 0 (full redraw): smithay 0.7's winit `buffer_age()` needs the EGL
        // surface current, which it often isn't between frames, so it fails with
        // EGL_BAD_SURFACE and floods the log (anvil hits this too). A full redraw of
        // the nested window is cheap; we still render only when something changed.
        // The udev backend gets ages from its DRM swapchain instead.
        let age = 0;
        let Backend::Winit(backend) = &mut self.backend else {
            return;
        };
        let damage = {
            let (renderer, mut framebuffer) = match backend.graphics.bind() {
                Ok(bound) => bound,
                Err(err) => {
                    warn!(?err, "failed to bind winit framebuffer");
                    return;
                }
            };
            let backdrop = match output_geo {
                Some(geo) => crate::overview::backdrop(
                    &mut self.overview,
                    &mut self.wallpapers,
                    renderer,
                    &backend.output,
                    geo,
                    scene.as_ref(),
                ),
                None => Vec::new(),
            };
            let mut elements = render::output_elements(
                renderer,
                &self.space,
                &self.closing,
                self.snap_preview.as_mut(),
                scene.map(|s| s.windows),
                backdrop,
                &backend.output,
                now,
            );
            if let Some(wallpaper) = self.wallpapers.element(renderer, &backend.output) {
                elements.push(render::OutputElement::Wallpaper(wallpaper));
            }
            match backend.damage_tracker.render_output(
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
            }
        };

        if let Some(damage) = damage {
            trace!(rects = damage.len(), "submitting frame");
            if let Err(err) = backend.graphics.submit(Some(&damage)) {
                warn!(?err, "failed to submit frame");
            }
        }

        let output = backend.output.clone();
        self.send_frame_callbacks(&output);

        self.closing.retain(|c| !c.is_done(now));

        // Keep frames coming while anything is animating; otherwise go idle.
        if render::animations_running(
            &self.space,
            &self.closing,
            self.snap_preview.as_ref(),
            self.workspaces.is_moving() || self.overview.is_animating(now),
            now,
        ) {
            self.queue_redraw();
        }
    }
}
