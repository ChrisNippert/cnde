// Adapted from smithay: smallvil/src/state.rs

use std::{ffi::OsString, sync::Arc, time::Instant};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, keyboard::Keycode},
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};
use tracing::warn;

use crate::{backend::winit::WinitBackend, keybinds::ModKey};

pub struct State {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    pub backend: WinitBackend,
    /// Set whenever something on screen changed; the backend renders once per request.
    pub redraw_queued: bool,

    pub space: Space<Window>,
    /// Toplevels that exist but haven't attached a buffer yet. They are mapped
    /// (placed + animated in) on their first buffer commit.
    pub unmapped: Vec<Window>,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
    pub mod_key: ModKey,
    /// Keys whose press triggered a compositor binding; their releases are
    /// swallowed too so clients never see half a key event.
    pub suppressed_keys: Vec<Keycode>,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
}

impl State {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        backend: WinitBackend,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();

        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        // Nested, the host always gives us a keyboard and pointer. Hotplug
        // tracking comes with the libinput backend.
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let mut space = Space::default();
        space.map_output(&backend.output, (0, 0));

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        Ok(Self {
            start_time: Instant::now(),
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),

            backend,
            redraw_queued: true,

            space,
            unmapped: Vec::new(),
            popups: PopupManager::default(),

            seat,
            // Nested inside GNOME etc. the host grabs Super, so use Alt there.
            mod_key: ModKey::Alt,
            suppressed_keys: Vec::new(),

            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
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
        if let Err(err) = self.display_handle.flush_clients() {
            warn!(?err, "failed to flush clients");
        }
        if self.redraw_queued {
            self.redraw_queued = false;
            self.backend.request_redraw();
        }
    }

    pub fn queue_redraw(&mut self) {
        self.redraw_queued = true;
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
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
