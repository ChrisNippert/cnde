// Adapted from smithay: smallvil/src/state.rs

use std::{
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType, utils::send_frames_surface_tree},
    input::{
        Seat, SeatState,
        keyboard::Keycode,
        pointer::{CursorImageStatus, MotionEvent},
    },
    output::Output,
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::{
            wlr_layer::{Layer, WlrLayerShellState},
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        xdg_activation::XdgActivationState,
        socket::ListeningSocketSource,
        presentation::PresentationState,
        viewporter::ViewporterState,
    },
};
use tracing::warn;

use std::path::PathBuf;

use cnde_config::Config;

use crate::{
    backend::Backend, effects::ClosingWindow, keybinds::ModKey, snap::SnapPreview, watchdog::Phase,
};

/// How long the pointer stays busy after a launch with no window opening.
/// The most often the shell is told about windows, edges and workspaces,
/// and window colours are sampled: the loop runs over a thousand times a
/// second while apps are busy, and nothing these feed needs more than a
/// refresh's worth.
const SYNC_EVERY: Duration = Duration::from_millis(8);

pub const LAUNCH_FEEDBACK: std::time::Duration = std::time::Duration::from_secs(5);

pub struct State {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, State>,

    pub backend: Backend,
    pub xwayland: Option<crate::xwayland::Xwayland>,
    pub config: Config,
    /// Where `config` came from; `None` if there's no home directory.
    pub config_path: Option<PathBuf>,
    /// Set whenever something on screen changed; the backend renders once per request.
    pub redraw_queued: bool,
    /// Monitors that need a frame when only some do (a window's own commit);
    /// `redraw_queued` overrides it with every monitor.
    pub redraw_outputs: Vec<Output>,
    /// Set when windows moved or resized under the pointer; see `refresh_pointer_focus`.
    pub pointer_focus_dirty: bool,

    pub space: Space<Window>,
    pub workspaces: crate::workspaces::Workspaces,
    /// Individual workspaces: name of the monitor workspace keys act on,
    /// set by clicks and focus (see `current_output`).
    pub workspace_monitor: Option<String>,
    pub ext_workspace_state: crate::ext_workspace::ExtWorkspaceState,
    /// A three-finger swipe is driving the workspace strip.
    pub swipe_active: bool,
    /// An app was started (the shell says): the pointer shows it's busy
    /// until a window maps or `LAUNCH_FEEDBACK` passes.
    pub launching: Option<std::time::Instant>,
    /// Which way the current three-finger swipe goes, once it's clear:
    /// true = sideways (workspaces), false = up/down (the overview).
    pub swipe_sideways: Option<bool>,
    /// Travel before the direction was clear.
    pub swipe_pending: (f64, f64),
    /// Toplevels that exist but haven't attached a buffer yet. They are mapped
    /// (placed + animated in) on their first buffer commit.
    pub unmapped: Vec<Window>,
    /// The open popup grab is the shell's (quick settings, a menu): a
    /// click that dismisses it also reaches the window under it.
    pub shell_popup_grab: bool,
    /// Snapshots of destroyed windows, still animating out.
    pub closing: Vec<ClosingWindow>,
    /// Window id to the dock icon holding it: its output and cell within
    /// the dock's surface (see `Request::DockIcons`).
    pub dock_icons: std::collections::HashMap<u64, (String, [f64; 4])>,
    pub wallpapers: crate::wallpaper::Wallpapers,
    pub dock_frost: crate::frost::DockFrost,
    /// Window colours for the adaptive palette.
    pub tints: crate::tint::Tints,
    /// A timer is set to sample a window `tints` held back.
    pub tint_wake: bool,
    /// When the shell sync and the adaptive colours may next run (see
    /// `SYNC_EVERY`), and whether a timer will wake the loop for it.
    sync_at: Instant,
    sync_wake: bool,
    pub crossfade: crate::crossfade::Crossfade,
    /// Dark mode on a timer.
    pub schedules: crate::schedule::Schedules,
    pub night_light: crate::night_light::NightLight,
    /// Dimming, screen off and suspend when idle.
    pub idle: crate::power::Idle,
    pub decorations: crate::decorations::Decorations,
    pub bar_input: crate::decorations::BarInput,
    pub overview: crate::overview::Overview,
    /// The Alt+Tab switcher, and windows by last focus, newest first.
    pub switcher: crate::switcher::Switcher,
    pub mru: Vec<smithay::desktop::Window>,
    pub ipc: crate::ipc::IpcClients,
    pub popup_anims: crate::popup_anim::PopupAnims,
    pub capture: crate::capture::CaptureState,
    pub lock: crate::lock::Lock,
    pub screenshot: crate::screenshot::Screenshot,
    /// Super+Shift+S without a shell: capture this monitor once frozen.
    pub pending_quick_shot: Option<String>,
    /// Drag-to-edge preview while a window is being moved.
    pub snap_preview: Option<SnapPreview>,
    /// The snap zones, lit a moment after their size changes.
    pub edge_hint: Option<crate::snap::EdgeHint>,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
    /// What the pointer should look like, as last requested by a client.
    pub cursor_status: CursorImageStatus,
    /// A drag-and-drop is under way (the pointer shows it), and the icon
    /// surface its source asked to have drawn at the pointer.
    pub dnd_active: bool,
    pub dnd_icon: Option<WlSurface>,
    pub mod_key: ModKey,
    /// `[shortcuts]`, compiled; rebuilt on config changes.
    pub shortcuts: crate::keybinds::Shortcuts,
    /// Show Desktop: the workspace and windows it minimized, to bring back.
    pub desktop_shown: Option<(u64, Vec<u64>)>,
    /// The mod key went down alone; releasing it without anything else
    /// happening in between counts as a tap (opens the launcher).
    pub mod_tap_armed: bool,
    /// Set by the key filter when a tap completed; acted on after it returns.
    pub mod_tapped: bool,
    /// When a tap last opened the overview: a second tap soon after goes
    /// on to the app grid, a later one closes it.
    pub mod_tap_opened: Option<std::time::Instant>,
    /// Keys whose press triggered a compositor binding; their releases are
    /// swallowed too so clients never see half a key event.
    pub suppressed_keys: Vec<Keycode>,
    /// A held volume or brightness key, and its repeat timer.
    pub media_repeat: Option<(Keycode, smithay::reexports::calloop::RegistrationToken)>,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub xdg_activation_state: XdgActivationState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)] // Holds the wp_presentation global.
    pub presentation_state: PresentationState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub fractional_scale_state: FractionalScaleManagerState,
    pub data_device_state: DataDeviceState,
}

impl State {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend: Backend,
        mod_key: ModKey,
        seat_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(
            &dh,
            vec![smithay::reexports::wayland_server::protocol::wl_shm::Format::Abgr8888],
        );
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(
            &dh,
            <smithay::utils::Monotonic as smithay::utils::ClockSource>::ID as u32,
        );
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(&dh);
        // Games: raw mouse motion, and the pointer locked or confined to
        // their window so turning the camera doesn't stop at a screen edge.
        smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(&dh);
        smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(&dh);
        let lock = crate::lock::Lock::new(
            smithay::wayland::session_lock::SessionLockManagerState::new::<Self, _>(&dh, |_| true),
        );
        crate::ext_workspace::init(&dh);
        let idle = crate::power::Idle::new(&dh, event_loop.handle());
        let capture = crate::capture::CaptureState::new(&dh);
        let mut seat_state = SeatState::new();

        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, seat_name);
        // Always advertise a keyboard and pointer: clients expect them, and
        // input devices come and go underneath (libinput hotplug).
        // Layout comes from XKB_DEFAULT_* until the config crate exists.
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        Ok(Self {
            start_time: Instant::now(),
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            loop_handle: event_loop.handle(),

            backend,
            xwayland: None,
            config: Config::default(),
            config_path: None,
            redraw_queued: true,
            redraw_outputs: Vec::new(),
            pointer_focus_dirty: false,

            space: Space::default(),
            workspaces: crate::workspaces::Workspaces::new(),
            ext_workspace_state: Default::default(),
            workspace_monitor: None,
            swipe_active: false,
            launching: None,
            swipe_sideways: None,
            swipe_pending: (0.0, 0.0),
            unmapped: Vec::new(),
            shell_popup_grab: false,
            closing: Vec::new(),
            dock_icons: Default::default(),
            wallpapers: crate::wallpaper::Wallpapers::new(&event_loop.handle(), &Config::default()),
            dock_frost: Default::default(),
            tints: Default::default(),
            tint_wake: false,
            sync_at: Instant::now(),
            sync_wake: false,
            crossfade: Default::default(),
            schedules: Default::default(),
            night_light: Default::default(),
            idle,
            decorations: Default::default(),
            bar_input: Default::default(),
            overview: crate::overview::Overview::new(),
            switcher: Default::default(),
            mru: Vec::new(),
            ipc: Default::default(),
            popup_anims: Default::default(),
            capture,
            lock,
            screenshot: Default::default(),
            pending_quick_shot: None,
            snap_preview: None,
            edge_hint: None,
            popups: PopupManager::default(),

            seat,
            cursor_status: CursorImageStatus::default_named(),
            dnd_active: false,
            dnd_icon: None,
            mod_key,
            shortcuts: Default::default(),
            desktop_shown: None,
            mod_tap_armed: false,
            mod_tapped: false,
            mod_tap_opened: None,
            suppressed_keys: Vec::new(),
            media_repeat: None,

            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            xdg_activation_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            viewporter_state,
            presentation_state,
            fractional_scale_state,
            data_device_state,
        })
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> Result<OsString, Box<dyn std::error::Error>> {
        let listening_socket = ListeningSocketSource::new_auto()?;
        let socket_name = listening_socket.socket_name().to_os_string();

        let handle = event_loop.handle();
        handle.insert_source(listening_socket, |stream, _, state| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                warn!(?err, "failed to add wayland client");
            }
        })?;

        handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // Safety: the display is owned by this source and never dropped while dispatching.
                unsafe {
                    let start = crate::perf::now();
                    display.get_mut().dispatch_clients(state)?;
                    crate::perf::since("clients", start);
                }
                Ok(PostAction::Continue)
            },
        )?;

        Ok(socket_name)
    }

    /// Runs after every batch of events: housekeeping, then ask for a frame if needed.
    pub fn on_loop_iteration(&mut self) {
        crate::watchdog::beat(Phase::Housekeeping);
        let start = crate::perf::now();
        self.space.refresh();
        self.trim_window_outputs();
        self.popups.cleanup();
        self.update_idle_inhibit();
        self.finish_workspace_motion(Instant::now());
        self.finish_overview_animation(Instant::now());
        self.popup_anims.finish(Instant::now());
        if self.schedules.dirty {
            self.update_schedules();
        }
        if self.pointer_focus_dirty {
            self.pointer_focus_dirty = false;
            // Windows dragged to another monitor join its workspace.
            self.sync_workspace_membership();
            self.refresh_pointer_focus();
        }
        let outputs = std::mem::take(&mut self.redraw_outputs);
        if self.redraw_queued {
            self.redraw_queued = false;
            match &self.backend {
                Backend::Winit(winit) => winit.request_redraw(),
                Backend::Udev(_) => self.udev_queue_redraw_all(),
            }
        } else if !outputs.is_empty() {
            match &self.backend {
                Backend::Winit(winit) => winit.request_redraw(),
                Backend::Udev(_) => self.udev_queue_redraw_outputs(&outputs),
            }
        }
        // Why render here and not in an idle callback: calloop runs idles
        // *before* this hook, so an idle queued from here sat until some
        // unrelated event woke the loop. Typing showed up late.
        crate::perf::since("housekeeping", start);
        crate::watchdog::beat(Phase::Render);
        if matches!(self.backend, Backend::Udev(_)) {
            crate::perf::time("render", || self.udev_render_queued());
        }
        crate::watchdog::beat(Phase::Finish);
        // Window captures don't wait for a monitor to redraw: the window may
        // be on a hidden workspace.
        if self.capture.has_pending()
            && let Some(renderer) = self.backend.renderer()
        {
            crate::perf::time("window_capture", || {
                crate::capture::capture_toplevels(&mut self.capture, renderer, &self.space)
            });
        }
        if self.sync_due() {
            crate::perf::time("adaptive_tints", || self.update_tints());
            let start = crate::perf::now();
            self.refresh_ext_workspaces();
            self.sync_ipc_windows();
            self.sync_ipc_edges();
            crate::perf::since("ipc_sync", start);
        }
        self.screenshot_ready_check();
        // Last, so frame callbacks sent while rendering go out now.
        let start = crate::perf::now();
        if let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, "failed to flush clients");
        }
        crate::perf::since("flush", start);
        crate::watchdog::beat(Phase::Dispatch);
    }

    /// A moment after startup: compile shaders and load fonts the switcher,
    /// dock preview and thumbnails use, so their first showing doesn't
    /// stutter.
    pub fn prepare_effects(&mut self) {
        let Some(renderer) = self.backend.renderer() else {
            return;
        };
        self.switcher.prepare(renderer);
        self.decorations.downscale.program(renderer);
    }

    /// Whether the syncs may run now; if not, a timer brings the loop back
    /// when they may, so the last change still goes out.
    fn sync_due(&mut self) -> bool {
        let now = Instant::now();
        if now >= self.sync_at {
            self.sync_at = now + SYNC_EVERY;
            return true;
        }
        if !self.sync_wake {
            self.sync_wake = true;
            let timer = Timer::from_deadline(self.sync_at);
            let _ = self.loop_handle.insert_source(timer, |_, _, state| {
                state.sync_wake = false;
                TimeoutAction::Drop
            });
        }
        false
    }

    /// Presentation-time feedback for everything visible on `output`, to
    /// fire when this frame reaches the screen. Games pace themselves on it
    /// (vkd3d's present-wait), so it must exist.
    pub fn presentation_feedback(
        space: &smithay::desktop::Space<smithay::desktop::Window>,
        output: &Output,
    ) -> smithay::desktop::utils::OutputPresentationFeedback {
        use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
        let mut feedback = smithay::desktop::utils::OutputPresentationFeedback::new(output);
        for window in space.elements_for_output(output) {
            window.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| Kind::empty(),
            );
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(output.clone()),
                |_, _| Kind::empty(),
            );
        }
        feedback
    }

    /// Tell clients visible on `output` to draw their next frame.
    pub fn send_frame_callbacks(&self, output: &Output) {
        let elapsed = self.start_time.elapsed();
        for window in self.space.elements_for_output(output) {
            window.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        self.send_layer_frame_callbacks(output, elapsed);
        if let CursorImageStatus::Surface(surface) = &self.cursor_status {
            send_frames_surface_tree(surface, output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        if let Some(surface) = &self.dnd_icon {
            send_frames_surface_tree(surface, output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        if let Some(surface) = self.lock.surface(output) {
            send_frames_surface_tree(surface, output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    pub fn client_env(&self) -> crate::spawn::ClientEnv {
        crate::spawn::ClientEnv {
            wayland_display: self.socket_name.clone(),
            x11_display: self.xwayland.as_ref().map(|x| x.display_name()),
        }
    }

    pub fn queue_redraw(&mut self) {
        self.redraw_queued = true;
    }

    /// Only `output` changed, e.g. a game on one monitor drew a frame: the
    /// other monitors needn't redraw.
    pub fn queue_redraw_output(&mut self, output: &Output) {
        if !self.redraw_outputs.contains(output) {
            self.redraw_outputs.push(output.clone());
        }
    }

    /// Window layout changed: re-check what's under the pointer at the next
    /// loop iteration (batched, so many changes cost one update).
    pub fn queue_pointer_refresh(&mut self) {
        self.pointer_focus_dirty = true;
    }

    /// Re-send the pointer position after windows moved under a stationary
    /// cursor. Otherwise clients keep stale surface-local coordinates until
    /// the mouse moves, which breaks hover and double-click detection (e.g. a
    /// titlebar double-click right after a maximize).
    fn refresh_pointer_focus(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // While a grab is active (a held button, a move/resize) it owns pointer
        // focus. Retry after it ends: e.g. a titlebar double-click maximizes
        // while the button is still down, and the *next* click must already
        // see the window's new position.
        if pointer.is_grabbed() {
            self.pointer_focus_dirty = true;
            return;
        }
        let location = pointer.current_location();
        let under = self.surface_under(location);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }

    /// The surface under `pos`, honouring stacking: overlay and top layers,
    /// then windows, then bottom and background layers.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.lock.is_locked() {
            return self.lock_surface_under(pos);
        }
        // The dock's window preview is drawn over everything.
        if self.preview_covers(pos) {
            return None;
        }
        if let Some((_, surface, loc)) =
            self.layer_surface_under(self.layers_above_windows(pos), pos)
        {
            return Some((surface, loc));
        }
        // In the overview, windows are pictures: the compositor handles clicks.
        // While a screenshot is being framed, only its tool takes input.
        if self.overview.is_open() || self.screenshot.active {
            return None;
        }
        // A title bar or resize border cnde draws: the compositor's.
        if self.decoration_under(pos).is_some() {
            return None;
        }
        let window = self
            .space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            });
        window.or_else(|| {
            self.layer_surface_under(&[Layer::Bottom, Layer::Background], pos)
                .map(|(_, surface, loc)| (surface, loc))
        })
    }

    /// Layers drawn above windows at `pos`: the panel (top layer) hides
    /// behind a fullscreen window.
    pub fn layers_above_windows(&self, pos: Point<f64, Logical>) -> &'static [Layer] {
        if self.screenshot.active {
            return &[Layer::Overlay];
        }
        let fullscreen = !self.overview.is_open()
            && self
                .space
                .output_under(pos)
                .next()
                .is_some_and(|output| self.fullscreen_on(output));
        if fullscreen {
            &[Layer::Overlay]
        } else {
            &[Layer::Overlay, Layer::Top]
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        // Apps only print the code; the message says what went wrong.
        if let DisconnectReason::ProtocolError(err) = reason {
            warn!(%err, "client disconnected for a protocol error");
        }
    }
}
