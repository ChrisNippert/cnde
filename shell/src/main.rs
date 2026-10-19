//! cnde-shell: the panel, quick settings and the overview's app search (and
//! later notifications). One process drawing layer-shell surfaces with
//! cnde-ui (ARCHITECTURE.md §5). It only redraws when something changes:
//! the minute ticks, workspaces change, or a monitor comes or goes.

mod actions;
mod apps;
mod config;
mod context_menus;
mod dock;
mod edit_mode;
mod ipc;
mod launcher;
mod menu;
mod osd;
mod panel;
mod polkit;
mod quick_settings;
mod calendar;
mod screenshot;
mod status;
mod workspaces;

use std::{path::PathBuf, time::Duration, time::SystemTime};

use chrono::{Local, Timelike};
use cnde_config::Config;
use cnde_ui::{Fonts, Icons, Theme};
use smithay_client_toolkit::reexports::client::backend::WaylandError;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle,
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
        pointer::{
            BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler,
        },
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
use tracing::{debug, info, warn};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;

use crate::{
    actions::Actions,
    dock::{Click, Dock},
    launcher::{Launcher, Outcome},
    panel::{Button, Panel},
    quick_settings::QuickSettings,
    status::Update,
    workspaces::Workspaces,
};

/// The latest of everything the status threads report.
#[derive(Debug, Default, Clone)]
pub struct Status {
    pub battery: Option<status::Battery>,
    pub network: status::Network,
    pub wifi_enabled: Option<bool>,
    pub volume: Option<status::Volume>,
    /// What quick settings shows: the daemon's, else the custom command's.
    pub power_profiles: Option<status::PowerProfiles>,
    /// What the power-modes daemon reported, if one is running.
    pub daemon_profiles: Option<status::PowerProfiles>,
    /// `power.profile_command`, empty if unset.
    pub profile_command: String,
    /// The mode last chosen through the command (balanced until then).
    pub custom_profile: Option<status::PowerProfile>,
    pub bluetooth: Option<status::Bluetooth>,
    /// Read when quick settings opens; `None` on desktops.
    pub backlight: Option<status::Backlight>,
}

impl Status {
    /// Settles `power_profiles`: the daemon's, else all three modes if a
    /// command is set.
    pub fn sync_profiles(&mut self) {
        self.power_profiles = self.daemon_profiles.clone().or_else(|| {
            (!self.profile_command.is_empty()).then(|| status::PowerProfiles {
                active: self
                    .custom_profile
                    .unwrap_or(status::PowerProfile::Balanced),
                available: status::PowerProfile::ALL.to_vec(),
            })
        });
    }

    pub fn set_profile_command(&mut self, command: &str) {
        self.profile_command = command.trim().to_string();
        self.sync_profiles();
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Battery(battery) => self.battery = battery,
            Update::Network(network) => self.network = network,
            Update::WifiEnabled(enabled) => self.wifi_enabled = enabled,
            Update::Volume(volume) => self.volume = volume,
            Update::PowerProfiles(profiles) => {
                self.daemon_profiles = profiles;
                self.sync_profiles();
            }
            Update::CustomProfile(profile) => {
                self.custom_profile = Some(profile);
                self.sync_profiles();
            }
            Update::Bluetooth(bluetooth) => self.bluetooth = bluetooth,
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
    /// The calendar under the clock, while open.
    calendar: Option<calendar::Calendar>,
    /// As `quick_settings_dismissed`, for the calendar.
    calendar_dismissed: Option<std::time::Instant>,
    seat_handle: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    loop_handle: LoopHandle<'static, App>,

    ipc: ipc::Ipc,
    /// The overview's search box, while the overview is open.
    launcher: Option<Launcher>,
    dock: Option<Dock>,
    /// The open right-click menu.
    menu: Option<menu::Menu>,
    /// The desktop menu's anchor surface, waiting to be configured, and the
    /// scale of its monitor.
    desktop_holder: Option<(
        smithay_client_toolkit::shell::wlr_layer::LayerSurface,
        i32,
        u32,
    )>,
    /// The dock is being edited in place (kept across it being recreated,
    /// e.g. when it moves to another edge).
    dock_editing: bool,
    /// Where the dock's move grip was pressed.
    dock_drag_start: Option<(f64, f64)>,
    /// A left press on the dock that's a click or a drag, not yet known
    /// which (see `dock_press_pointer`).
    dock_press: Option<(f64, f64)>,
    /// Where a dragged dock would land.
    dock_preview: Option<edit_mode::DockPreview>,
    /// The latest input serial, for grabs the compositor started for us.
    last_serial: u32,
    /// The screenshot tool, while the screen is frozen for one.
    screenshot: Option<screenshot::ScreenshotUi>,
    /// Every open window, from the compositor.
    windows: Vec<cnde_ipc::WindowInfo>,
    /// Which monitor edges windows touch (bars there go solid).
    edges: Vec<cnde_ipc::OutputEdges>,
    /// The colour the adaptive palette is worked out from.
    screen: Option<cnde_ui::Color>,
    /// Where an overview dock goes (the overview's monitor).
    dock_output: Option<wl_output::WlOutput>,
    overview_open: bool,
    /// Whether we last told the compositor the app grid is showing.
    reported_grid: bool,
    /// When quick settings was last dismissed by the compositor (a click
    /// outside it). A click on the status icons right after is that same
    /// click, meant to close it, not to reopen it.
    quick_settings_dismissed: Option<std::time::Instant>,
    apps: Vec<apps::App>,
    app_dirs: Vec<PathBuf>,
    /// When `app_dirs` last changed, as of the last scan.
    apps_stamp: Vec<Option<SystemTime>>,
    apps_loading: bool,
    apps_tx: channel::Sender<(Vec<apps::App>, Vec<Option<SystemTime>>)>,
    launches: apps::LaunchCounts,
    /// The volume or brightness level, shown after its key.
    osd: Option<osd::Osd>,
    /// The polkit agent's password dialog.
    polkit: polkit::Polkit,
    /// The low-battery warning has been shown for this discharge.
    low_battery_warned: bool,
}

/// When `main` began, for the startup timing logs.
pub(crate) static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let started = *START.get_or_init(std::time::Instant::now);
    // Config first (it names the fonts), then the font scan, the slowest
    // startup step, runs alongside the Wayland setup.
    let config = config::load();
    let theme = config::theme_from(&config, None);
    let families = theme.font_families;
    let fonts_thread = std::thread::Builder::new()
        .name("fonts".into())
        .spawn(move || {
            let begun = std::time::Instant::now();
            let fonts = Fonts::new(families);
            debug!(elapsed = ?begun.elapsed(), "fonts loaded");
            fonts
        })?;
    debug!(elapsed = ?started.elapsed(), "config loaded");

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    debug!(elapsed = ?started.elapsed(), "wayland connected");

    cnde_ui::anim::set_speed(config.animations.speed as f32);
    let mut icons = Icons::new();
    icons.set_theme(&config.appearance.icon_theme);
    let fonts = fonts_thread
        .join()
        .map_err(|_| "the font loader panicked")?;
    debug!(elapsed = ?started.elapsed(), "fonts ready");
    info!(font = fonts.family(), "cnde-shell starting");

    let shm = Shm::bind(&globals, &qh)?;

    // Overview open/close from the compositor.
    let (ipc_tx, ipc_rx) = channel::channel();
    event_loop.handle().insert_source(ipc_rx, |event, _, app| {
        if let Event::Msg(event) = event {
            app.ipc_event(event);
        }
    })?;
    // The installed apps, scanned off the main thread.
    let (apps_tx, apps_rx) = channel::channel();
    event_loop
        .handle()
        .insert_source(apps_rx, |event, _, app: &mut App| {
            if let Event::Msg((list, stamp)) = event {
                app.apps = list;
                app.apps_stamp = stamp;
                app.apps_loading = false;
                if let Some(launcher) = &mut app.launcher {
                    launcher.refresh(&app.apps, &app.launches, &app.theme);
                }
                app.draw_launcher();
                app.sync_dock();
            }
        })?;

    let (status_tx, status_rx) = channel::channel();
    let profile_command = config.power.profile_command.clone();
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
        icons,
        status: {
            let mut status = Status::default();
            status.set_profile_command(&profile_command);
            status
        },
        panels: Vec::new(),
        workspaces: Workspaces::default(),
        actions: Actions::start(status_tx.clone()),
        quick_settings: None,
        calendar: None,
        calendar_dismissed: None,
        seat_handle: None,
        pointer: None,
        keyboard: None,
        modifiers: Modifiers::default(),
        loop_handle: event_loop.handle(),
        ipc: ipc::Ipc::connect(ipc_tx),
        launcher: None,
        dock: None,
        screenshot: None,
        menu: None,
        desktop_holder: None,
        last_serial: 0,
        dock_editing: false,
        dock_drag_start: None,
        dock_press: None,
        dock_preview: None,
        windows: Vec::new(),
        edges: Vec::new(),
        screen: None,
        dock_output: None,
        overview_open: false,
        reported_grid: false,
        quick_settings_dismissed: None,
        apps: Vec::new(),
        app_dirs: apps::application_dirs(),
        apps_stamp: Vec::new(),
        apps_loading: false,
        apps_tx,
        launches: apps::LaunchCounts::load(),
        osd: None,
        polkit: polkit::Polkit::default(),
        low_battery_warned: false,
    };
    app.rescan_apps_if_changed();
    app.workspaces.manager = globals
        .bind::<ExtWorkspaceManagerV1, _, _>(&qh, 1..=1, workspaces::ManagerData)
        .map_err(|err| warn!(?err, "no ext-workspace; the indicator stays empty"))
        .ok();

    // One roundtrip so wl_shm has listed its formats before we draw.
    queue.roundtrip(&mut app)?;
    app.choose_shm_format();
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;

    config::watch(&event_loop.handle());

    // Battery, network and volume, each from its own background thread.
    event_loop
        .handle()
        .insert_source(status_rx, |event, _, app| {
            if let Event::Msg(update) = event {
                app.status.apply(update);
                app.check_low_battery();
                if let Some((qs, _)) = &mut app.quick_settings {
                    qs.status_changed(&app.xdg_shell, &app.theme, &app.status);
                }
                app.redraw_all();
            }
        })?;
    status::start(status_tx);
    app.polkit = polkit::start(&event_loop.handle());

    // The clock: wake exactly when the minute changes, not every second.
    event_loop
        .handle()
        .insert_source(Timer::from_duration(until_next_minute()), |_, _, app| {
            app.redraw_all();
            TimeoutAction::ToDuration(until_next_minute())
        })?;

    // Debug aid: CNDE_SHELL_OPEN=quick_settings or calendar opens that
    // popup on the first panel, for looking at it without a click.
    if let Ok(which) = std::env::var("CNDE_SHELL_OPEN") {
        event_loop.handle().insert_source(
            Timer::from_duration(Duration::from_secs(2)),
            move |_, _, app| {
                let serial = app.last_serial;
                match which.as_str() {
                    "calendar" => app.toggle_calendar(0, serial),
                    _ => app.toggle_quick_settings(0, serial),
                }
                TimeoutAction::Drop
            },
        )?;
    }
    // Debug aid: CNDE_SHELL_EDIT=dock,panel starts in edit mode, for
    // looking at it without a right-click.
    if let Ok(which) = std::env::var("CNDE_SHELL_EDIT") {
        event_loop.handle().insert_source(
            Timer::from_duration(Duration::from_secs(2)),
            move |_, _, app| {
                if which.contains("dock") {
                    app.start_dock_edit();
                }
                if which.contains("panel") {
                    app.start_panel_edit();
                }
                TimeoutAction::Drop
            },
        )?;
    }

    loop {
        event_loop.dispatch(None, &mut app)?;
        if let Some(err) = connection_lost(&conn) {
            return Err(err.into());
        }
    }
}

/// Why the Wayland connection is dead, if it is. After a protocol error, or
/// with the compositor gone, the socket stays readable and the event loop
/// would spin instead of exiting (the compositor restarts the shell).
fn connection_lost(conn: &Connection) -> Option<String> {
    if let Some(err) = conn.protocol_error() {
        return Some(format!("the compositor closed our connection: {err}"));
    }
    match conn.flush() {
        Err(WaylandError::Io(io)) if io.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(err) => Some(format!("lost the compositor: {err}")),
        Ok(()) => None,
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
    fn ipc_event(&mut self, event: cnde_ipc::Event) {
        match event {
            cnde_ipc::Event::DesktopMenu {
                output,
                x,
                y,
                serial,
            } => self.desktop_menu(&output, x, y, serial),
            cnde_ipc::Event::OverviewOpened { output } => {
                self.overview_open = true;
                self.redraw_panels();
                self.rescan_apps_if_changed();
                let found = self.outputs.outputs().find_map(|o| {
                    let info = self.outputs.info(&o)?;
                    let size = info.logical_size.unwrap_or((1920, 1080));
                    (info.name.as_deref() == Some(output.as_str())).then_some((
                        o,
                        info.scale_factor,
                        (size.0 as f32, size.1 as f32),
                    ))
                });
                let Some((output, scale, size)) = found else {
                    warn!(%output, "overview opened on an unknown monitor");
                    return;
                };
                if self.config.dock.position == cnde_config::DockPosition::Overview {
                    self.dock_output = Some(output.clone());
                    self.sync_dock();
                }
                if self.launcher.as_ref().is_some_and(|l| l.output == output) {
                    return;
                }
                self.launcher = Some(Launcher::new(
                    &self.compositor,
                    &self.layer_shell,
                    &self.shm,
                    &self.qh,
                    output,
                    size,
                    &self.theme,
                    scale,
                ));
            }
            cnde_ipc::Event::MediaKey { key } => self.media_key(key),
            cnde_ipc::Event::ToggleAppGrid => {
                if let Some(launcher) = &mut self.launcher {
                    launcher.toggle_grid(&self.theme, self.apps.len());
                }
                self.draw_launcher();
            }
            cnde_ipc::Event::OverviewClosed => {
                self.overview_open = false;
                self.launcher = None;
                self.reported_grid = false;
                self.redraw_panels();
                self.sync_dock();
            }
            cnde_ipc::Event::Windows { windows } => {
                self.windows = windows;
                self.sync_dock();
            }
            cnde_ipc::Event::ScreenshotUi { windows } => self.open_screenshot_ui(windows),
            cnde_ipc::Event::Edges { outputs } => {
                self.edges = outputs;
                // The overview hides which window is in front; keep its
                // colour meanwhile.
                let screen = self.screen_colour();
                if !self.overview_open && screen != self.screen {
                    self.screen = screen;
                    self.apply_theme();
                }
                self.redraw_panels();
                self.sync_dock();
            }
            cnde_ipc::Event::ScreenshotDone { path } => {
                self.screenshot = None;
                if let Some(path) = path {
                    info!(%path, "screenshot saved");
                }
            }
        }
    }

    fn open_screenshot_ui(&mut self, windows: Vec<cnde_ipc::ScreenshotWindow>) {
        self.quick_settings = None;
        self.calendar = None;
        let main = self.main_output();
        let monitors = self
            .outputs
            .outputs()
            .filter_map(|o| {
                let info = self.outputs.info(&o)?;
                Some(screenshot::Monitor {
                    main: main.as_ref() == Some(&o),
                    name: info.name.clone().unwrap_or_default(),
                    origin: info.logical_position.unwrap_or((0, 0)),
                    size: info.logical_size.unwrap_or((1920, 1080)),
                    scale: info.scale_factor,
                    output: o,
                })
            })
            .collect();
        self.screenshot = Some(screenshot::ScreenshotUi::open(
            &self.compositor,
            &self.layer_shell,
            &self.shm,
            &self.qh,
            monitors,
            windows,
        ));
    }

    fn screenshot_outcome(&mut self, outcome: screenshot::Outcome) {
        match outcome {
            screenshot::Outcome::Nothing => {}
            screenshot::Outcome::Redraw => {
                if let Some(ui) = &mut self.screenshot {
                    ui.draw_all(
                        &self.qh,
                        self.shm_format,
                        &mut self.fonts,
                        &mut self.icons,
                        &self.theme,
                    );
                }
            }
            screenshot::Outcome::Cancel => {
                self.screenshot = None;
                self.ipc.send(&cnde_ipc::Request::CancelScreenshot);
            }
            screenshot::Outcome::Take(target) => {
                // Gone at once: the shot is cut from the frozen screen,
                // which never had this tool on it.
                self.screenshot = None;
                self.ipc.send(&cnde_ipc::Request::Screenshot { target });
            }
        }
    }

    /// The main monitor: configured, else the leftmost.
    fn main_output(&self) -> Option<wl_output::WlOutput> {
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
        outputs
            .iter()
            .find(|(_, name, _)| self.config.output(name).is_some_and(|c| c.main))
            .or_else(|| outputs.iter().min_by_key(|(_, _, x)| *x))
            .map(|(o, _, _)| o.clone())
    }

    /// Rescan the installed apps in the background if anything changed.
    fn rescan_apps_if_changed(&mut self) {
        let stamp = apps::dirs_stamp(&self.app_dirs);
        if self.apps_loading || (stamp == self.apps_stamp && !self.apps.is_empty()) {
            return;
        }
        self.apps_loading = true;
        let dirs = self.app_dirs.clone();
        let tx = self.apps_tx.clone();
        let _ = std::thread::Builder::new()
            .name("apps".into())
            .spawn(move || {
                let list = apps::load(&dirs);
                let _ = tx.send((list, stamp));
            });
    }

    fn draw_launcher(&mut self) {
        // Keep the compositor up to date on the grid (Super's next step).
        let grid = self
            .launcher
            .as_ref()
            .is_some_and(|l| l.mode() == launcher::Mode::Grid);
        if grid != self.reported_grid {
            self.reported_grid = grid;
            self.ipc
                .send(&cnde_ipc::Request::AppGridShown { shown: grid });
        }
        let reserve = self.launcher_reserve();
        if let Some(launcher) = &mut self.launcher {
            launcher.bottom_reserve = reserve;
            launcher.draw(
                &self.qh,
                &self.compositor,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &self.theme,
                &self.apps,
            );
        }
    }

    /// How much of the bottom of the launcher's monitor a dock takes up,
    /// so the app grid (and its page dots) stop above it.
    fn launcher_reserve(&self) -> f32 {
        use cnde_config::DockPosition;
        let Some(launcher) = &self.launcher else {
            return 0.0;
        };
        let margin = match self.config.dock.position {
            DockPosition::Overview => 20.0,
            DockPosition::Bottom => 8.0,
            _ => return 0.0,
        };
        let height = self
            .dock
            .as_ref()
            .filter(|d| d.output == launcher.output && d.size().1 > 0)
            .map_or(
                self.config.dock.icon_size as f32 + 2.0 * self.config.dock.padding + 12.0,
                |d| d.size().1 as f32,
            );
        height + margin + 16.0
    }

    fn launcher_key(&mut self, event: KeyEvent) {
        let Some(launcher) = &mut self.launcher else {
            return;
        };
        match launcher.key(
            &event,
            &self.modifiers,
            &self.apps,
            &self.launches,
            &self.theme,
        ) {
            Outcome::Nothing => {}
            Outcome::Redraw => self.draw_launcher(),
            Outcome::Close => self.close_overview(),
            Outcome::Launch(id) => self.launch(&id),
        }
    }

    fn launch(&mut self, id: &str) {
        if let Some(app) = self.apps.iter().find(|a| a.id == id) {
            let terminal = std::env::var("TERMINAL").unwrap_or_else(|_| "alacritty".into());
            apps::launch(app, &terminal);
            self.launches.record(id);
            // Something to show it worked: a busy pointer, and the dock
            // icon bouncing until the window opens.
            self.ipc.send(&cnde_ipc::Request::Launching);
            self.dock_launching(id);
        }
        self.close_overview();
    }

    /// `id` was just launched: its dock icon hops until a window opens.
    fn dock_launching(&mut self, id: &str) {
        if let Some(dock) = &mut self.dock {
            dock.launching.retain(|(other, _)| other != id);
            dock.launching
                .push((id.to_string(), std::time::Instant::now()));
        }
        self.draw_dock();
    }

    fn close_overview(&mut self) {
        self.ipc
            .send(&cnde_ipc::Request::SetOverview { open: false });
    }

    /// Which monitors get a panel: the main one (configured, else the
    /// leftmost), every one with `panel.all_monitors`, or none with
    /// `panel.enabled = false`.
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
        if self.config.dock.position != cnde_config::DockPosition::Overview {
            self.dock_output = main.clone();
        }
        let wanted: Vec<wl_output::WlOutput> = if !self.config.panel.enabled {
            // Only the bar goes: the overview, dock and screenshot tool stay.
            Vec::new()
        } else if self.config.panel.all_monitors {
            outputs.into_iter().map(|(o, _, _)| o).collect()
        } else {
            main.into_iter().collect()
        };

        if self.panels.iter().any(|p| !wanted.contains(&p.output)) {
            // Quick settings hangs from a panel; it can't outlive it.
            self.quick_settings = None;
            self.calendar = None;
        }
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
        self.sync_dock();
    }

    /// Create, move or drop the dock to match the config (and, for an
    /// overview dock, whether the overview is open), then refresh it.
    fn sync_dock(&mut self) {
        use cnde_config::DockPosition;
        let position = self.config.dock.position;
        let wanted = match position {
            DockPosition::Off => None,
            DockPosition::Overview if !self.overview_open => None,
            _ => self.dock_output.clone(),
        };
        if self.dock.as_ref().is_some_and(|d| {
            Some(&d.output) != wanted.as_ref()
                || d.position() != position
                || d.placement()
                    != (
                        self.config.dock.extend && position != DockPosition::Overview,
                        self.config.dock.align,
                        self.config.dock.float_align.unwrap_or(self.config.dock.align),
                        self.config.dock.reserve,
                    )
        }) {
            self.dock = None;
        }
        let Some(output) = wanted else {
            self.dock = None;
            // Editing ends with the dock (the overview closing).
            self.dock_editing = false;
            return;
        };
        if self.dock.is_none() {
            let scale = self
                .outputs
                .info(&output)
                .map_or(1, |info| info.scale_factor);
            self.dock = Some(Dock::new(
                &self.compositor,
                &self.layer_shell,
                &self.qh,
                output,
                position,
                self.config.dock.extend,
                self.config.dock.align,
                self.config.dock.float_align.unwrap_or(self.config.dock.align),
                self.config.dock.reserve,
                scale,
            ));
        }
        let items = dock::items(&self.config.dock.favorites, &self.apps, &self.windows);
        let icon = self.config.dock.icon_size as f32;
        // A window against the dock's edge makes it solid.
        let touched = self.dock.as_ref().is_some_and(|dock| {
            self.edges_of(&dock.output)
                .is_some_and(|e| match dock.position() {
                    cnde_config::DockPosition::Left => e.left,
                    cnde_config::DockPosition::Right => e.right,
                    _ => e.bottom,
                })
        });
        // The colour to take (`dock.adaptive_color`). Only a bottom dock
        // has a window's bottom edge colour to follow.
        let tint = self.dock.as_ref().and_then(|dock| {
            let e = self.edges_of(&dock.output)?;
            let side = matches!(
                dock.position(),
                cnde_config::DockPosition::Left | cnde_config::DockPosition::Right
            );
            let colour = |c: Option<[u8; 3]>| c.map(|[r, g, b]| cnde_ui::Color::rgb(r, g, b));
            cnde_ui::tint::dock_choice(
                self.config.dock.adaptive_color,
                colour(e.top_tint),
                colour(e.bottom_tint.filter(|_| !side)),
                colour(e.wallpaper_tint),
            )
        });
        if let Some(dock) = &mut self.dock {
            if dock.edit.is_some() != self.dock_editing {
                dock.edit = self.dock_editing.then(dock::DockEdit::default);
            }
            dock.frosted = dock::frosted(self.config.dock.background);
            dock.solid = touched;
            dock.tint = tint;
            dock.autohide = self.config.dock.visibility != cnde_config::DockVisibility::Always;
            // Windows aren't judged while the overview's open (it holds the
            // dock out anyway); keep what they were.
            if !self.overview_open {
                dock.touched = touched;
            }
            dock.pinned = self.overview_open || self.dock_editing || self.menu.is_some();
            dock.mode = self.config.dock.mode;
            dock.edge_length = self
                .outputs
                .info(&dock.output)
                .and_then(|info| info.logical_size)
                .map_or(0, |(w, h)| match dock.position() {
                    cnde_config::DockPosition::Left | cnde_config::DockPosition::Right => h,
                    _ => w,
                })
                .max(0) as u32;
            dock.pad = self.config.dock.padding;
            dock.launcher = dock::LauncherPlace {
                first: self.config.dock.launcher == cnde_config::LauncherSide::Start,
                gap: self.config.dock.launcher_gap,
            };
            dock.update(items, icon);
        }
        self.draw_dock();
        // A dock that grew (bigger icons, edit mode) pushes the grid up.
        if self
            .launcher
            .as_ref()
            .is_some_and(|l| l.bottom_reserve != self.launcher_reserve())
        {
            self.draw_launcher();
        }
    }

    /// Refresh the dock once an autohide dock's linger is over, so it can
    /// slide away without anything else happening.
    pub(crate) fn sync_dock_later(&self) {
        if self.config.dock.visibility == cnde_config::DockVisibility::Always {
            return;
        }
        let _ = self.loop_handle.insert_source(
            Timer::from_duration(dock::HIDE_DELAY + Duration::from_millis(30)),
            |_, _, app| {
                app.sync_dock();
                TimeoutAction::Drop
            },
        );
    }

    /// Act on a left or middle click on the dock.
    fn dock_click(&mut self, click: Click) {
        match click {
            Click::Nothing => {}
            Click::Launch(id) => self.launch(&id),
            Click::Activate(id) => {
                self.ipc.send(&cnde_ipc::Request::ActivateWindow { id });
            }
            Click::AppGrid => self.ipc.send(&cnde_ipc::Request::ShowAppGrid),
        }
    }

    fn draw_dock(&mut self) {
        if let Some(dock) = &mut self.dock {
            dock.draw(
                &self.qh,
                &mut self.pool,
                self.shm_format,
                &mut self.icons,
                &self.theme,
                self.config.dock.icon_size as f32,
            );
        }
    }

    pub fn redraw_all(&mut self) {
        self.redraw_panels();
        self.draw_quick_settings();
        // Today moves at midnight.
        self.draw_calendar();
    }

    /// The colour for the adaptive palette, from the main monitor first:
    /// the window against its top, else its wallpaper.
    fn screen_colour(&self) -> Option<cnde_ui::Color> {
        let main = self.main_output().and_then(|o| self.edges_of(&o).cloned());
        let all = || main.iter().chain(&self.edges);
        cnde_ui::tint::screen(
            all().find_map(|e| e.top_tint),
            all().find_map(|e| e.wallpaper_tint),
        )
    }

    /// What the compositor last said touches `output`'s edges.
    fn edges_of(&self, output: &wl_output::WlOutput) -> Option<&cnde_ipc::OutputEdges> {
        let name = self.outputs.info(output)?.name?;
        self.edges.iter().find(|e| e.output == name)
    }

    fn redraw_panels(&mut self) {
        for i in 0..self.panels.len() {
            self.draw_panel(i);
        }
    }

    fn draw_quick_settings(&mut self) {
        let theme = self.popup_theme();
        if let Some((qs, _)) = &mut self.quick_settings {
            qs.draw(
                &self.qh,
                &mut self.pool,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &theme,
                &self.status,
            );
        }
    }

    fn draw_calendar(&mut self) {
        let theme = self.popup_theme();
        if let Some(calendar) = &mut self.calendar {
            calendar.draw(
                &self.qh,
                &mut self.pool,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &theme,
            );
        }
    }

    /// The theme popups (quick settings, the calendar, menus) draw in: the
    /// bar's current colour, with their own translucency.
    pub fn popup_theme(&self) -> Theme {
        match self.panels.iter().find_map(|p| p.shown) {
            Some(shown) => shown.apply_translucent(&self.theme),
            None => self.theme.clone(),
        }
    }

    /// Clicking the clock opens the calendar, or closes it.
    fn toggle_calendar(&mut self, panel_index: usize, serial: u32) {
        let just_dismissed = self
            .calendar_dismissed
            .take()
            .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
        if self.calendar.take().is_some() || just_dismissed {
            self.redraw_panels();
            return;
        }
        let (Some(panel), Some(seat)) = (self.panels.get(panel_index), &self.seat_handle) else {
            return;
        };
        let Some(anchor) = panel.clock_anchor() else {
            return;
        };
        // Before the new grab, or the compositor disconnects us.
        self.menu = None;
        self.quick_settings = None;
        self.calendar = calendar::Calendar::open(
            &self.compositor,
            &self.xdg_shell,
            &self.qh,
            &panel.layer,
            seat,
            serial,
            anchor,
            panel.scale(),
        );
        self.redraw_panels();
    }

    fn calendar_pointer(&mut self, event: &PointerEvent) {
        let Some(calendar) = &mut self.calendar else {
            return;
        };
        let redraw = match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                calendar.motion(event.position)
            }
            PointerEventKind::Leave { .. } => calendar.motion((-1.0, -1.0)),
            PointerEventKind::Press {
                button: BTN_LEFT, ..
            } => calendar.press(event.position),
            PointerEventKind::Axis {
                vertical, ..
            } => {
                let steps = if vertical.discrete != 0 {
                    vertical.discrete
                } else if vertical.value120 != 0 {
                    vertical.value120 / 120
                } else {
                    // Touchpads: a page per decent swipe.
                    (vertical.absolute / 40.0) as i32
                };
                calendar.scroll(steps)
            }
            _ => false,
        };
        if redraw {
            self.draw_calendar();
        }
    }

    /// Clicking the status area opens quick settings, or closes it.
    fn toggle_quick_settings(&mut self, panel_index: usize, serial: u32) {
        let just_dismissed = self
            .quick_settings_dismissed
            .take()
            .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
        if self.quick_settings.take().is_some() || just_dismissed {
            self.redraw_panels();
            return;
        }
        let (Some(panel), Some(seat)) = (self.panels.get(panel_index), &self.seat_handle) else {
            return;
        };
        let Some(anchor) = panel.status_anchor() else {
            return;
        };
        // Brightness can change behind our back (keys, other tools).
        self.status.backlight = status::read_backlight();
        // Before the new grab, or the compositor disconnects us.
        self.menu = None;
        self.calendar = None;
        self.quick_settings = QuickSettings::open(
            &self.compositor,
            &self.xdg_shell,
            &self.qh,
            &panel.layer,
            seat,
            serial,
            anchor,
            &self.theme,
            &self.status,
            panel.scale(),
        )
        .map(|qs| (qs, anchor));
        self.redraw_panels();
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
            self.redraw_panels();
        } else {
            self.draw_quick_settings();
            // Redraw once a pressed toggle's wait is over, so a refused
            // change visibly snaps back (and again while a Bluetooth
            // device is still connecting).
            if matches!(event.kind, PointerEventKind::Press { .. }) {
                let wait = quick_settings::CONFIRM_WITHIN + Duration::from_millis(50);
                let _ = self
                    .loop_handle
                    .insert_source(Timer::from_duration(wait), |_, _, app| {
                        app.draw_quick_settings();
                        match &app.quick_settings {
                            Some((qs, _)) if qs.waiting() => {
                                TimeoutAction::ToDuration(Duration::from_secs(1))
                            }
                            _ => TimeoutAction::Drop,
                        }
                    });
            }
        }
    }

    fn draw_panel(&mut self, index: usize) {
        let solid = self
            .panels
            .get(index)
            .and_then(|p| self.edges_of(&p.output))
            .is_some_and(|e| e.top);
        let backdrop_light = self
            .panels
            .get(index)
            .and_then(|p| self.edges_of(&p.output))
            .is_some_and(|e| e.top_light);
        let tint = self
            .panels
            .get(index)
            .and_then(|p| self.edges_of(&p.output))
            .and_then(|e| {
                let colour = |c: Option<[u8; 3]>| c.map(|[r, g, b]| cnde_ui::Color::rgb(r, g, b));
                cnde_ui::tint::bar_choice(colour(e.top_tint))
            });
        let Some(panel) = self.panels.get_mut(index) else {
            return;
        };
        let was = panel.shown;
        panel.draw(
            &mut self.pool,
            self.shm_format,
            panel::Draw {
                qh: &self.qh,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: &self.theme,
                clock_format: &self.config.panel.clock_format,
                workspaces: &self.workspaces.list,
                status: &self.status,
                overview_open: self.overview_open,
                quick_settings_open: self.quick_settings.is_some(),
                calendar_open: self.calendar.is_some(),
                clock_position: self.config.panel.clock_position,
                battery_percent: self.config.power.battery_percent,
                battery_low: self
                    .status
                    .battery
                    .is_some_and(|b| b.is_low(self.config.power.low_battery)),
                solid,
                mode: self.config.panel.mode,
                clear: self.config.panel.clear,
                backdrop_light,
                tint,
            },
        );
        // Popups wear the bar's colour, so they follow it.
        if self.panels[index].shown != was {
            self.draw_quick_settings();
            self.draw_calendar();
            self.draw_menu();
        }
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
        if let Some(calendar) = &mut self.calendar
            && calendar.popup.wl_surface() == surface
        {
            calendar.set_scale(factor);
            self.draw_calendar();
        }
        if let Some(launcher) = &mut self.launcher
            && launcher.layer.wl_surface() == surface
        {
            launcher.set_scale(factor);
            self.draw_launcher();
        }
        if let Some(menu) = &mut self.menu
            && menu.popup.wl_surface() == surface
        {
            menu.set_scale(factor);
            self.draw_menu();
        }
        if let Some(dock) = &mut self.dock
            && dock.layer.wl_surface() == surface
        {
            dock.set_scale(factor);
            self.draw_dock();
        }
        if let Some(ui) = &mut self.screenshot
            && ui.set_scale(surface, factor)
        {
            ui.draw_surface(
                surface,
                &self.qh,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &self.theme,
            );
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
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(i) = self
            .panels
            .iter()
            .position(|p| p.layer.wl_surface() == surface)
            && self.panels[i].frame_done()
        {
            self.draw_panel(i);
        }
        if let Some(menu) = &mut self.menu
            && menu.popup.wl_surface() == surface
            && menu.frame_done()
        {
            self.draw_menu();
        }
        if let Some((qs, _)) = &mut self.quick_settings
            && qs.popup.wl_surface() == surface
            && qs.frame_done()
        {
            self.draw_quick_settings();
        }
        if let Some(calendar) = &mut self.calendar
            && calendar.popup.wl_surface() == surface
            && calendar.frame_done()
        {
            self.draw_calendar();
        }
        if let Some(launcher) = &mut self.launcher
            && launcher.layer.wl_surface() == surface
            && launcher.frame_done()
        {
            self.draw_launcher();
        }
        if let Some(dock) = &mut self.dock
            && dock.layer.wl_surface() == surface
            && dock.frame_done()
        {
            self.draw_dock();
        }
        if let Some(ui) = &mut self.screenshot
            && ui.frame_done(surface)
        {
            ui.draw_surface(
                surface,
                &self.qh,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &self.theme,
            );
        }
    }
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
        if self.launcher.as_ref().is_some_and(|l| &l.layer == layer) {
            self.launcher = None;
        }
        if self.dock.as_ref().is_some_and(|d| &d.layer == layer) {
            self.dock = None;
        }
        if self.osd.as_ref().is_some_and(|o| &o.layer == layer) {
            self.osd = None;
        }
        self.polkit_closed(layer.wl_surface());
        if self
            .screenshot
            .as_ref()
            .is_some_and(|ui| ui.owns(layer.wl_surface()))
        {
            self.screenshot = None;
            self.ipc.send(&cnde_ipc::Request::CancelScreenshot);
        }
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
            self.panels[i].update_input_region(&self.compositor);
            self.draw_panel(i);
        }
        if self
            .desktop_holder
            .as_ref()
            .is_some_and(|(holder, _, _)| holder == layer)
        {
            self.desktop_holder_configured();
        }
        self.polkit_configured(layer.wl_surface());
        if self.osd.as_ref().is_some_and(|o| &o.layer == layer) {
            self.osd_configured();
        }
        if self
            .dock_preview
            .as_ref()
            .is_some_and(|p| &p.layer == layer)
        {
            self.dock_preview_configured(configure.new_size);
        }
        if let Some(launcher) = &mut self.launcher
            && &launcher.layer == layer
        {
            launcher.configured();
            self.draw_launcher();
        }
        if let Some(dock) = &mut self.dock
            && &dock.layer == layer
        {
            dock.configured(configure.new_size);
            self.draw_dock();
        }
        if let Some(ui) = &mut self.screenshot
            && ui.configured(layer.wl_surface(), configure.new_size)
        {
            ui.draw_surface(
                layer.wl_surface(),
                &self.qh,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &self.theme,
            );
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
            // With repeat, so holding Backspace in the search keeps deleting.
            self.keyboard = self
                .seat
                .get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.loop_handle.clone(),
                    Box::new(|app, _, event| app.launcher_key(event)),
                )
                .ok();
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
            if let PointerEventKind::Enter { serial }
            | PointerEventKind::Press { serial, .. }
            | PointerEventKind::Release { serial, .. } = event.kind
            {
                self.last_serial = serial;
            }
            if self
                .polkit
                .dialog
                .as_ref()
                .is_some_and(|d| d.owns(&event.surface))
            {
                match event.kind {
                    PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                        self.polkit_pointer(event.position, false)
                    }
                    PointerEventKind::Press {
                        button: BTN_LEFT, ..
                    } => self.polkit_pointer(event.position, true),
                    _ => {}
                }
                continue;
            }
            if let Some(menu) = &mut self.menu
                && menu.popup.wl_surface() == &event.surface
            {
                match event.kind {
                    PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                        if menu.motion(Some(event.position)) {
                            self.draw_menu();
                        }
                    }
                    PointerEventKind::Leave { .. } => {
                        if menu.motion(None) {
                            self.draw_menu();
                        }
                    }
                    PointerEventKind::Press {
                        button: BTN_LEFT, ..
                    } => {
                        if let Some(action) = menu.click(event.position) {
                            self.menu_action(action);
                        }
                    }
                    PointerEventKind::Release {
                        button: BTN_LEFT | BTN_RIGHT,
                        ..
                    } if menu.release_counts() => {
                        if let Some(action) = menu.click(event.position) {
                            self.menu_action(action);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(ui) = &mut self.screenshot
                && ui.owns(&event.surface)
            {
                let outcome = match event.kind {
                    PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } => {
                        ui.motion(&event.surface, event.position)
                    }
                    PointerEventKind::Press {
                        button: BTN_LEFT, ..
                    } => ui.press(&event.surface, event.position),
                    PointerEventKind::Release {
                        button: BTN_LEFT, ..
                    } => ui.release(),
                    _ => screenshot::Outcome::Nothing,
                };
                self.screenshot_outcome(outcome);
                continue;
            }
            if let Some(dock) = &mut self.dock
                && dock.layer.wl_surface() == &event.surface
            {
                match event.kind {
                    PointerEventKind::Enter { .. } => dock.pointer(true),
                    PointerEventKind::Leave { .. } => {
                        dock.pointer(false);
                        self.sync_dock_later();
                    }
                    _ => {}
                }
                // A press elsewhere on the dock closes the open menu and goes
                // no further; a right-click then opens its own menu below.
                if let PointerEventKind::Press { button, .. } = event.kind
                    && self.menu.is_some()
                    && press_closes_menu(button)
                {
                    self.close_menu();
                    continue;
                }
                let Some(dock) = &mut self.dock else {
                    continue;
                };
                let right_click = matches!(
                    event.kind,
                    PointerEventKind::Press {
                        button: BTN_RIGHT,
                        ..
                    }
                );
                if (dock.edit.is_some() || dock.drag.is_some()) && !right_click {
                    self.dock_edit_pointer(event);
                    continue;
                }
                if self.dock_press_pointer(event) {
                    continue;
                }
                let Some(dock) = &mut self.dock else {
                    continue;
                };
                let icon = self.config.dock.icon_size as f32;
                let click = match event.kind {
                    PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                        if dock.hover(Some(event.position), icon) {
                            self.draw_dock();
                        }
                        continue;
                    }
                    PointerEventKind::Leave { .. } => {
                        if dock.hover(None, icon) {
                            self.draw_dock();
                        }
                        continue;
                    }
                    PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                        dock.click(event.position, icon, false)
                    }
                    PointerEventKind::Press { button, .. } if button == BTN_MIDDLE => {
                        dock.click(event.position, icon, true)
                    }
                    PointerEventKind::Press {
                        button: BTN_RIGHT,
                        serial,
                        ..
                    } => {
                        self.dock_menu(event.position, serial);
                        continue;
                    }
                    _ => continue,
                };
                self.dock_click(click);
                continue;
            }
            if let Some(launcher) = &mut self.launcher
                && launcher.layer.wl_surface() == &event.surface
            {
                let apps = self.apps.len();
                match event.kind {
                    PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } => {
                        if launcher.hover(event.position, &self.theme, apps) {
                            self.draw_launcher();
                        }
                    }
                    PointerEventKind::Press {
                        button: BTN_LEFT, ..
                    } => match launcher.click(event.position, &self.theme, &self.apps) {
                        Outcome::Launch(id) => self.launch(&id),
                        Outcome::Redraw => self.draw_launcher(),
                        Outcome::Nothing | Outcome::Close => {}
                    },
                    PointerEventKind::Axis { vertical, .. } => {
                        // Wheel notches (v120), or touchpad travel.
                        let notches = if vertical.value120 != 0 {
                            vertical.value120 as f64 / 120.0
                        } else {
                            vertical.absolute / 60.0
                        };
                        if launcher.scroll(notches, &self.theme, apps) {
                            self.draw_launcher();
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let on_quick_settings = self
                .quick_settings
                .as_ref()
                .is_some_and(|(qs, _)| qs.popup.wl_surface() == &event.surface);
            if on_quick_settings {
                self.quick_settings_pointer(event);
                continue;
            }
            if self
                .calendar
                .as_ref()
                .is_some_and(|c| c.popup.wl_surface() == &event.surface)
            {
                self.calendar_pointer(event);
                continue;
            }
            let Some(index) = self
                .panels
                .iter()
                .position(|p| p.layer.wl_surface() == &event.surface)
            else {
                continue;
            };
            let right_click = matches!(
                event.kind,
                PointerEventKind::Press {
                    button: BTN_RIGHT,
                    ..
                }
            );
            if self.panels[index].edit.is_some() && !right_click {
                self.panel_edit_pointer(index, event);
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let hover = self.panels[index].button_at(event.position);
                    if self.panels[index].set_hover(hover) {
                        self.draw_panel(index);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.panels[index].set_hover(None) {
                        self.draw_panel(index);
                    }
                }
                PointerEventKind::Press {
                    button: BTN_RIGHT,
                    serial,
                    ..
                } => self.panel_menu(index, event.position, serial),
                PointerEventKind::Press {
                    button: BTN_LEFT,
                    serial,
                    ..
                } => match self.panels[index].button_at(event.position) {
                    Some(Button::Status) => self.toggle_quick_settings(index, serial),
                    Some(Button::Clock) => self.toggle_calendar(index, serial),
                    Some(Button::Activities) => {
                        self.ipc.send(&cnde_ipc::Request::ToggleOverview);
                    }
                    None => {}
                },
                _ => {}
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
        if let Some(calendar) = &mut self.calendar
            && &calendar.popup == popup
        {
            calendar.configured();
            self.draw_calendar();
        }
        if let Some(menu) = &mut self.menu
            && &menu.popup == popup
        {
            menu.configured();
            self.draw_menu();
        }
    }

    /// The compositor dismissed it (a click outside).
    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self.menu.as_ref().is_some_and(|m| &m.popup == popup) {
            self.close_menu();
        }
        if self
            .quick_settings
            .as_ref()
            .is_some_and(|(qs, _)| &qs.popup == popup)
        {
            self.quick_settings = None;
            self.quick_settings_dismissed = Some(std::time::Instant::now());
            self.redraw_panels();
        }
        if self.calendar.as_ref().is_some_and(|c| &c.popup == popup) {
            self.calendar = None;
            self.calendar_dismissed = Some(std::time::Instant::now());
            self.redraw_panels();
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
        if self.polkit_key(&event, &self.modifiers.clone()) {
            return;
        }
        if let Some(menu) = &mut self.menu {
            match event.keysym {
                Keysym::Escape => self.close_menu(),
                Keysym::Up => {
                    menu.key(-1);
                    self.draw_menu();
                }
                Keysym::Down | Keysym::Tab => {
                    menu.key(1);
                    self.draw_menu();
                }
                Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                    if let Some(action) = menu.chosen() {
                        self.menu_action(action);
                    }
                }
                _ => {}
            }
        } else if let Some(ui) = &mut self.screenshot {
            let outcome = ui.key(&event, &self.modifiers);
            self.screenshot_outcome(outcome);
        } else if self.launcher.is_some() {
            self.launcher_key(event);
        } else if event.keysym == Keysym::Escape
            && (self.quick_settings.take().is_some() || self.calendar.take().is_some())
        {
            self.redraw_panels();
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
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
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

/// Whether a press on the dock, with a menu open, only closes the menu.
/// A right-click goes on to open its own menu in place of the old one.
fn press_closes_menu(button: u32) -> bool {
    button != BTN_RIGHT
}

#[cfg(test)]
mod dock_menu_tests {
    use super::*;

    #[test]
    fn a_press_beside_the_menu_closes_it_without_clicking() {
        assert!(press_closes_menu(BTN_LEFT));
        assert!(press_closes_menu(BTN_MIDDLE));
        assert!(!press_closes_menu(BTN_RIGHT));
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn a_command_stands_in_for_a_missing_daemon() {
        let mut status = Status::default();
        status.apply(Update::PowerProfiles(None));
        assert!(status.power_profiles.is_none());

        status.set_profile_command("my-tool");
        let shown = status.power_profiles.clone().unwrap();
        assert_eq!(shown.available, status::PowerProfile::ALL.to_vec());
        assert_eq!(shown.active, status::PowerProfile::Balanced);

        status.apply(Update::CustomProfile(status::PowerProfile::PowerSaver));
        assert_eq!(
            status.power_profiles.as_ref().unwrap().active,
            status::PowerProfile::PowerSaver
        );

        // A running daemon wins.
        let daemon = status::PowerProfiles {
            active: status::PowerProfile::Performance,
            available: vec![
                status::PowerProfile::Balanced,
                status::PowerProfile::Performance,
            ],
        };
        status.apply(Update::PowerProfiles(Some(daemon.clone())));
        assert_eq!(status.power_profiles, Some(daemon));
    }
}
