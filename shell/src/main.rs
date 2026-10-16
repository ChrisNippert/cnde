//! cnde-shell: the panel (and later quick settings, notifications, the
//! launcher). One process drawing layer-shell surfaces with cnde-ui
//! (ARCHITECTURE.md §5). It only redraws when something changes: the
//! minute ticks, workspaces change, or a monitor comes or goes.

mod config;
mod panel;
mod workspaces;

use std::time::Duration;

use chrono::{Local, Timelike};
use cnde_config::Config;
use cnde_ui::{Fonts, Theme};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_output, wl_pointer, wl_seat, wl_surface},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tracing::{info, warn};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;

use crate::{panel::Panel, workspaces::Workspaces};

pub struct App {
    registry: RegistryState,
    outputs: OutputState,
    seat: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<App>,

    config: Config,
    theme: Theme,
    fonts: Fonts,
    panels: Vec<Panel>,
    pub workspaces: Workspaces,
    pointer: Option<wl_pointer::WlPointer>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    WaylandSource::new(conn, queue).insert(event_loop.handle())?;

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
        pool: SlotPool::new(4 * 1024 * 1024, &shm)?,
        shm,
        qh: qh.clone(),
        config,
        theme,
        fonts,
        panels: Vec::new(),
        workspaces: Workspaces::default(),
        pointer: None,
    };
    app.workspaces.manager = globals
        .bind::<ExtWorkspaceManagerV1, _, _>(&qh, 1..=1, workspaces::ManagerData)
        .map_err(|err| warn!(?err, "no ext-workspace; the indicator stays empty"))
        .ok();

    config::watch(&event_loop.handle());

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
    }

    fn draw_panel(&mut self, index: usize) {
        let Some(panel) = self.panels.get_mut(index) else {
            return;
        };
        panel.draw(
            &mut self.pool,
            &mut self.fonts,
            &self.theme,
            &self.config.panel.clock_format,
            &self.workspaces.list,
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
            let PointerEventKind::Press {
                button: BTN_LEFT, ..
            } = event.kind
            else {
                continue;
            };
            let Some(panel) = self
                .panels
                .iter()
                .find(|p| p.layer.wl_surface() == &event.surface)
            else {
                continue;
            };
            if let Some(handle) = panel.workspace_at(event.position) {
                self.workspaces.activate(&handle);
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
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
