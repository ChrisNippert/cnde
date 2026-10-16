//! cnde-shell: the panel (and later quick settings, notifications, the
//! launcher). One process drawing layer-shell surfaces with cnde-ui
//! (ARCHITECTURE.md §5). It only redraws when something changes: the
//! minute ticks, workspaces change, or a monitor comes or goes.

mod actions;
mod config;
mod panel;
mod quick_settings;
mod status;
mod workspaces;

use std::time::Duration;

use chrono::{Local, Timelike};
use cnde_config::Config;
use cnde_ui::{Fonts, Icons, Theme};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop,
            channel::{self, Event},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        xdg::{
            XdgShell,
            popup::{Popup, PopupConfigure, PopupHandler},
            window::{Window, WindowConfigure, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tracing::{info, warn};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;

use crate::{
    actions::Actions, panel::Panel, quick_settings::QuickSettings, status::Update,
    workspaces::Workspaces,
};

/// The latest of everything the status threads report.
#[derive(Debug, Default)]
pub struct Status {
    pub battery: Option<status::Battery>,
    pub network: status::Network,
    pub wifi_enabled: Option<bool>,
    pub volume: Option<status::Volume>,
}

impl Status {
    fn apply(&mut self, update: Update) {
        match update {
            Update::Battery(battery) => self.battery = battery,
            Update::Network(network) => self.network = network,
            Update::WifiEnabled(enabled) => self.wifi_enabled = enabled,
            Update::Volume(volume) => self.volume = volume,
        }
    }
}

pub struct App {
    registry: RegistryState,
    outputs: OutputState,
    seat: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    xdg_shell: XdgShell,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<App>,
    /// ABGR8888 if the compositor offers it (we draw RGBA, zero-copy), else
    /// the always-available ARGB8888 with a channel swap.
    shm_format: wl_shm::Format,

    config: Config,
    theme: Theme,
    fonts: Fonts,
    icons: Icons,
    status: Status,
    panels: Vec<Panel>,
    pub workspaces: Workspaces,
    actions: Actions,
    /// Open quick settings, and the panel-surface rect it hangs from.
    quick_settings: Option<(QuickSettings, (i32, i32, i32, i32))>,
    seat_handle: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;

    let config = config::load();
    let theme = config::theme_from(&config);
    let fonts = Fonts::new(theme.font_families);
    info!(font = fonts.family(), "cnde-shell starting");

    let shm = Shm::bind(&globals, &qh)?;
    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seat: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh)?,
        layer_shell: LayerShell::bind(&globals, &qh)?,
        xdg_shell: XdgShell::bind(&globals, &qh)?,
        pool: SlotPool::new(4 * 1024 * 1024, &shm)?,
        shm_format: wl_shm::Format::Argb8888,
        shm,
        qh: qh.clone(),
        config,
        theme,
        fonts,
        icons: Icons::new(),
        status: Status::default(),
        panels: Vec::new(),
        workspaces: Workspaces::default(),
        actions: Actions::start(),
        quick_settings: None,
        seat_handle: None,
        pointer: None,
        keyboard: None,
    };
    app.workspaces.manager = globals
        .bind::<ExtWorkspaceManagerV1, _, _>(&qh, 1..=1, workspaces::ManagerData)
        .map_err(|err| warn!(?err, "no ext-workspace; the indicator stays empty"))
        .ok();

    // One roundtrip so wl_shm has listed its formats before we draw.
    queue.roundtrip(&mut app)?;
    app.choose_shm_format();
    WaylandSource::new(conn, queue).insert(event_loop.handle())?;

    config::watch(&event_loop.handle());

    // Battery, network and volume, each from its own background thread.
    let (status_tx, status_rx) = channel::channel();
    event_loop
        .handle()
        .insert_source(status_rx, |event, _, app| {
            if let Event::Msg(update) = event {
                app.status.apply(update);
                app.redraw_all();
            }
        })?;
    status::start(status_tx);

    // The clock: wake exactly when the minute changes, not every second.
    event_loop
        .handle()
        .insert_source(Timer::from_duration(until_next_minute()), |_, _, app| {
            app.redraw_all();
            TimeoutAction::ToDuration(until_next_minute())
        })?;

    loop {
        event_loop.dispatch(None, &mut app)?;
    }
}

fn until_next_minute() -> Duration {
    let now = Local::now();
    let into_minute = Duration::from_secs(now.second() as u64)
        + Duration::from_nanos(now.nanosecond() as u64 % 1_000_000_000);
    // A little past the boundary, so the new minute is definitely current.
    Duration::from_secs(60).saturating_sub(into_minute) + Duration::from_millis(50)
}

impl App {
    /// Which monitors get a panel: the main one (configured, else the
    /// leftmost), or every one with `panel.all_monitors`.
    pub fn sync_panels(&mut self) {
        let outputs: Vec<(wl_output::WlOutput, String, i32)> = self
            .outputs
            .outputs()
            .filter_map(|o| {
                let info = self.outputs.info(&o)?;
                Some((
                    o,
                    info.name.clone().unwrap_or_default(),
                    info.logical_position.unwrap_or((0, 0)).0,
                ))
            })
            .collect();
        let main = outputs
            .iter()
            .find(|(_, name, _)| self.config.output(name).is_some_and(|c| c.main))
            .or_else(|| outputs.iter().min_by_key(|(_, _, x)| *x))
            .map(|(o, _, _)| o.clone());
        let wanted: Vec<wl_output::WlOutput> = if self.config.panel.all_monitors {
            outputs.into_iter().map(|(o, _, _)| o).collect()
        } else {
            main.into_iter().collect()
        };

        self.panels.retain(|p| wanted.contains(&p.output));
        for output in wanted {
            if !self.panels.iter().any(|p| p.output == output) {
                let panel = Panel::new(
                    &self.compositor,
                    &self.layer_shell,
                    &self.qh,
                    output,
                    &self.theme,
                );
                self.panels.push(panel);
            }
        }
    }

    pub fn redraw_all(&mut self) {
        for i in 0..self.panels.len() {
            self.draw_panel(i);
        }
        self.draw_quick_settings();
    }

    fn draw_quick_settings(&mut self) {
        if let Some((qs, _)) = &mut self.quick_settings {
            qs.draw(
                &mut self.pool,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &self.theme,
                &self.status,
            );
        }
    }

    /// Clicking the status area opens quick settings, or closes it.
    fn toggle_quick_settings(&mut self, panel_index: usize, serial: u32) {
        if self.quick_settings.take().is_some() {
            return;
        }
        let (Some(panel), Some(seat)) = (self.panels.get(panel_index), &self.seat_handle) else {
            return;
        };
        let Some(anchor) = panel.status_anchor() else {
            return;
        };
        self.quick_settings = QuickSettings::open(
            &self.compositor,
            &self.xdg_shell,
            &self.qh,
            &panel.layer,
            seat,
            serial,
            anchor,
            &self.theme,
            panel.scale(),
        )
        .map(|qs| (qs, anchor));
    }

    fn quick_settings_pointer(&mut self, event: &PointerEvent) {
        let Some((qs, anchor)) = &mut self.quick_settings else {
            return;
        };
        let close = match event.kind {
            PointerEventKind::Press {
                button: BTN_LEFT, ..
            } => qs.press(
                event.position,
                &self.status,
                &self.actions,
                &self.xdg_shell,
                &self.theme,
                *anchor,
            ),
            PointerEventKind::Motion { .. } => {
                qs.motion(event.position, &self.actions);
                false
            }
            PointerEventKind::Release {
                button: BTN_LEFT, ..
            } => {
                qs.release();
                false
            }
            _ => return,
        };
        if close {
            self.quick_settings = None;
        } else {
            self.draw_quick_settings();
        }
    }

    fn draw_panel(&mut self, index: usize) {
        let Some(panel) = self.panels.get_mut(index) else {
            return;
        };
        panel.draw(
            &mut self.pool,
            self.shm_format,
            panel::Draw {
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: &self.theme,
                clock_format: &self.config.panel.clock_format,
                workspaces: &self.workspaces.list,
                status: &self.status,
            },
        );
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if let Some(i) = self
            .panels
            .iter()
            .position(|p| p.layer.wl_surface() == surface)
        {
            self.panels[i].set_scale(factor);
            self.draw_panel(i);
        }
        if let Some((qs, _)) = &mut self.quick_settings
            && qs.popup.wl_surface() == surface
        {
            qs.set_scale(factor);
            self.draw_quick_settings();
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.sync_panels();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.sync_panels();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.sync_panels();
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor took it away (e.g. its monitor was unplugged).
        self.panels.retain(|p| &p.layer != layer);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(i) = self.panels.iter().position(|p| &p.layer == layer) {
            self.panels[i].configure(configure.new_size);
            self.draw_panel(i);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat.get_pointer(qh, &seat).ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat.get_keyboard(qh, &seat, None).ok();
        }
        self.seat_handle = Some(seat);
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let on_quick_settings = self
                .quick_settings
                .as_ref()
                .is_some_and(|(qs, _)| qs.popup.wl_surface() == &event.surface);
            if on_quick_settings {
                self.quick_settings_pointer(event);
                continue;
            }
            let PointerEventKind::Press {
                button: BTN_LEFT,
                serial,
                ..
            } = event.kind
            else {
                continue;
            };
            let Some(index) = self
                .panels
                .iter()
                .position(|p| p.layer.wl_surface() == &event.surface)
            else {
                continue;
            };
            let panel = &self.panels[index];
            if panel.status_at(event.position) {
                self.toggle_quick_settings(index, serial);
            } else if let Some(handle) = panel.workspace_at(event.position) {
                self.workspaces.activate(&handle);
            }
        }
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        _: PopupConfigure,
    ) {
        if let Some((qs, _)) = &mut self.quick_settings
            && &qs.popup == popup
        {
            qs.configured();
            self.draw_quick_settings();
        }
    }

    /// The compositor dismissed it (a click outside).
    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self
            .quick_settings
            .as_ref()
            .is_some_and(|(qs, _)| &qs.popup == popup)
        {
            self.quick_settings = None;
        }
    }
}

/// Required by sctk's xdg-shell binding; the shell never opens windows.
impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {}
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        _: WindowConfigure,
        _: u32,
    ) {
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            self.quick_settings = None;
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl App {
    /// Pick the buffer format once the compositor has listed its formats.
    fn choose_shm_format(&mut self) {
        self.shm_format = if self.shm.formats().contains(&wl_shm::Format::Abgr8888) {
            wl_shm::Format::Abgr8888
        } else {
            wl_shm::Format::Argb8888
        };
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
delegate_dispatch2!(App);
