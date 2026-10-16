//! Screen and window capture for screenshots and screen sharing
//! (ARCHITECTURE.md §5.2, Milestone 4): the standard protocols that
//! xdg-desktop-portal-wlr, OBS, grim and friends use.
//!
//! - `ext-foreign-toplevel-list-v1`: the list of windows a picker shows
//!   (smithay implements it; we keep it in sync with our windows).
//! - `ext-image-capture-source-v1`: "this monitor" or "this window" as
//!   something to capture.
//! - `ext-image-copy-capture-v1`: copy frames of a source into buffers the
//!   client provides, either shared memory or dma-bufs on our GPU.
//!
//! A monitor capture copies what that monitor just showed, right after it
//! renders, so a screen share costs nothing while the screen is still. A
//! window capture draws just that window, whenever it has drawn something
//! new, even while it's on a hidden workspace.

use std::{
    collections::HashSet,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::{Buffer as _, Fourcc},
        renderer::{
            Bind, ExportMem, Offscreen, TextureMapping,
            damage::OutputDamageTracker,
            element::{RenderElement, surface::WaylandSurfaceRenderElement},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::{Space, Window},
    output::{Output, WeakOutput},
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_protocols::ext::{
            foreign_toplevel_list::v1::server::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
            image_capture_source::v1::server::{
                ext_foreign_toplevel_image_capture_source_manager_v1::{
                    self, ExtForeignToplevelImageCaptureSourceManagerV1,
                },
                ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
                ext_output_image_capture_source_manager_v1::{
                    self, ExtOutputImageCaptureSourceManagerV1,
                },
            },
            image_copy_capture::v1::server::{
                ext_image_copy_capture_cursor_session_v1::{
                    self, ExtImageCopyCaptureCursorSessionV1,
                },
                ext_image_copy_capture_frame_v1::{
                    self, ExtImageCopyCaptureFrameV1, FailureReason,
                },
                ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1, Options},
                ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
            },
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::ObjectId,
            protocol::{wl_buffer::WlBuffer, wl_output::Transform as WlTransform, wl_shm},
        },
    },
    utils::{Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::{
        compositor::with_states,
        dmabuf::get_dmabuf,
        foreign_toplevel_list::{
            ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
            ForeignToplevelWeakHandle,
        },
        shell::xdg::XdgToplevelSurfaceData,
        shm::with_buffer_contents_mut,
    },
};
use tracing::{debug, warn};

use crate::state::State;

/// What a capture source refers to.
#[derive(Debug, Clone)]
enum Source {
    Output(WeakOutput),
    Toplevel(ForeignToplevelWeakHandle),
    /// The object it referred to was already gone.
    Gone,
}

pub struct SourceData(Source);

pub struct SessionData {
    source: Source,
    paint_cursors: bool,
    /// The first frame of a session is captured right away; later ones
    /// wait for the content to change.
    captured_once: AtomicBool,
    stopped: AtomicBool,
}

pub struct FrameData {
    session: ExtImageCopyCaptureSessionV1,
    buffer: Mutex<Option<WlBuffer>>,
    captured: AtomicBool,
    /// When the client asked for the capture.
    requested: Mutex<Option<Instant>>,
}

/// A capture waiting for new content gets the current frame anyway after
/// this long. Some clients (Discord) drop the first frame while setting up
/// their stream; on a still screen they'd otherwise never get another.
const REPEAT_AFTER: Duration = Duration::from_millis(250);

pub struct CursorSessionData {
    session_created: AtomicBool,
}

/// dma-buf capture: the render node clients must allocate on, and the
/// formats (with modifiers) we can render into.
#[derive(Debug, Clone)]
pub struct DmabufInfo {
    pub device: u64,
    pub formats: Vec<(Fourcc, Vec<u64>)>,
}

pub struct CaptureState {
    pub toplevel_list: ForeignToplevelListState,
    /// Every mapped window and its handle in the toplevel list.
    toplevels: Vec<(ForeignToplevelHandle, Window)>,
    sessions: Vec<ExtImageCopyCaptureSessionV1>,
    /// Frames the client asked to capture, waiting for new content.
    pending: Vec<ExtImageCopyCaptureFrameV1>,
    /// Root surfaces of windows that committed since their last capture.
    dirty: HashSet<ObjectId>,
    pub dmabuf: Option<DmabufInfo>,
}

impl CaptureState {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ExtOutputImageCaptureSourceManagerV1, _>(1, ());
        dh.create_global::<State, ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, ());
        dh.create_global::<State, ExtImageCopyCaptureManagerV1, _>(1, ());
        Self {
            toplevel_list: ForeignToplevelListState::new::<State>(dh),
            toplevels: Vec::new(),
            sessions: Vec::new(),
            pending: Vec::new(),
            dirty: HashSet::new(),
            dmabuf: None,
        }
    }

    /// A window appeared: list it for pickers.
    pub fn window_mapped(&mut self, window: &Window) {
        if self.toplevels.iter().any(|(_, w)| w == window) {
            return;
        }
        let (title, app_id) = title_and_app_id(window);
        let handle = self.toplevel_list.new_toplevel::<State>(title, app_id);
        handle.send_done();
        self.toplevels.push((handle, window.clone()));
    }

    /// A window went away: unlist it and end its captures.
    pub fn window_unmapped(&mut self, window: &Window) {
        let Some(index) = self.toplevels.iter().position(|(_, w)| w == window) else {
            return;
        };
        let (handle, _) = self.toplevels.remove(index);
        self.toplevel_list.remove_toplevel(&handle);
        self.stop_sessions(|source| match source {
            Source::Toplevel(weak) => weak
                .upgrade()
                .is_none_or(|h| h.identifier() == handle.identifier()),
            _ => false,
        });
    }

    /// A monitor went away: end its captures.
    pub fn output_removed(&mut self, output: &Output) {
        self.stop_sessions(|source| match source {
            Source::Output(weak) => weak.upgrade().is_none_or(|o| &o == output),
            _ => false,
        });
    }

    /// A surface committed: keep titles current, and note new window
    /// content for window captures.
    pub fn surface_committed(
        &mut self,
        root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some((handle, window)) = self
            .toplevels
            .iter()
            .find(|(_, w)| w.toplevel().is_some_and(|t| t.wl_surface() == root))
        else {
            return;
        };
        let (title, app_id) = title_and_app_id(window);
        let mut changed = false;
        if handle.title() != title {
            handle.send_title(&title);
            changed = true;
        }
        if handle.app_id() != app_id {
            handle.send_app_id(&app_id);
            changed = true;
        }
        if changed {
            handle.send_done();
        }
        if !self.pending.is_empty() {
            self.dirty.insert(root.id());
        }
    }

    fn stop_sessions(&mut self, matches: impl Fn(&Source) -> bool) {
        self.sessions.retain(|s| s.is_alive());
        for session in &self.sessions {
            let Some(data) = session.data::<SessionData>() else {
                continue;
            };
            if matches(&data.source) && !data.stopped.swap(true, Ordering::Relaxed) {
                session.stopped();
            }
        }
        // Frames waiting on a stopped session fail now.
        self.pending.retain(|frame| {
            let stopped = frame
                .data::<FrameData>()
                .and_then(|f| f.session.data::<SessionData>())
                .is_none_or(|s| s.stopped.load(Ordering::Relaxed));
            if stopped {
                frame.failed(FailureReason::Stopped);
            }
            !stopped
        });
    }

    fn window_for(&self, weak: &ForeignToplevelWeakHandle) -> Option<Window> {
        let handle = weak.upgrade()?;
        self.toplevels
            .iter()
            .find(|(h, _)| h.identifier() == handle.identifier())
            .map(|(_, w)| w.clone())
    }

    /// The buffer size clients must use for `source`.
    fn source_size(&self, source: &Source, space: &Space<Window>) -> Option<Size<i32, Physical>> {
        match source {
            Source::Output(weak) => output_size(&weak.upgrade()?),
            Source::Toplevel(weak) => {
                let window = self.window_for(weak)?;
                let scale = window_scale(&window, space);
                let size = window.geometry().size;
                (!size.is_empty()).then(|| size.to_f64().to_physical(scale).to_i32_round())
            }
            Source::Gone => None,
        }
    }

    /// Tell a session which buffers to use (or that it can't run).
    fn send_constraints(&self, session: &ExtImageCopyCaptureSessionV1, space: &Space<Window>) {
        let Some(data) = session.data::<SessionData>() else {
            return;
        };
        let Some(size) = self.source_size(&data.source, space) else {
            data.stopped.store(true, Ordering::Relaxed);
            session.stopped();
            return;
        };
        session.buffer_size(size.w as u32, size.h as u32);
        session.shm_format(wl_shm::Format::Argb8888);
        session.shm_format(wl_shm::Format::Xrgb8888);
        if let Some(dmabuf) = &self.dmabuf {
            session.dmabuf_device(dmabuf.device.to_ne_bytes().to_vec());
            for (format, modifiers) in &dmabuf.formats {
                let modifiers: Vec<u8> = modifiers.iter().flat_map(|m| m.to_ne_bytes()).collect();
                session.dmabuf_format(*format as u32, modifiers);
            }
        }
        session.done();
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

fn title_and_app_id(window: &Window) -> (String, String) {
    let Some(toplevel) = window.toplevel() else {
        return Default::default();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .map(|data| {
                (
                    data.title.clone().unwrap_or_default(),
                    data.app_id.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default()
    })
}

fn output_size(output: &Output) -> Option<Size<i32, Physical>> {
    let mode = output.current_mode()?;
    Some(output.current_transform().transform_size(mode.size))
}

/// The scale of the monitor a window is mostly on (1 if none).
fn window_scale(window: &Window, space: &Space<Window>) -> f64 {
    space
        .outputs_for_element(window)
        .first()
        .map_or(1.0, |o| o.current_scale().fractional_scale())
}

/// Monitor captures, right after `output` rendered `elements` (top first;
/// the first `cursor_count` are the pointer). `changed`: the frame differed
/// from the last one, so waiting captures can complete.
pub fn capture_output<E: RenderElement<GlesRenderer>>(
    capture: &mut CaptureState,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[E],
    cursor_count: usize,
    changed: bool,
) {
    if capture.pending.is_empty() {
        return;
    }
    let Some(size) = output_size(output) else {
        return;
    };
    let scale = Scale::from(output.current_scale().fractional_scale());
    let frames: Vec<ExtImageCopyCaptureFrameV1> = capture
        .pending
        .iter()
        .filter(|frame| {
            let Some(session) = session_of(frame) else {
                return false;
            };
            let first = !session.captured_once.load(Ordering::Relaxed);
            let stale = frame
                .data::<FrameData>()
                .and_then(|f| *f.requested.lock().ok()?)
                .is_some_and(|at| at.elapsed() >= REPEAT_AFTER);
            matches!(&session.source, Source::Output(weak) if weak.upgrade().is_some_and(|o| &o == output))
                && (changed || first || stale)
        })
        .cloned()
        .collect();
    for frame in frames {
        capture.pending.retain(|f| f != &frame);
        let paint_cursors = session_of(&frame).is_some_and(|s| s.paint_cursors);
        let elements = if paint_cursors {
            elements
        } else {
            &elements[cursor_count.min(elements.len())..]
        };
        copy_into(renderer, &frame, elements, size, scale);
    }
}

/// Window captures whose window drew something new (or that haven't had
/// a first frame yet).
pub fn capture_toplevels(
    capture: &mut CaptureState,
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
) {
    if capture.pending.is_empty() {
        return;
    }
    let frames: Vec<(ExtImageCopyCaptureFrameV1, Window)> = capture
        .pending
        .iter()
        .filter_map(|frame| {
            let session = session_of(frame)?;
            let Source::Toplevel(weak) = &session.source else {
                return None;
            };
            let window = capture.window_for(weak)?;
            let root = window.toplevel()?.wl_surface().id();
            let first = !session.captured_once.load(Ordering::Relaxed);
            let stale = frame
                .data::<FrameData>()
                .and_then(|f| *f.requested.lock().ok()?)
                .is_some_and(|at| at.elapsed() >= REPEAT_AFTER);
            (first || stale || capture.dirty.contains(&root)).then(|| (frame.clone(), window))
        })
        .collect();
    for (frame, window) in frames {
        capture.pending.retain(|f| f != &frame);
        if let Some(toplevel) = window.toplevel() {
            capture.dirty.remove(&toplevel.wl_surface().id());
        }
        let scale = Scale::from(window_scale(&window, space));
        let geometry = window.geometry();
        let size = geometry.size.to_f64().to_physical(scale).to_i32_round();
        // The window geometry's top-left at the buffer's origin; client-side
        // shadows outside it are cut off, as other compositors do.
        let location: Point<i32, Physical> =
            (Point::default() - geometry.loc).to_physical_precise_round(scale);
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            smithay::backend::renderer::element::AsRenderElements::render_elements(
                &window, renderer, location, scale, 1.0,
            );
        copy_into(renderer, &frame, &elements, size, scale);
    }
}

fn session_of(frame: &ExtImageCopyCaptureFrameV1) -> Option<&SessionData> {
    frame.data::<FrameData>()?.session.data::<SessionData>()
}

/// Render `elements` into the frame's buffer and complete it.
fn copy_into<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    frame: &ExtImageCopyCaptureFrameV1,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) {
    let Some(data) = frame.data::<FrameData>() else {
        return;
    };
    let Some(buffer) = data.buffer.lock().ok().and_then(|b| b.clone()) else {
        frame.failed(FailureReason::Unknown);
        return;
    };
    let result = if let Ok(dmabuf) = get_dmabuf(&buffer) {
        copy_to_dmabuf(renderer, dmabuf.clone(), elements, size, scale)
    } else {
        copy_to_shm(renderer, &buffer, elements, size, scale)
    };
    match result {
        Ok(()) => {
            if let Some(session) = data.session.data::<SessionData>() {
                session.captured_once.store(true, Ordering::Relaxed);
            }
            frame.transform(WlTransform::Normal);
            frame.damage(0, 0, size.w, size.h);
            let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
            let secs = now.tv_sec as u64;
            frame.presentation_time((secs >> 32) as u32, secs as u32, now.tv_nsec as u32);
            frame.ready();
        }
        Err(reason) => {
            debug!(?reason, "capture failed");
            frame.failed(reason);
        }
    }
}

fn copy_to_dmabuf<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    mut dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> Result<(), FailureReason> {
    if dmabuf.width() as i32 != size.w || dmabuf.height() as i32 != size.h {
        return Err(FailureReason::BufferConstraints);
    }
    let mut target = renderer
        .bind(&mut dmabuf)
        .map_err(|err| warn!(?err, "can't render into capture dma-buf"))
        .map_err(|_| FailureReason::Unknown)?;
    let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    let result = tracker
        .render_output(renderer, &mut target, 0, elements, [0.0, 0.0, 0.0, 1.0])
        .map_err(|err| warn!(?err, "capture render failed"))
        .map_err(|_| FailureReason::Unknown)?;
    // The client reads the buffer as soon as it hears "ready".
    let _ = result.sync.wait();
    Ok(())
}

fn copy_to_shm<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    buffer: &WlBuffer,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> Result<(), FailureReason> {
    let buffer_size = Size::from((size.w, size.h));
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, buffer_size)
        .map_err(|_| FailureReason::Unknown)?;
    let mapping = {
        let mut target = renderer
            .bind(&mut texture)
            .map_err(|_| FailureReason::Unknown)?;
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        tracker
            .render_output(renderer, &mut target, 0, elements, [0.0, 0.0, 0.0, 1.0])
            .map_err(|_| FailureReason::Unknown)?;
        renderer
            .copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Abgr8888)
            .map_err(|err| warn!(?err, "capture readback failed"))
            .map_err(|_| FailureReason::Unknown)?
    };
    // "Flipped" relative to GL's bottom-left origin: rows already run top
    // to bottom, which is what clients expect.
    let top_down = mapping.flipped();
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|_| FailureReason::Unknown)?;
    let row = size.w as usize * 4;

    with_buffer_contents_mut(buffer, |base, len, data| {
        if data.width != size.w
            || data.height != size.h
            || !matches!(
                data.format,
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
            )
        {
            return Err(FailureReason::BufferConstraints);
        }
        let stride = data.stride as usize;
        let offset = data.offset as usize;
        if offset + stride * (size.h as usize - 1) + row > len
            || pixels.len() < row * size.h as usize
        {
            return Err(FailureReason::BufferConstraints);
        }
        let mut line = vec![0u8; row];
        for y in 0..size.h as usize {
            let src_y = if top_down { y } else { size.h as usize - 1 - y };
            line.copy_from_slice(&pixels[src_y * row..src_y * row + row]);
            // We read RGBA; ARGB8888/XRGB8888 are BGRA in memory.
            for pixel in line.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            // Safety: in bounds (checked above); the pool is shared memory
            // the client may scribble on, so it's only written, never read.
            unsafe {
                std::ptr::copy_nonoverlapping(line.as_ptr(), base.add(offset + y * stride), row);
            }
        }
        Ok(())
    })
    .map_err(|_| FailureReason::Unknown)?
}

// --- Protocol plumbing.

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.capture.toplevel_list
    }
}
smithay::delegate_foreign_toplevel_list!(State);

impl GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtOutputImageCaptureSourceManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_output_image_capture_source_manager_v1::Request::CreateSource {
            source,
            output,
        } = request
        {
            let source_kind = Output::from_resource(&output)
                .map_or(Source::Gone, |o| Source::Output(o.downgrade()));
            data_init.init(source, SourceData(source_kind));
        }
    }
}

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for State {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource {
            source,
            toplevel_handle,
        } = request
        {
            let source_kind = handle_of(&toplevel_handle)
                .map_or(Source::Gone, |h| Source::Toplevel(h.downgrade()));
            data_init.init(source, SourceData(source_kind));
        }
    }
}

fn handle_of(resource: &ExtForeignToplevelHandleV1) -> Option<ForeignToplevelHandle> {
    ForeignToplevelHandle::from_resource(resource)
}

impl Dispatch<ExtImageCaptureSourceV1, SourceData> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: ext_image_capture_source_v1::Request,
        _: &SourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only destroy.
    }
}

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtImageCopyCaptureManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_image_copy_capture_manager_v1::Request::CreateSession {
                session,
                source,
                options,
            } => {
                let options = match options.into_result() {
                    Ok(options) => options,
                    Err(_) => {
                        manager.post_error(
                            ext_image_copy_capture_manager_v1::Error::InvalidOption,
                            "unknown options",
                        );
                        return;
                    }
                };
                let source = source
                    .data::<SourceData>()
                    .map_or(Source::Gone, |d| d.0.clone());
                debug!(?source, "capture session started");
                let session = data_init.init(
                    session,
                    SessionData {
                        source,
                        paint_cursors: options.contains(Options::PaintCursors),
                        captured_once: AtomicBool::new(false),
                        stopped: AtomicBool::new(false),
                    },
                );
                state.capture.send_constraints(&session, &state.space);
                state.capture.sessions.push(session);
            }
            ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
                session,
                ..
            } => {
                // Cursors are painted into frames (paint_cursors) instead.
                data_init.init(
                    session,
                    CursorSessionData {
                        session_created: AtomicBool::new(false),
                    },
                );
            }
            ext_image_copy_capture_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorSessionData> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        resource: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        data: &CursorSessionData,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } =
            request
        {
            if data.session_created.swap(true, Ordering::Relaxed) {
                resource.post_error(
                    ext_image_copy_capture_cursor_session_v1::Error::DuplicateSession,
                    "get_capture_session sent twice",
                );
                return;
            }
            let session = data_init.init(
                session,
                SessionData {
                    source: Source::Gone,
                    paint_cursors: false,
                    captured_once: AtomicBool::new(false),
                    stopped: AtomicBool::new(true),
                },
            );
            session.stopped();
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, SessionData> for State {
    fn request(
        _: &mut Self,
        _: &Client,
        session: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        _: &SessionData,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = request {
            // (The duplicate_frame check is left to well-behaved clients.)
            data_init.init(
                frame,
                FrameData {
                    session: session.clone(),
                    buffer: Mutex::new(None),
                    captured: AtomicBool::new(false),
                    requested: Mutex::new(None),
                },
            );
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, FrameData> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use ext_image_copy_capture_frame_v1::{Error, Request};
        match request {
            Request::AttachBuffer { buffer } => {
                if data.captured.load(Ordering::Relaxed) {
                    frame.post_error(Error::AlreadyCaptured, "buffer attached after capture");
                    return;
                }
                if let Ok(mut slot) = data.buffer.lock() {
                    *slot = Some(buffer);
                }
            }
            Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if data.captured.load(Ordering::Relaxed) {
                    frame.post_error(Error::AlreadyCaptured, "damage after capture");
                } else if x < 0 || y < 0 || width <= 0 || height <= 0 {
                    frame.post_error(Error::InvalidBufferDamage, "invalid damage");
                }
                // We always redraw the whole buffer, so damage is otherwise moot.
            }
            Request::Capture => {
                if data.captured.swap(true, Ordering::Relaxed) {
                    frame.post_error(Error::AlreadyCaptured, "capture sent twice");
                    return;
                }
                if data.buffer.lock().map(|b| b.is_none()).unwrap_or(true) {
                    frame.post_error(Error::NoBuffer, "capture without a buffer");
                    return;
                }
                let stopped = data
                    .session
                    .data::<SessionData>()
                    .is_none_or(|s| s.stopped.load(Ordering::Relaxed));
                if stopped {
                    frame.failed(FailureReason::Stopped);
                    return;
                }
                if let Ok(mut requested) = data.requested.lock() {
                    *requested = Some(Instant::now());
                }
                state.capture.pending.push(frame.clone());
                // Something renders now (for a first frame), and again once
                // it's been waiting long enough to get a repeat.
                state.queue_redraw();
                let timer = Timer::from_duration(REPEAT_AFTER + Duration::from_millis(5));
                let _ = state.loop_handle.insert_source(timer, |_, _, state| {
                    if state.capture.has_pending() {
                        state.queue_redraw();
                    }
                    TimeoutAction::Drop
                });
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ExtImageCopyCaptureFrameV1,
        _: &FrameData,
    ) {
        state.capture.pending.retain(|f| f != frame);
    }
}
