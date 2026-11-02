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
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType, VrrSupport,
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
    desktop::layer_map_for_output,
    output::{Mode as WlMode, Output, PhysicalProperties, Scale as OutputScale},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Mode as DrmMode, ModeFlags, ModeTypeFlags, connector, crtc},
        input::{Device as InputDevice, Libinput},
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Point, Scale, Transform},
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use super::Backend;
use cnde_config::Vrr;

use crate::{cursor::Cursors, render, state::State};

/// 10-bit first for less banding, 8-bit as the universal fallback.
const COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];
const CLEAR_COLOR: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

/// Each queued frame carries the presentation feedback it answers.
type Feedback = Option<smithay::desktop::utils::OutputPresentationFeedback>;

type GbmOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Feedback,
    DrmDeviceFd,
>;
type GbmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, Feedback, DrmDeviceFd>;

pub struct UdevBackend {
    pub session: LibSeatSession,
    libinput: Libinput,
    /// The GPU we render with (its render node).
    primary_gpu: DrmNode,
    gpu: Option<Gpu>,
    pub dmabuf_state: DmabufState,
    dmabuf_global: Option<DmabufGlobal>,
    pub cursors: Cursors,
    /// Live libinput devices, so config changes can be re-applied to them.
    input_devices: Vec<InputDevice>,
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
    /// What the connector offers, and what it's running.
    modes: Vec<DrmMode>,
    mode: DrmMode,
    /// Whether the monitor can do variable refresh (FreeSync).
    vrr: VrrSupport,
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
            input_devices: Vec::new(),
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
        match &mut event {
            InputEvent::DeviceAdded { device } => {
                state.configure_input_device(device);
                if let Some(udev) = state.udev() {
                    udev.input_devices.push(device.clone());
                }
            }
            InputEvent::DeviceRemoved { device } => {
                if let Some(udev) = state.udev() {
                    udev.input_devices.retain(|d| d != device);
                }
            }
            _ => {}
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

/// DPMS off for one monitor: nothing scanned out, no frame pending.
fn blank(surface: &mut Surface) {
    if let Err(err) = surface.drm_output.with_compositor(|c| c.clear()) {
        warn!(name = %surface.output.name(), ?err, "can't turn the monitor off");
    }
    surface.redraw = RedrawState::Idle;
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

        // Screen captures may hand us dma-bufs to render into.
        let capture_dmabuf = crate::capture::DmabufInfo {
            device: render_node.dev_id(),
            formats: capture_formats(&renderer),
        };
        udev.gpu = Some(Gpu {
            node,
            renderer,
            outputs,
            scanner: DrmScanner::new(),
            surfaces: HashMap::new(),
            vblank_token,
        });
        self.shm_state.update_formats(shm_formats);
        self.capture.dmabuf = Some(capture_dmabuf);
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
        self.workspaces_after_output_change();
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
        let modes: Vec<DrmMode> = connector
            .modes()
            .iter()
            .filter(|m| !m.flags().contains(ModeFlags::INTERLACE))
            .copied()
            .collect();
        let config = self.config.output(&name).and_then(|c| c.mode);
        let Some(drm_mode) = pick_mode(&modes, config.as_ref()) else {
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

        // Parked off to the side; `apply_output_layout` below places it.
        // Every mode, so Settings can offer them.
        for mode in &modes {
            output.add_mode(WlMode::from(*mode));
        }
        if let Some(preferred) = pick_mode(&modes, None) {
            output.set_preferred(WlMode::from(preferred));
        }
        output.change_current_state(
            Some(wl_mode),
            Some(Transform::Normal),
            Some(OutputScale::Integer(1)),
            None,
        );
        self.space.map_output(&output, (i32::MAX / 2, 0));

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
        let vrr = drm_output
            .with_compositor(|c| c.vrr_supported(connector.handle()))
            .unwrap_or(VrrSupport::NotSupported);
        info!(
            %name,
            size = ?wl_mode.size,
            refresh_hz = wl_mode.refresh as f64 / 1000.0,
            ?vrr,
            "monitor connected"
        );
        gpu.surfaces.insert(
            crtc,
            Surface {
                output,
                drm_output,
                modes,
                mode: drm_mode,
                vrr,
                global,
                redraw: RedrawState::Idle,
                frame_callback_timer: None,
            },
        );
        self.apply_output_layout();
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
        // Panels on that monitor go away; clients like waybar re-create
        // theirs when a monitor comes back.
        for layer in layer_map_for_output(&surface.output).layers() {
            layer.layer_surface().send_close();
        }
        self.space.unmap_output(&surface.output);
        self.wallpapers.forget(&surface.output.name());
        self.crossfade.forget(&surface.output.name());
        self.night_light.forget(&surface.output.name());
        self.capture.output_removed(&surface.output);
        self.display_handle.remove_global::<State>(surface.global);
    }

    /// Re-apply mouse/touchpad settings to every connected device.
    pub fn reconfigure_input_devices(&mut self) {
        let devices = self
            .udev()
            .map(|u| u.input_devices.clone())
            .unwrap_or_default();
        for mut device in devices {
            self.configure_input_device(&mut device);
        }
    }

    /// Position and scale every monitor: configured ones where the config
    /// says, the rest left to right after them, top-aligned.
    pub fn apply_output_layout(&mut self) {
        if !matches!(self.backend, Backend::Udev(_)) {
            return;
        }
        self.apply_output_modes();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut placed: Vec<(Output, Point<i32, smithay::utils::Logical>, f64)> = Vec::new();
        let mut auto = Vec::new();
        for output in outputs {
            let config = self.config.output(&output.name());
            let scale = config.and_then(|c| c.scale).unwrap_or(1.0);
            match config.and_then(|c| c.position) {
                Some(pos) => placed.push((output, (pos.x, pos.y).into(), scale)),
                None => auto.push((output, scale)),
            }
        }
        let logical_width = |output: &Output, scale: f64| {
            output.current_mode().map_or(0, |mode| {
                mode.size.to_f64().to_logical(scale).to_i32_round::<i32>().w
            })
        };
        for (output, scale) in auto {
            let x = placed
                .iter()
                .map(|(o, pos, s)| pos.x + logical_width(o, *s))
                .max()
                .unwrap_or(0);
            placed.push((output, (x, 0).into(), scale));
        }
        for (output, position, scale) in placed {
            let output_scale = if scale.fract() == 0.0 {
                OutputScale::Integer(scale as i32)
            } else {
                OutputScale::Fractional(scale)
            };
            output.change_current_state(None, None, Some(output_scale), Some(position));
            self.space.map_output(&output, position);
        }
        self.refresh_fractional_scales();
        self.relayout_after_output_change();
        self.fixup_window_positions();
        self.workspaces_after_output_change();
        self.queue_pointer_refresh();
    }

    /// Switch monitors whose configured resolution or refresh changed.
    fn apply_output_modes(&mut self) {
        let config = self.config.clone();
        let Some(gpu) = self.udev().and_then(|u| u.gpu.as_mut()) else {
            return;
        };
        let mut changed = Vec::new();
        for (crtc, surface) in &mut gpu.surfaces {
            let name = surface.output.name();
            let wanted = config.output(&name).and_then(|c| c.mode);
            let Some(mode) = pick_mode(&surface.modes, wanted.as_ref()) else {
                continue;
            };
            if mode == surface.mode {
                continue;
            }
            if let Err(err) = surface.drm_output.use_mode(
                mode,
                &mut gpu.renderer,
                &DrmOutputRenderElements::<_, render::OutputElement>::default(),
            ) {
                warn!(%name, ?err, "monitor refused the new mode");
                continue;
            }
            let wl_mode = WlMode::from(mode);
            info!(
                %name,
                size = ?wl_mode.size,
                refresh_hz = wl_mode.refresh as f64 / 1000.0,
                "mode changed"
            );
            surface.mode = mode;
            surface
                .output
                .change_current_state(Some(wl_mode), None, None, None);
            changed.push(*crtc);
        }
        for crtc in changed {
            self.queue_output_redraw(crtc);
        }
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

    /// Ask just these monitors for a frame.
    pub fn udev_queue_redraw_outputs(&mut self, outputs: &[Output]) {
        let crtcs: Vec<crtc::Handle> = self
            .udev()
            .and_then(|u| u.gpu.as_ref())
            .map(|gpu| {
                gpu.surfaces
                    .iter()
                    .filter(|(_, s)| outputs.contains(&s.output))
                    .map(|(crtc, _)| *crtc)
                    .collect()
            })
            .unwrap_or_default();
        for crtc in crtcs {
            self.queue_output_redraw(crtc);
        }
    }

    /// Mark `crtc` as needing a frame. Rendering happens in
    /// [`udev_render_queued`](Self::udev_render_queued) at the end of the
    /// loop iteration, so several requests cost one frame.
    fn queue_output_redraw(&mut self, crtc: crtc::Handle) {
        let screens_off = self.idle.screens_off;
        let Some(udev) = self.udev() else {
            return;
        };
        // Switched to another VT: the GPU isn't ours until we're resumed
        // (which queues a full redraw).
        if !udev.session.is_active() {
            return;
        }
        // Monitors off for being idle: the first input queues a redraw.
        if screens_off {
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
            RedrawState::Idle => surface.redraw = RedrawState::Queued,
            RedrawState::Queued => {}
            RedrawState::WaitingForVblank { .. } => {
                surface.redraw = RedrawState::WaitingForVblank {
                    redraw_needed: true,
                };
            }
        }
    }

    /// Whether `output`'s monitor advertises variable refresh (its
    /// connector's `vrr_capable`); `None` off the udev backend.
    pub fn udev_vrr_capable(&mut self, output: &Output) -> Option<bool> {
        let gpu = self.udev()?.gpu.as_ref()?;
        let surface = gpu.surfaces.values().find(|s| s.output == *output)?;
        Some(surface.vrr != VrrSupport::NotSupported)
    }

    /// Idle: turn the monitors off (DPMS). Drawing stops until input; the
    /// next frame turns them back on. A monitor with a frame in flight is
    /// blanked when it lands.
    pub fn udev_screens_off(&mut self) {
        let Some(gpu) = self.udev().and_then(|u| u.gpu.as_mut()) else {
            return;
        };
        for surface in gpu.surfaces.values_mut() {
            if !matches!(surface.redraw, RedrawState::WaitingForVblank { .. }) {
                blank(surface);
            }
        }
    }

    /// Render every output that has a frame queued.
    pub fn udev_render_queued(&mut self) {
        let queued: Vec<crtc::Handle> = self
            .udev()
            .and_then(|u| u.gpu.as_ref())
            .map(|gpu| {
                gpu.surfaces
                    .iter()
                    .filter(|(_, s)| s.redraw == RedrawState::Queued)
                    .map(|(crtc, _)| *crtc)
                    .collect()
            })
            .unwrap_or_default();
        for crtc in queued {
            self.render_output(crtc);
        }
    }

    fn render_output(&mut self, crtc: crtc::Handle) {
        let screens_off = self.idle.screens_off;
        let now = std::time::Instant::now();
        let elapsed = self.start_time.elapsed();
        let pointer = self.seat.get_pointer().map(|p| p.current_location());
        let scene = match &self.backend {
            Backend::Udev(udev) => udev
                .gpu
                .as_ref()
                .and_then(|gpu| gpu.surfaces.get(&crtc))
                .and_then(|surface| self.scene(&surface.output, now)),
            Backend::Winit(_) => None,
        };
        let bar_tint = match &self.backend {
            Backend::Udev(udev) => udev
                .gpu
                .as_ref()
                .and_then(|gpu| gpu.surfaces.get(&crtc))
                .and_then(|surface| self.bar_tint(&surface.output)),
            Backend::Winit(_) => None,
        };
        let edge_hint = match &self.backend {
            Backend::Udev(udev) => udev
                .gpu
                .as_ref()
                .and_then(|gpu| gpu.surfaces.get(&crtc))
                .map(|surface| surface.output.clone()),
            Backend::Winit(_) => None,
        }
        .map(|output| self.edge_hint_elements(&output, now))
        .unwrap_or_default();
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
        if screens_off {
            return;
        }
        let output = surface.output.clone();

        // An app starting: the plain arrow shows it's busy meanwhile.
        if self
            .launching
            .is_some_and(|at| at.elapsed() > crate::state::LAUNCH_FEEDBACK)
        {
            self.launching = None;
        }
        let busy = self.launching.is_some()
            && matches!(
                self.cursor_status,
                smithay::input::pointer::CursorImageStatus::Named(
                    smithay::input::pointer::CursorIcon::Default
                )
            );
        // Dragging something wins over busy.
        let dragging = self.dnd_active;
        let busy = busy || dragging;
        let mut busy_status = smithay::input::pointer::CursorImageStatus::Named(if dragging {
            smithay::input::pointer::CursorIcon::Grabbing
        } else {
            smithay::input::pointer::CursorIcon::Progress
        });
        let mut elements = Vec::new();
        if let (Some(pointer), Some(output_geo)) = (pointer, self.space.output_geometry(&output)) {
            elements.extend(render::cursor_elements(
                &mut gpu.renderer,
                &mut udev.cursors,
                if busy {
                    &mut busy_status
                } else {
                    &mut self.cursor_status
                },
                pointer,
                output_geo,
                Scale::from(output.current_scale().fractional_scale()),
                elapsed,
            ));
            if !self.lock.is_locked()
                && let Some(icon) = &self.dnd_icon
            {
                elements.extend(render::dnd_icon_elements(
                    &mut gpu.renderer,
                    icon,
                    pointer,
                    output_geo,
                    Scale::from(output.current_scale().fractional_scale()),
                ));
            }
        }
        let cursor_count = elements.len();
        // A screenshot being framed: the frozen screen, the tool on top.
        let frozen = self.screenshot.frozen_element(&output);
        if self.lock.is_locked() {
            // Locked: the locker, and nothing else.
            elements.extend(self.lock.elements(&mut gpu.renderer, &output));
        } else if let (Some(frozen), Some(geo)) = (frozen, self.space.output_geometry(&output)) {
            elements.extend(render::overlay_elements(
                &mut gpu.renderer,
                &mut self.popup_anims,
                &output,
                geo.loc.to_f64(),
                now,
            ));
            elements.push(render::OutputElement::Wallpaper(frozen));
        } else {
            // First: it uploads a freshly loaded wallpaper, which the frost
            // is cut from. Otherwise the frame it arrives in has no frost,
            // and nothing redraws to add it.
            let wallpaper = self.wallpapers.element(&mut gpu.renderer, &output);
            let frost = self.dock_frost.elements(
                &mut gpu.renderer,
                &self.wallpapers,
                &output,
                &self.config,
                &self.popup_anims,
                &crate::frost::Behind::new(&self.space, &output, bar_tint),
                now,
            );
            let backdrop = match self.space.output_geometry(&output) {
                Some(geo) => crate::overview::backdrop(
                    &mut self.overview,
                    &self.wallpapers,
                    &mut gpu.renderer,
                    &output,
                    geo,
                    scene.as_ref(),
                ),
                None => Vec::new(),
            };
            let mut over_windows: Vec<_> = match (
                scene.as_ref().and_then(|s| s.close.as_ref()),
                self.space.output_geometry(&output),
            ) {
                (Some(close), Some(geo)) => self
                    .decorations
                    .close_button(
                        &mut gpu.renderer,
                        close.rect,
                        close.hovered,
                        geo.loc.to_f64(),
                        output.current_scale().fractional_scale().into(),
                    )
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            over_windows.extend(edge_hint);
            elements.extend(render::output_elements(
                &mut gpu.renderer,
                &self.space,
                &self.closing,
                self.snap_preview.as_mut(),
                scene.map(|s| s.windows),
                backdrop,
                frost,
                over_windows,
                &mut self.decorations,
                &mut self.popup_anims,
                crate::frost::window_frost(
                    &mut self.dock_frost,
                    &self.wallpapers,
                    &output,
                    &self.config,
                    now,
                ),
                &output,
                now,
            ));
            if let Some(wallpaper) = wallpaper {
                elements.push(render::OutputElement::Wallpaper(wallpaper));
            }
            // The window switcher goes over everything, under the pointer.
            let switcher = self.switcher.elements(
                &mut gpu.renderer,
                &mut self.decorations,
                &self.space,
                &output,
                &elements[cursor_count..],
                crate::frost::window_frost(
                    &mut self.dock_frost,
                    &self.wallpapers,
                    &output,
                    &self.config,
                    now,
                ),
                now,
            );
            for (i, element) in switcher.into_iter().enumerate() {
                elements.insert(cursor_count + i, element);
            }
            if self.screenshot.wants_freeze(&output) {
                self.screenshot
                    .freeze(&mut gpu.renderer, &output, &elements[cursor_count..]);
            }
        }

        // A style or wallpaper change: the old look, fading out on top
        // (under the pointer).
        if self.crossfade.wants_capture(&output) && !self.lock.is_locked() {
            self.crossfade
                .capture(&mut gpu.renderer, &output, &elements[cursor_count..]);
        }
        let ready = self.wallpapers.ready(&output);
        if let Some(fade) = self.crossfade.element(&output, ready, now)
            && !self.lock.is_locked()
        {
            elements.insert(cursor_count, render::OutputElement::Wallpaper(fade));
        }

        // The night light filter goes under the pointer but over
        // everything else, and is left out of screen captures.
        let night = self.night_light.element(&mut gpu.renderer, &output, now);
        let filtered = night.is_some();
        if let Some(night) = night {
            elements.insert(cursor_count, render::OutputElement::Night(night));
        }
        let wanted = match self.config.output(&output.name()).map(|c| c.vrr) {
            Some(Vrr::On) => true,
            Some(Vrr::Fullscreen) => {
                !self.lock.is_locked() && State::fullscreen_in(&self.space, &output)
            }
            Some(Vrr::Off) | None => false,
        };
        if surface.vrr != VrrSupport::NotSupported
            && surface.drm_output.with_compositor(|c| c.vrr_enabled()) != wanted
        {
            match surface.drm_output.with_compositor(|c| c.use_vrr(wanted)) {
                Ok(()) => info!(name = %output.name(), on = wanted, "variable refresh"),
                Err(err) => {
                    warn!(name = %output.name(), ?err, "monitor refused variable refresh");
                    surface.vrr = VrrSupport::NotSupported;
                }
            }
        }
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
        if filtered {
            elements.remove(cursor_count);
        }
        // Screen captures copy what this frame showed.
        crate::capture::capture_output(
            &mut self.capture,
            &mut gpu.renderer,
            &output,
            &elements,
            cursor_count,
            rendered,
        );

        if rendered {
            let feedback = State::presentation_feedback(&self.space, &output);
            match surface.drm_output.queue_frame(Some(feedback)) {
                Ok(()) => {
                    surface.redraw = RedrawState::WaitingForVblank {
                        redraw_needed: false,
                    };
                    if self.lock.is_locked() {
                        self.lock.frame_drawn();
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
                || self.night_light.is_animating(now)
                || self.launching.is_some(),
            now,
        ) {
            self.queue_redraw();
        }
    }

    fn on_vblank(&mut self, crtc: crtc::Handle, _metadata: &mut Option<DrmEventMetadata>) {
        let screens_off = self.idle.screens_off;
        let Some(surface) = self
            .udev()
            .and_then(|u| u.gpu.as_mut())
            .and_then(|gpu| gpu.surfaces.get_mut(&crtc))
        else {
            return;
        };
        match surface.drm_output.frame_submitted() {
            Ok(Some(Some(mut feedback))) => {
                use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
                let refresh = surface.output.current_mode().map_or(
                    smithay::wayland::presentation::Refresh::Unknown,
                    |mode| {
                        smithay::wayland::presentation::Refresh::fixed(
                            std::time::Duration::from_secs_f64(
                                1000.0 / mode.refresh.max(1) as f64,
                            ),
                        )
                    },
                );
                let (time, seq) = match _metadata {
                    Some(DrmEventMetadata {
                        time: smithay::backend::drm::DrmEventTime::Monotonic(time),
                        sequence,
                    }) => (smithay::utils::Time::from(*time), *sequence as u64),
                    _ => (smithay::utils::Clock::<smithay::utils::Monotonic>::new().now(), 0),
                };
                feedback.presented::<_, smithay::utils::Monotonic>(
                    time,
                    refresh,
                    seq,
                    Kind::Vsync | Kind::HwClock | Kind::HwCompletion,
                );
            }
            Ok(_) => {}
            Err(err) => warn!(?err, "frame_submitted failed"),
        }
        // The monitors went off while this frame was on its way.
        if screens_off {
            blank(surface);
            return;
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

/// The configured mode (the closest refresh at that size), else the
/// monitor's preferred resolution at the highest refresh rate it offers for
/// it (EDID "preferred" is often 60 Hz on high-refresh panels).
fn pick_mode(modes: &[DrmMode], config: Option<&cnde_config::ModeConfig>) -> Option<DrmMode> {
    let hz = |m: &DrmMode| WlMode::from(*m).refresh as f64 / 1000.0;
    if let Some(config) = config {
        let size = (config.width as u16, config.height as u16);
        let fits = modes.iter().filter(|m| m.size() == size);
        let picked = match config.refresh {
            Some(want) => fits.min_by(|a, b| (hz(a) - want).abs().total_cmp(&(hz(b) - want).abs())),
            None => fits.max_by(|a, b| hz(a).total_cmp(&hz(b))),
        };
        if let Some(mode) = picked {
            return Some(*mode);
        }
        warn!(?config, "monitor has no such mode; using its preferred one");
    }
    let preferred = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())?;
    modes
        .iter()
        .filter(|m| m.size() == preferred.size())
        .max_by(|a, b| hz(a).total_cmp(&hz(b)))
        .copied()
}

/// The 8-bit RGB formats (and modifiers) we can render capture frames into.
fn capture_formats(renderer: &GlesRenderer) -> Vec<(Fourcc, Vec<u64>)> {
    let wanted = [
        Fourcc::Argb8888,
        Fourcc::Xrgb8888,
        Fourcc::Abgr8888,
        Fourcc::Xbgr8888,
    ];
    let formats = renderer.egl_context().dmabuf_render_formats();
    wanted
        .iter()
        .filter_map(|&code| {
            let modifiers: Vec<u64> = formats
                .iter()
                .filter(|f| f.code == code)
                .map(|f| u64::from(f.modifier))
                .collect();
            (!modifiers.is_empty()).then_some((code, modifiers))
        })
        .collect()
}
