// Adapted from smithay: anvil/src/udev.rs (device and connector setup,
// session handling). Frame scheduling is our own: render on demand only.

//! Real hardware: libseat session, one GPU via DRM/GBM, an output per
//! connected monitor, and libinput for input devices.
//!
//! Single-GPU only for now. Hybrid laptops (render on one GPU, scan out on
//! another) need smithay's multi-GPU renderer and come later.

use std::{collections::HashMap, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            dmabuf::Dmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{ImportDma, ImportMemWl, gles::GlesRenderer},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend as UdevDevices, UdevEvent, all_gpus, primary_gpu},
    },
    output::{Mode as WlMode, Output, PhysicalProperties, Scale},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{ModeTypeFlags, connector, crtc},
        input::{DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Transform},
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use super::Backend;
use crate::{cursor::Cursors, render, state::State};

/// 10-bit first for less banding, 8-bit as the universal fallback.
const COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const CLEAR_COLOR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

type GbmOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;
type GbmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

pub struct UdevBackend {
    pub session: LibSeatSession,
    libinput: Libinput,
    /// The GPU we render with (its render node).
    primary_gpu: DrmNode,
    gpu: Option<Gpu>,
    pub dmabuf_state: DmabufState,
    dmabuf_global: Option<DmabufGlobal>,
    pub cursors: Cursors,
}

struct Gpu {
    /// Primary (KMS) node of the device; what udev events refer to.
    node: DrmNode,
    renderer: GlesRenderer,
    outputs: GbmOutputManager,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
    vblank_token: RegistrationToken,
}

/// One lit-up monitor.
struct Surface {
    output: Output,
    drm_output: GbmOutput,
    global: GlobalId,
    redraw: RedrawState,
    /// Pending throttled frame callbacks after an empty render.
    frame_callback_timer: Option<RegistrationToken>,
}

/// Per-output frame scheduling. We only render when something changed, and
/// never more than once per refresh: while a frame is on its way to the
/// screen, new requests just mark that another frame is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedrawState {
    Idle,
    /// A render is scheduled for this loop iteration.
    Queued,
    /// A frame was queued for scanout; render again after vblank if needed.
    WaitingForVblank {
        redraw_needed: bool,
    },
}

impl UdevBackend {
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        self.gpu.as_mut().map(|gpu| &mut gpu.renderer)
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.renderer()
            .is_some_and(|renderer| renderer.import_dmabuf(dmabuf, None).is_ok())
    }

    pub fn seat_name(&self) -> String {
        self.session.seat()
    }
}

/// Open the session and pick the GPU. Devices and outputs are set up by
/// [`init`] once `State` exists.
pub fn create() -> Result<
    (
        UdevBackend,
        smithay::backend::session::libseat::LibSeatSessionNotifier,
    ),
    Box<dyn std::error::Error>,
> {
    let (session, notifier) = LibSeatSession::new()
        .map_err(|err| format!("could not start a seat session (run from a TTY): {err}"))?;

    let primary_gpu = primary_gpu(session.seat())?
        .and_then(|path| DrmNode::from_path(path).ok())
        .or_else(|| {
            all_gpus(session.seat())
                .ok()?
                .into_iter()
                .find_map(|path| DrmNode::from_path(path).ok())
        })
        .ok_or("no GPU found")?;
    let primary_gpu = primary_gpu
        .node_with_type(NodeType::Render)
        .and_then(Result::ok)
        .unwrap_or(primary_gpu);
    info!(%primary_gpu, "using GPU");

    let libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());

    Ok((
        UdevBackend {
            session,
            libinput,
            primary_gpu,
            gpu: None,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            cursors: Cursors::load(),
        },
        notifier,
    ))
}

/// Open the GPU, light up monitors, and hook devices, input and session
/// events into the event loop.
pub fn init(
    state: &mut State,
    event_loop: &mut EventLoop<'static, State>,
    session_notifier: smithay::backend::session::libseat::LibSeatSessionNotifier,
) -> Result<(), Box<dyn std::error::Error>> {
    let handle = event_loop.handle();
    let Backend::Udev(udev) = &mut state.backend else {
        return Err("udev::init called without the udev backend".into());
    };
    let seat = udev.seat_name();

    // Input.
    udev.libinput
        .udev_assign_seat(&seat)
        .map_err(|()| "failed to assign libinput to the seat")?;
    let libinput_backend = LibinputInputBackend::new(udev.libinput.clone());
    handle.insert_source(libinput_backend, |mut event, _, state| {
        if let InputEvent::DeviceAdded { device } = &mut event {
            configure_input_device(device);
        }
        state.process_input_event(event);
    })?;

    // Session pause/resume (VT switches).
    handle.insert_source(session_notifier, |event, _, state| {
        state.on_session_event(event)
    })?;

    // GPUs and monitor hotplug.
    let devices = UdevDevices::new(&seat)?;
    for (device_id, path) in devices.device_list() {
        state.udev_device_added(device_id, path, &handle);
    }
    handle.insert_source(devices, |event, _, state| {
        let handle = state.loop_handle.clone();
        match event {
            UdevEvent::Added { device_id, path } => {
                state.udev_device_added(device_id, &path, &handle)
            }
            UdevEvent::Changed { device_id } => state.udev_device_changed(device_id),
            UdevEvent::Removed { device_id } => state.udev_device_removed(device_id),
        }
    })?;

    if let Backend::Udev(udev) = &state.backend
        && udev.gpu.is_none()
    {
        return Err("the primary GPU could not be opened".into());
    }
    Ok(())
}

/// GNOME-like touchpad defaults. Becomes configurable with the config crate.
fn configure_input_device(device: &mut smithay::reexports::input::Device) {
    if device.has_capability(DeviceCapability::Pointer) && device.config_tap_finger_count() > 0 {
        let _ = device.config_tap_set_enabled(true);
        let _ = device.config_tap_set_drag_enabled(true);
        let _ = device.config_dwt_set_enabled(true);
        if device.config_scroll_has_natural_scroll() {
            let _ = device.config_scroll_set_natural_scroll_enabled(true);
        }
    }
}

impl State {
    fn udev(&mut self) -> Option<&mut UdevBackend> {
        match &mut self.backend {
            Backend::Udev(udev) => Some(udev),
            Backend::Winit(_) => None,
        }
    }

    fn on_session_event(&mut self, event: SessionEvent) {
        let Some(udev) = self.udev() else {
            return;
        };
        match event {
            SessionEvent::PauseSession => {
                info!("session paused (switched away)");
                udev.libinput.suspend();
                if let Some(gpu) = &mut udev.gpu {
                    gpu.outputs.pause();
                    for surface in gpu.surfaces.values_mut() {
                        surface.redraw = RedrawState::Idle;
                    }
                }
            }
            SessionEvent::ActivateSession => {
                info!("session resumed");
                if let Err(err) = udev.libinput.resume() {
                    error!(?err, "failed to resume libinput");
                }
                if let Some(gpu) = &mut udev.gpu
                    && let Err(err) = gpu.outputs.activate(false)
                {
                    error!(?err, "failed to reactivate the GPU");
                }
                // Monitors may have changed while we were away.
                if let Some(node) = self.udev().and_then(|u| u.gpu.as_ref()).map(|g| g.node) {
                    self.udev_device_changed(node.dev_id());
                }
                self.queue_redraw();
            }
        }
    }

    fn udev_device_added(
        &mut self,
        device_id: u64,
        path: &Path,
        handle: &LoopHandle<'static, State>,
    ) {
        let Some(udev) = self.udev() else {
            return;
        };
        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            return;
        };
        let is_primary = node == udev.primary_gpu
            || node.node_with_type(NodeType::Render).and_then(Result::ok) == Some(udev.primary_gpu);
        if !is_primary || udev.gpu.is_some() {
            debug!(%node, "ignoring non-primary GPU (multi-GPU comes later)");
            return;
        }
        match self.open_gpu(node, path, handle) {
            Ok(()) => self.udev_device_changed(device_id),
            Err(err) => error!(%node, %err, "failed to open GPU"),
        }
    }

    fn open_gpu(
        &mut self,
        node: DrmNode,
        path: &Path,
        handle: &LoopHandle<'static, State>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dh = self.display_handle.clone();
        let Some(udev) = self.udev() else {
            return Ok(());
        };
        let fd = udev.session.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, drm_events) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        // Safety: the EGL display/context are only used from this thread.
        let display = unsafe { EGLDisplay::new(gbm.clone())? };
        let render_node = EGLDevice::device_for_display(&display)
            .ok()
            .and_then(|device| device.try_get_render_node().ok().flatten())
            .unwrap_or(node);
        let context = EGLContext::new_with_priority(&display, ContextPriority::High)?;
        // Safety: as above.
        let renderer = unsafe { GlesRenderer::new(context)? };

        let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
        let outputs = DrmOutputManager::new(
            drm,
            GbmAllocator::new(
                gbm.clone(),
                GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
            ),
            GbmFramebufferExporter::new(gbm.clone(), Some(render_node)),
            Some(gbm),
            COLOR_FORMATS.iter().copied(),
            render_formats,
        );

        let vblank_token =
            handle.insert_source(drm_events, move |event, metadata, state| match event {
                DrmEvent::VBlank(crtc) => state.on_vblank(crtc, metadata),
                DrmEvent::Error(err) => error!(?err, "DRM error"),
            })?;

        // Clients render on our GPU and hand us dmabufs.
        let feedback =
            DmabufFeedbackBuilder::new(render_node.dev_id(), renderer.dmabuf_formats()).build()?;
        let shm_formats = renderer.shm_formats();
        if udev.dmabuf_global.is_none() {
            udev.dmabuf_global = Some(
                udev.dmabuf_state
                    .create_global_with_default_feedback::<State>(&dh, &feedback),
            );
        }

        udev.gpu = Some(Gpu {
            node,
            renderer,
            outputs,
            scanner: DrmScanner::new(),
            surfaces: HashMap::new(),
            vblank_token,
        });
        self.shm_state.update_formats(shm_formats);
        info!(%node, %render_node, "GPU ready");
        Ok(())
    }

    /// Rescan connectors: light up new monitors, drop unplugged ones.
    fn udev_device_changed(&mut self, device_id: u64) {
        let Some(gpu) = self.udev().and_then(|u| u.gpu.as_mut()) else {
            return;
        };
        if gpu.node.dev_id() != device_id {
            return;
        }
        let events = match gpu.scanner.scan_connectors(gpu.outputs.device()) {
            Ok(result) => result.iter().collect::<Vec<_>>(),
            Err(err) => {
                warn!(?err, "failed to scan connectors");
                return;
            }
        };
        for event in events {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(connector, crtc),
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_disconnected(&connector, crtc),
                _ => {}
            }
        }
        self.fixup_window_positions();
    }

    fn udev_device_removed(&mut self, device_id: u64) {
        let Some(udev) = self.udev() else {
            return;
        };
        if udev
            .gpu
            .as_ref()
            .is_none_or(|gpu| gpu.node.dev_id() != device_id)
        {
            return;
        }
        let Some(gpu) = udev.gpu.take() else {
            return;
        };
        let dh = self.display_handle.clone();
        for surface in gpu.surfaces.into_values() {
            self.space.unmap_output(&surface.output);
            dh.remove_global::<State>(surface.global);
        }
        self.loop_handle.remove(gpu.vblank_token);
        warn!("GPU removed");
    }

    fn connector_connected(&mut self, connector: connector::Info, crtc: crtc::Handle) {
        let name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let Some(drm_mode) = pick_mode(&connector) else {
            warn!(%name, "connector has no modes");
            return;
        };
        let wl_mode = WlMode::from(drm_mode);
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                // TODO: make/model from EDID once libdisplay-info is sorted out.
                make: "Unknown".into(),
                model: name.clone(),
            },
        );
        let global = output.create_global::<State>(&self.display_handle);

        // Lay monitors out left to right, top-aligned (per-monitor arrangement
        // comes with the settings/config work).
        let x = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .map(|geo| geo.loc.x + geo.size.w)
            .max()
            .unwrap_or(0);
        output.set_preferred(wl_mode);
        output.change_current_state(
            Some(wl_mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((x, 0).into()),
        );
        self.space.map_output(&output, (x, 0));

        let Some(gpu) = self.udev().and_then(|u| u.gpu.as_mut()) else {
            return;
        };
        let planes = gpu.outputs.device().planes(&crtc).ok();
        let drm_output = match gpu.outputs.initialize_output::<_, render::OutputElement>(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            planes,
            &mut gpu.renderer,
            &DrmOutputRenderElements::default(),
        ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!(%name, ?err, "failed to light up monitor");
                self.space.unmap_output(&output);
                self.display_handle.remove_global::<State>(global);
                return;
            }
        };
        info!(
            %name,
            size = ?wl_mode.size,
            refresh_hz = wl_mode.refresh as f64 / 1000.0,
            "monitor connected"
        );
        gpu.surfaces.insert(
            crtc,
            Surface {
                output,
                drm_output,
                global,
                redraw: RedrawState::Idle,
                frame_callback_timer: None,
            },
        );
        self.queue_output_redraw(crtc);
    }

    fn connector_disconnected(&mut self, connector: &connector::Info, crtc: crtc::Handle) {
        let Some(gpu) = self.udev().and_then(|u| u.gpu.as_mut()) else {
            return;
        };
        let Some(surface) = gpu.surfaces.remove(&crtc) else {
            return;
        };
        if let Some(token) = surface.frame_callback_timer {
            self.loop_handle.remove(token);
        }
        info!(name = %surface.output.name(), connector = ?connector.handle(), "monitor disconnected");
        self.space.unmap_output(&surface.output);
        self.display_handle.remove_global::<State>(surface.global);
    }

    /// Pull windows that ended up on no monitor (after an unplug) back onto one.
    fn fixup_window_positions(&mut self) {
        let Some(first) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(target) = self.space.output_geometry(&first) else {
            return;
        };
        let stranded: Vec<_> = self
            .space
            .elements()
            .filter(|w| self.space.outputs_for_element(w).is_empty())
            .cloned()
            .collect();
        for window in stranded {
            self.space.map_element(window, target.loc, false);
        }
        self.queue_redraw();
    }

    /// Ask every monitor for a frame.
    pub fn udev_queue_redraw_all(&mut self) {
        let crtcs: Vec<crtc::Handle> = self
            .udev()
            .and_then(|u| u.gpu.as_ref())
            .map(|gpu| gpu.surfaces.keys().copied().collect())
            .unwrap_or_default();
        for crtc in crtcs {
            self.queue_output_redraw(crtc);
        }
    }

    fn queue_output_redraw(&mut self, crtc: crtc::Handle) {
        let handle = self.loop_handle.clone();
        let Some(udev) = self.udev() else {
            return;
        };
        // Switched to another VT: the GPU isn't ours until we're resumed
        // (which queues a full redraw).
        if !udev.session.is_active() {
            return;
        }
        let Some(surface) = udev
            .gpu
            .as_mut()
            .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
        else {
            return;
        };
        match surface.redraw {
            RedrawState::Idle => {
                surface.redraw = RedrawState::Queued;
                handle.insert_idle(move |state| state.render_output(crtc));
            }
            RedrawState::Queued => {}
            RedrawState::WaitingForVblank { .. } => {
                surface.redraw = RedrawState::WaitingForVblank {
                    redraw_needed: true,
                };
            }
        }
    }

    fn render_output(&mut self, crtc: crtc::Handle) {
        let now = std::time::Instant::now();
        let elapsed = self.start_time.elapsed();
        let pointer = self.seat.get_pointer().map(|p| p.current_location());
        let Backend::Udev(udev) = &mut self.backend else {
            return;
        };
        let Some(gpu) = &mut udev.gpu else {
            return;
        };
        let Some(surface) = gpu.surfaces.get_mut(&crtc) else {
            return;
        };
        if surface.redraw != RedrawState::Queued {
            return;
        }
        surface.redraw = RedrawState::Idle;
        let output = surface.output.clone();

        let mut elements = Vec::new();
        if let (Some(pointer), Some(output_geo)) = (pointer, self.space.output_geometry(&output)) {
            elements.extend(render::cursor_elements(
                &mut gpu.renderer,
                &mut udev.cursors,
                &mut self.cursor_status,
                pointer,
                output_geo,
                elapsed,
            ));
        }
        elements.extend(render::output_elements(
            &mut gpu.renderer,
            &self.space,
            &self.closing,
            self.snap_preview.as_mut(),
            &output,
            now,
        ));

        let rendered = match surface.drm_output.render_frame(
            &mut gpu.renderer,
            &elements,
            CLEAR_COLOR,
            FrameFlags::DEFAULT,
        ) {
            Ok(result) => !result.is_empty,
            Err(err) => {
                warn!(?err, "render failed");
                false
            }
        };

        if rendered {
            match surface.drm_output.queue_frame(()) {
                Ok(()) => {
                    surface.redraw = RedrawState::WaitingForVblank {
                        redraw_needed: false,
                    }
                }
                Err(err) => warn!(?err, "failed to queue frame"),
            }
            self.send_frame_callbacks(&output);
        } else {
            // Nothing changed on screen, but clients waiting on a frame
            // callback still need one, at most once per refresh, or a client
            // committing without damage would spin.
            self.schedule_frame_callbacks(crtc);
        }

        self.closing.retain(|c| !c.is_done(now));
        if render::animations_running(&self.space, &self.closing, self.snap_preview.as_ref(), now) {
            self.queue_redraw();
        }
    }

    fn on_vblank(&mut self, crtc: crtc::Handle, _metadata: &mut Option<DrmEventMetadata>) {
        let Some(surface) = self
            .udev()
            .and_then(|u| u.gpu.as_mut())
            .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
        else {
            return;
        };
        if let Err(err) = surface.drm_output.frame_submitted() {
            warn!(?err, "frame_submitted failed");
        }
        let redraw_needed = matches!(
            surface.redraw,
            RedrawState::WaitingForVblank {
                redraw_needed: true
            }
        );
        surface.redraw = RedrawState::Idle;
        if redraw_needed {
            self.queue_output_redraw(crtc);
        }
    }

    fn schedule_frame_callbacks(&mut self, crtc: crtc::Handle) {
        let handle = self.loop_handle.clone();
        let Some(surface) = self
            .udev()
            .and_then(|u| u.gpu.as_mut())
            .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
        else {
            return;
        };
        if surface.frame_callback_timer.is_some() {
            return;
        }
        let refresh_mhz = surface
            .output
            .current_mode()
            .map_or(60_000, |m| m.refresh)
            .max(1);
        let frame = Duration::from_secs_f64(1000.0 / refresh_mhz as f64);
        let token = handle.insert_source(Timer::from_duration(frame), move |_, _, state| {
            let output = state
                .udev()
                .and_then(|u| u.gpu.as_mut())
                .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
                .map(|surface| {
                    surface.frame_callback_timer = None;
                    surface.output.clone()
                });
            if let Some(output) = output {
                state.send_frame_callbacks(&output);
            }
            TimeoutAction::Drop
        });
        match token {
            Ok(token) => surface.frame_callback_timer = Some(token),
            Err(err) => warn!(?err, "failed to schedule frame callbacks"),
        }
    }

    pub fn switch_vt(&mut self, vt: i32) {
        if let Some(udev) = self.udev()
            && let Err(err) = udev.session.change_vt(vt)
        {
            warn!(vt, ?err, "failed to switch VT");
        }
    }
}

/// The monitor's preferred resolution at the highest refresh rate it offers
/// for that resolution (EDID "preferred" is often 60 Hz on high-refresh panels).
fn pick_mode(connector: &connector::Info) -> Option<smithay::reexports::drm::control::Mode> {
    let modes = connector.modes();
    let preferred = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())?;
    modes
        .iter()
        .filter(|m| m.size() == preferred.size())
        .max_by_key(|m| m.vrefresh())
        .copied()
}
