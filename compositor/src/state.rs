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
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic,
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
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::{
            wlr_layer::{Layer, WlrLayerShellState},
            xdg::{XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};
use tracing::warn;

use std::path::PathBuf;

use cnde_config::Config;

use crate::{backend::Backend, effects::ClosingWindow, keybinds::ModKey, snap::SnapPreview};

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
    /// Set when windows moved or resized under the pointer; see `refresh_pointer_focus`.
    pub pointer_focus_dirty: bool,

    pub space: Space<Window>,
    pub workspaces: crate::workspaces::Workspaces,
    pub ext_workspace_state: crate::ext_workspace::ExtWorkspaceState,
    /// A three-finger swipe is driving the workspace strip.
    pub swipe_active: bool,
    /// Toplevels that exist but haven't attached a buffer yet. They are mapped
    /// (placed + animated in) on their first buffer commit.
    pub unmapped: Vec<Window>,
    /// Snapshots of destroyed windows, still animating out.
    pub closing: Vec<ClosingWindow>,
    pub wallpapers: crate::wallpaper::Wallpapers,
    pub dock_frost: crate::frost::DockFrost,
    pub crossfade: crate::crossfade::Crossfade,
    pub decorations: crate::decorations::Decorations,
    pub bar_input: crate::decorations::BarInput,
    pub overview: crate::overview::Overview,
    pub ipc: crate::ipc::IpcClients,
    pub popup_anims: crate::popup_anim::PopupAnims,
    pub capture: crate::capture::CaptureState,
    pub screenshot: crate::screenshot::Screenshot,
    /// Super+Shift+S without a shell: capture this monitor once frozen.
    pub pending_quick_shot: Option<String>,
    /// Drag-to-edge preview while a window is being moved.
    pub snap_preview: Option<SnapPreview>,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
    /// What the pointer should look like, as last requested by a client.
    pub cursor_status: CursorImageStatus,
    pub mod_key: ModKey,
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

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub viewporter_state: ViewporterState,
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
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        crate::ext_workspace::init(&dh);
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
            pointer_focus_dirty: false,

            space: Space::default(),
            workspaces: crate::workspaces::Workspaces::new(),
            ext_workspace_state: Default::default(),
            swipe_active: false,
            unmapped: Vec::new(),
            closing: Vec::new(),
            wallpapers: crate::wallpaper::Wallpapers::new(&event_loop.handle(), &Config::default()),
            dock_frost: Default::default(),
            crossfade: Default::default(),
            decorations: Default::default(),
            bar_input: Default::default(),
            overview: crate::overview::Overview::new(),
            ipc: Default::default(),
            popup_anims: Default::default(),
            capture,
            screenshot: Default::default(),
            pending_quick_shot: None,
            snap_preview: None,
            popups: PopupManager::default(),

            seat,
            cursor_status: CursorImageStatus::default_named(),
            mod_key,
            mod_tap_armed: false,
            mod_tapped: false,
            mod_tap_opened: None,
            suppressed_keys: Vec::new(),

            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            viewporter_state,
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
                    display.get_mut().dispatch_clients(state)?;
                }
                Ok(PostAction::Continue)
            },
        )?;

        Ok(socket_name)
    }

    /// Runs after every batch of events: housekeeping, then ask for a frame if needed.
    pub fn on_loop_iteration(&mut self) {
        self.space.refresh();
        self.popups.cleanup();
        self.finish_workspace_motion(Instant::now());
        self.finish_overview_animation(Instant::now());
        self.popup_anims.finish(Instant::now());
        if self.pointer_focus_dirty {
            self.pointer_focus_dirty = false;
            // Windows dragged to another monitor join its workspace.
            self.sync_workspace_membership();
            self.refresh_pointer_focus();
        }
        if self.redraw_queued {
            self.redraw_queued = false;
            match &self.backend {
                Backend::Winit(winit) => winit.request_redraw(),
                Backend::Udev(_) => self.udev_queue_redraw_all(),
            }
        }
        // Why render here and not in an idle callback: calloop runs idles
        // *before* this hook, so an idle queued from here sat until some
        // unrelated event woke the loop. Typing showed up late.
        if matches!(self.backend, Backend::Udev(_)) {
            self.udev_render_queued();
        }
        // Window captures don't wait for a monitor to redraw: the window may
        // be on a hidden workspace.
        if self.capture.has_pending()
            && let Some(renderer) = self.backend.renderer()
        {
            crate::capture::capture_toplevels(&mut self.capture, renderer, &self.space);
        }
        self.refresh_ext_workspaces();
        self.sync_ipc_windows();
        self.screenshot_ready_check();
        // Last, so frame callbacks sent while rendering go out now.
        if let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, "failed to flush clients");
        }
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
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
