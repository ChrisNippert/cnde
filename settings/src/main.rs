//! cnde-settings: a window for editing config.toml (ARCHITECTURE.md §8).
//!
//! It writes the same file you'd edit by hand, keeping your comments, and
//! the compositor and shell pick changes up live, as they do for hand
//! edits. Usage: `cnde-settings [page]`, e.g. `cnde-settings displays`.

mod arrange;
mod backlight;
mod bluetooth;
mod pages;
mod power_mode;
mod view;
mod wallpapers;
mod wifi;

use std::{
    collections::HashSet,
    os::fd::OwnedFd,
    path::PathBuf,
    time::{Duration, Instant},
};

use cnde_config::{Config, ConfigFile};
use cnde_ui::{Canvas, Fonts, Icons, Painter, Theme};
use rustix::{
    fs::inotify::{self, CreateFlags, WatchFlags},
    io::Errno,
};
use smithay_client_toolkit::reexports::client::backend::WaylandError;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_dispatch2, delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
            channel,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, Proxy, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
        protocols::wp::{
            fractional_scale::v1::client::{
                wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
                wp_fractional_scale_v1::{self, WpFractionalScaleV1},
            },
            viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
        },
        protocols::xdg::shell::client::xdg_toplevel::ResizeEdge,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{
            BTN_LEFT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec,
            ThemedPointer,
        },
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tracing::{debug, info, warn};

use crate::{
    pages::{BluetoothKey, Control, Display, JoinField, Key, Page, Value, WifiKey},
    view::{Edit, Painted, Res, Target, Ui},
};

const DEFAULT_SIZE: (u32, u32) = (920, 680);
const MIN_SIZE: (u32, u32) = (640, 420);
/// How close to the window's edge a press resizes it.
const RESIZE_BORDER: f64 = 6.0;
const ERROR_TIMEOUT: Duration = Duration::from_secs(6);
/// Two header clicks this close together maximize.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// While resizing, a configure waits this long for the last frame to show
/// before painting anyway.
const CONFIGURE_WAIT: Duration = Duration::from_millis(40);

struct App {
    registry: RegistryState,
    outputs: OutputState,
    seat: SeatState,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<App>,
    conn: Connection,
    shm_format: wl_shm::Format,
    window: Window,
    loop_handle: LoopHandle<'static, App>,
    exit: LoopSignal,

    ui: Ui,
    res: Res,
    painted: Painted,
    config_path: Option<PathBuf>,

    size: (u32, u32),
    scale: i32,
    /// The scale the compositor prefers (wp_fractional_scale), when it has
    /// the protocol and a viewporter to go with it.
    fractional: Option<f64>,
    viewport: Option<WpViewport>,
    /// Kept alive to keep the preferred-scale events coming.
    _fractional_object: Option<WpFractionalScaleV1>,
    configured: bool,
    maximized: bool,
    frame_pending: bool,
    dirty: bool,

    seat_handle: Option<wl_seat::WlSeat>,
    pointer: Option<ThemedPointer>,
    /// The surface the pointer's cursor image goes on, until a pointer
    /// takes it.
    cursor_surface: Option<wl_surface::WlSurface>,
    cursor: CursorIcon,
    /// A configure is waiting for the frame callback (or a timeout).
    configure_waiting: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    pointer_pos: (f64, f64),
    /// The slider being dragged: its row's key, and where the slider is.
    dragging: Option<(Key, cnde_ui::Rect)>,
    /// A monitor being dragged on the arrangement: where the pointer and
    /// the monitor started.
    arranging: Option<((f64, f64), (i32, i32))>,
    last_header_click: Option<Instant>,
    error_timer: Option<RegistrationToken>,
    /// Commands for the Wi-Fi worker, started when its page first opens.
    wifi: Option<std::sync::mpsc::Sender<wifi::Command>>,
    /// Commands for the Bluetooth worker, started when its page first opens.
    bluetooth: Option<std::sync::mpsc::Sender<bluetooth::Command>>,
    /// Commands for the power mode worker, started when the Power page opens.
    power_mode: Option<std::sync::mpsc::Sender<power_mode::Command>>,
    /// Where a picture chosen through "Add" arrives.
    picked: Option<channel::Sender<Result<Option<wallpapers::Thumb>, String>>>,
    /// The colour on screen, for the adaptive palette (from the compositor).
    screen: Option<cnde_ui::Color>,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let page = std::env::args()
        .nth(1)
        .and_then(|name| {
            let page = Page::from_name(&name);
            if page.is_none() {
                warn!(name, "no such page");
            }
            page
        })
        .unwrap_or(Page::Appearance);

    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;

    let config_path = cnde_config::default_path();
    let config = config_path
        .as_deref()
        .and_then(|path| {
            Config::load_or_create(path)
                .map_err(|err| warn!(%err, "config has an error; showing defaults"))
                .ok()
        })
        .unwrap_or_default();
    let theme = theme_for(&config, None);
    let frosted = config.appearance.material == cnde_config::Material::Frosted;
    cnde_ui::anim::set_speed(config.animations.speed as f32);
    let mut icons = Icons::new();
    icons.set_theme(&config.appearance.icon_theme);
    let fonts = Fonts::new(theme.font_families);

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let cursor_surface = compositor.create_surface(&qh);
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestClient, &qh);
    window.set_title("Settings");
    window.set_app_id("cnde-settings");
    window.set_min_size(Some(MIN_SIZE));
    // Fractional scale (1.25 and the like) draws at the exact pixel size and
    // lets the compositor show it 1:1. Without both globals, integer scale.
    let (viewport, fractional_object) = match (
        globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ScaleData),
        globals.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ScaleData),
    ) {
        (Ok(viewporter), Ok(manager)) => (
            Some(viewporter.get_viewport(window.wl_surface(), &qh, ScaleData)),
            Some(manager.get_fractional_scale(window.wl_surface(), &qh, ScaleData)),
        ),
        _ => (None, None),
    };
    window.commit();

    // Wallpaper thumbnails, decoded off the main thread.
    let (thumb_tx, thumb_rx) = channel::channel();
    event_loop
        .handle()
        .insert_source(thumb_rx, |event, _, app: &mut App| {
            if let channel::Event::Msg(thumb) = event {
                app.ui.thumbs.push(thumb);
                if app.ui.page == Page::Appearance {
                    app.redraw();
                }
            }
        })?;
    wallpapers::load(view::expand(&config.wallpaper.light), thumb_tx);
    // A picture of your own, chosen through "Add": show it and set it.
    let (picked_tx, picked_rx) = channel::channel::<Result<Option<wallpapers::Thumb>, String>>();
    event_loop
        .handle()
        .insert_source(picked_rx, |event, _, app: &mut App| {
            let channel::Event::Msg(result) = event else {
                return;
            };
            match result {
                Ok(Some(thumb)) => {
                    let light = Value::Str(tilde(&thumb.path));
                    app.ui.thumbs.retain(|t| t.path != thumb.path);
                    app.ui.thumbs.insert(0, thumb);
                    app.write(&[
                        (Key::Path("wallpaper.light"), light),
                        (Key::Path("wallpaper.dark"), Value::Unset),
                    ]);
                }
                Ok(None) => {}
                Err(message) => app.show_error(message),
            }
            app.redraw();
        })?;

    let (screen_tx, screen_rx) = channel::channel();
    event_loop
        .handle()
        .insert_source(screen_rx, |event, _, app: &mut App| {
            if let channel::Event::Msg(screen) = event
                && screen != app.screen
            {
                app.screen = screen;
                if app.ui.config.appearance.adaptive() {
                    app.res.theme = theme_for(&app.ui.config, screen);
                    app.redraw();
                }
            }
        })?;
    watch_screen(screen_tx);

    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seat: SeatState::new(&globals, &qh),
        pool: SlotPool::new((DEFAULT_SIZE.0 * DEFAULT_SIZE.1 * 4) as usize, &shm)?,
        shm,
        qh: qh.clone(),
        conn: conn.clone(),
        shm_format: wl_shm::Format::Argb8888,
        window,
        loop_handle: event_loop.handle(),
        exit: event_loop.get_signal(),
        ui: Ui {
            config,
            displays: Vec::new(),
            thumbs: Vec::new(),
            page,
            advanced: HashSet::new(),
            scroll: 0.0,
            hover: None,
            popover: None,
            edit: None,
            error: None,
            rounded: true,
            frosted,
            dragging: None,
            wifi: None,
            wifi_join: None,
            wifi_details: None,
            bluetooth: None,
            power_mode: None,
            brightness: backlight::read(),
            arranged: None,
            arranging: None,
        },
        res: Res {
            fonts,
            icons,
            theme,
            anim: view::Anim::new(),
        },
        painted: Painted::default(),
        config_path,
        size: DEFAULT_SIZE,
        scale: 1,
        fractional: None,
        viewport,
        _fractional_object: fractional_object,
        configured: false,
        maximized: false,
        frame_pending: false,
        dirty: false,
        seat_handle: None,
        pointer: None,
        cursor_surface: Some(cursor_surface),
        cursor: CursorIcon::Default,
        configure_waiting: false,
        keyboard: None,
        modifiers: Modifiers::default(),
        pointer_pos: (0.0, 0.0),
        dragging: None,
        arranging: None,
        last_header_click: None,
        error_timer: None,
        wifi: None,
        bluetooth: None,
        power_mode: None,
        picked: Some(picked_tx),
        screen: None,
    };
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;
    watch_config(&mut app);
    if app.ui.page == Page::Wifi {
        app.open_wifi();
    }
    if app.ui.page == Page::Bluetooth {
        app.open_bluetooth();
    }
    if app.ui.page == Page::Power {
        app.open_power();
    }

    let mut lost = None;
    event_loop.run(None, &mut app, |app| {
        if lost.is_none()
            && let Some(err) = connection_lost(&conn)
        {
            lost = Some(err);
            app.exit.stop();
        }
    })?;
    if let Some(err) = lost {
        return Err(err.into());
    }
    Ok(())
}

impl App {
    /// The Wi-Fi page is showing: start the worker (once), or rescan.
    fn open_wifi(&mut self) {
        if self.wifi.is_some() {
            self.wifi_command(wifi::Command::Scan);
            return;
        }
        let (tx, rx) = channel::channel::<wifi::State>();
        let inserted = self
            .loop_handle
            .insert_source(rx, |event, _, app: &mut App| {
                if let channel::Event::Msg(state) = event {
                    // Joined: the form's done.
                    if let Some(join) = &app.ui.wifi_join
                        && state
                            .networks
                            .iter()
                            .any(|n| n.ssid == join.ssid && n.connected)
                    {
                        app.ui.wifi_join = None;
                    }
                    // Its details close once it's neither connected nor
                    // saved (forgotten).
                    if let Some(ssid) = &app.ui.wifi_details
                        && !state
                            .networks
                            .iter()
                            .any(|n| (n.connected || n.known) && n.ssid == *ssid)
                    {
                        app.ui.wifi_details = None;
                    }
                    app.ui.wifi = Some(state);
                    app.redraw();
                }
            });
        if inserted.is_ok() {
            self.wifi = Some(wifi::start(tx));
        }
    }

    /// The Bluetooth page is showing: start the worker (once), and look for
    /// nearby devices until it's left.
    fn open_bluetooth(&mut self) {
        if self.bluetooth.is_none() {
            let (tx, rx) = channel::channel::<bluetooth::State>();
            let inserted = self
                .loop_handle
                .insert_source(rx, |event, _, app: &mut App| {
                    if let channel::Event::Msg(state) = event {
                        app.ui.bluetooth = Some(state);
                        if app.ui.page == Page::Bluetooth {
                            app.redraw();
                        }
                    }
                });
            if inserted.is_ok() {
                self.bluetooth = Some(bluetooth::start(tx));
            }
        }
        self.bluetooth_command(bluetooth::Command::Discover(true));
    }

    fn bluetooth_command(&mut self, command: bluetooth::Command) {
        if let Some(tx) = &self.bluetooth {
            let _ = tx.send(command);
        }
    }

    /// The Power page is showing: start the worker (once) and look again.
    fn open_power(&mut self) {
        if self.power_mode.is_none() {
            let (tx, rx) = channel::channel::<power_mode::State>();
            let inserted = self
                .loop_handle
                .insert_source(rx, |event, _, app: &mut App| {
                    if let channel::Event::Msg(state) = event {
                        app.ui.power_mode = Some(state);
                        if app.ui.page == Page::Power {
                            app.redraw();
                        }
                    }
                });
            if inserted.is_ok() {
                self.power_mode = Some(power_mode::start(tx));
            }
        } else if let Some(tx) = &self.power_mode {
            let _ = tx.send(power_mode::Command::Refresh);
        }
    }

    /// The power mode buttons go to the worker.
    fn write_power_mode(&mut self, value: &Value) {
        if let (Some(tx), mode @ ("power-saver" | "balanced" | "performance")) =
            (&self.power_mode, value.as_str())
        {
            // Show it at once; the worker confirms (or corrects) shortly.
            if let Some(state) = &mut self.ui.power_mode {
                state.active = mode.to_string();
            }
            let _ = tx.send(power_mode::Command::Set(mode.to_string()));
        }
    }

    /// The brightness slider sets the panel's backlight directly.
    fn write_brightness(&mut self, value: &Value) {
        let Some(percent) = value.as_f64() else {
            return;
        };
        if let Err(err) = backlight::set(percent as u32) {
            self.show_error(err);
        }
        self.ui.brightness = backlight::read();
    }

    /// The Bluetooth switch and device buttons go to the worker.
    fn write_bluetooth(&mut self, key: &BluetoothKey, value: &Value) {
        let command = match (key, value) {
            (BluetoothKey::Powered, Value::Bool(on)) => bluetooth::Command::SetPowered(*on),
            (BluetoothKey::Device(address), value) => {
                let address = address.clone();
                match value.as_str() {
                    "pair" => bluetooth::Command::Pair(address),
                    "connect" => bluetooth::Command::Connect(address),
                    "disconnect" => bluetooth::Command::Disconnect(address),
                    "forget" => bluetooth::Command::Forget(address),
                    _ => return,
                }
            }
            _ => return,
        };
        self.bluetooth_command(command);
    }

    /// "Add" in the wallpaper grid: choose a picture.
    fn add_wallpaper(&mut self) {
        if let Some(tx) = &self.picked {
            wallpapers::add(tx.clone());
        }
    }

    fn wifi_command(&mut self, command: wifi::Command) {
        if let Some(tx) = &self.wifi {
            let _ = tx.send(command);
        }
    }

    /// A Wi-Fi row was clicked: bring it up if it's saved or open, else
    /// show what joining it needs.
    fn wifi_clicked(&mut self, ssid: &str) {
        let Some(network) = self
            .ui
            .wifi
            .as_ref()
            .and_then(|w| w.networks.iter().find(|n| n.ssid == ssid))
            .cloned()
        else {
            return;
        };
        if network.connected {
            self.open_sub_page(Some(ssid.to_string()));
            return;
        }
        if network.known || !network.security.secured() {
            self.ui.wifi_join = None;
            self.wifi_command(wifi::Command::Activate(ssid.to_string()));
            return;
        }
        // Clicking it again folds the form away.
        if self
            .ui
            .wifi_join
            .as_ref()
            .is_some_and(|j| !j.hidden && j.ssid == ssid)
        {
            self.ui.wifi_join = None;
            self.ui.edit = None;
            self.redraw();
            return;
        }
        self.ui.wifi_join = Some(wifi::Join {
            ssid: ssid.to_string(),
            security: network.security,
            ..wifi::Join::default()
        });
        let first = if network.security == wifi::Security::Enterprise {
            JoinField::Identity
        } else {
            JoinField::Password
        };
        self.edit_field(Key::Wifi(WifiKey::Join(first)), String::new());
        self.redraw();
    }

    /// Into the connected network's details (`Some`), or back out of them,
    /// with the fade a new page gets.
    fn open_sub_page(&mut self, ssid: Option<String>) {
        self.ui.wifi_details = ssid;
        self.ui.scroll = 0.0;
        self.res
            .anim
            .restart(view::AnimKey::Page, 0.0, 1.0, cnde_ui::anim::MEDIUM);
        self.redraw();
    }

    fn edit_field(&mut self, key: Key, text: String) {
        self.ui.edit = Some(Edit {
            key,
            cursor: text.chars().count(),
            text,
            int: false,
        });
    }

    /// Changes to Wi-Fi rows go to the worker (or the join form); the rest
    /// are config.
    fn write_wifi(&mut self, key: &WifiKey, value: &Value) {
        let text = value.as_str();
        match (key, value) {
            (WifiKey::Enabled, Value::Bool(on)) => {
                self.wifi_command(wifi::Command::SetEnabled(*on))
            }
            (WifiKey::Actions(_), _) if text == "disconnect" => {
                self.wifi_command(wifi::Command::Disconnect)
            }
            (WifiKey::Actions(ssid), _) if text == "forget" => {
                self.wifi_command(wifi::Command::Forget(ssid.clone()))
            }
            (WifiKey::Actions(ssid), _) if text == "connect" => {
                self.wifi_command(wifi::Command::Activate(ssid.clone()))
            }
            (WifiKey::AutoConnect(ssid), Value::Bool(on)) => {
                self.wifi_command(wifi::Command::SetAutoConnect(ssid.clone(), *on))
            }
            (WifiKey::Hidden, _) => {
                self.ui.wifi_join = Some(wifi::Join {
                    hidden: true,
                    security: wifi::Security::Psk,
                    ..wifi::Join::default()
                });
                self.edit_field(Key::Wifi(WifiKey::Join(JoinField::Ssid)), String::new());
            }
            (WifiKey::Join(JoinField::Submit), _) => match text {
                "connect" => self.submit_join(),
                _ => self.ui.wifi_join = None,
            },
            (WifiKey::Join(field), _) => {
                let Some(join) = &mut self.ui.wifi_join else {
                    return;
                };
                let text = text.to_string();
                match field {
                    JoinField::Ssid => join.ssid = text,
                    JoinField::Security => {
                        join.security = wifi::Security::from_name(&text).unwrap_or_default()
                    }
                    JoinField::Password => join.password = text,
                    JoinField::Eap => {
                        if let Some(eap) = wifi::Eap::ALL.into_iter().find(|e| e.name() == text) {
                            join.eap = eap;
                        }
                    }
                    JoinField::Phase2 => {
                        if let Some(p) = wifi::Phase2::ALL.into_iter().find(|p| p.name() == text) {
                            join.phase2 = p;
                        }
                    }
                    JoinField::Identity => join.identity = text,
                    JoinField::AnonymousIdentity => join.anonymous_identity = text,
                    JoinField::CaCert => join.ca_cert = text,
                    JoinField::Domain => join.domain = text,
                    JoinField::ClientCert => join.client_cert = text,
                    JoinField::PrivateKey => join.private_key = text,
                    JoinField::Submit => {}
                }
            }
            _ => {}
        }
    }

    /// Connect with what the form says, or say what's missing. The form
    /// stays until it's connected, in case something needs fixing.
    fn submit_join(&mut self) {
        let Some(join) = self.ui.wifi_join.clone() else {
            return;
        };
        if let Some(missing) = join.missing() {
            self.show_error(missing.into());
            return;
        }
        self.clear_error();
        self.wifi_command(wifi::Command::Join(join));
    }

    fn redraw(&mut self) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let started = Instant::now();
        // Fractional: paint at round(logical * scale) and let the viewport map
        // it back onto the logical size. Otherwise a whole-number scale.
        let fractional = self.fractional.filter(|_| self.viewport.is_some());
        let scale = fractional.unwrap_or(self.scale.max(1) as f64);
        let (w, h) = match fractional {
            Some(scale) => (
                (self.size.0 as f64 * scale).round() as i32,
                (self.size.1 as f64 * scale).round() as i32,
            ),
            None => (
                self.size.0 as i32 * scale as i32,
                self.size.1 as i32 * scale as i32,
            ),
        };
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate a buffer");
                return;
            }
        };
        {
            let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
                return;
            };
            self.painted = view::paint(&self.ui, &mut self.res, &mut canvas);
        }
        let painted_in = started.elapsed();
        // Something's moving: paint again on the next frame callback.
        if self.res.anim.running() {
            self.dirty = true;
        }
        // Scrolled past the end (the page got shorter): clamp and repaint.
        if self.ui.scroll > self.painted.max_scroll {
            self.ui.scroll = self.painted.max_scroll;
            self.dirty = true;
        }
        let swapped = self.shm_format == wl_shm::Format::Argb8888;
        if swapped {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        debug!(
            paint_ms = painted_in.as_secs_f64() * 1000.0,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            w,
            h,
            scale,
            swapped,
            "frame painted"
        );
        let surface = self.window.wl_surface();
        match (&self.viewport, fractional) {
            (Some(viewport), Some(_)) => {
                surface.set_buffer_scale(1);
                viewport.set_destination(self.size.0 as i32, self.size.1 as i32);
            }
            _ => surface.set_buffer_scale(scale as i32),
        }
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.frame(&self.qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            surface.commit();
        }
    }

    /// Write `changes` to config.toml together. Nothing is written if the
    /// result isn't a valid config; the error shows instead.
    fn write(&mut self, changes: &[(Key, Value)]) {
        let mut config_changes = Vec::new();
        for (key, value) in changes {
            match key {
                Key::Wifi(wifi) => self.write_wifi(wifi, value),
                Key::Bluetooth(key) => self.write_bluetooth(key, value),
                Key::Brightness => self.write_brightness(value),
                Key::PowerMode => self.write_power_mode(value),
                _ => config_changes.push((key.clone(), value.clone())),
            }
        }
        if config_changes.is_empty() {
            self.redraw();
            return;
        }
        let changes = &config_changes[..];
        let Some(path) = self.config_path.clone() else {
            self.show_error("No config file: HOME isn't set".into());
            return;
        };
        let result = ConfigFile::open(&path).and_then(|mut file| {
            // What the file says now, in case it was edited by hand since.
            let before = file.config().unwrap_or_else(|_| self.ui.config.clone());
            for (key, value) in changes {
                key.write(&mut file, &before, value);
                // The old switch for the adaptive palette would override
                // whatever's chosen now.
                if *key == pages::Key::Path("appearance.palette") {
                    file.remove("appearance.adaptive_color");
                    file.remove("appearance.match_windows");
                }
                // One material for everything: drop the old per-piece ones.
                if *key == pages::Key::Path("appearance.material") {
                    file.remove("panel.frosted");
                    file.remove("dock.background");
                }
            }
            file.save()
        });
        match result {
            Ok(config) => {
                self.set_config(config);
                self.clear_error();
            }
            Err(err) => self.show_error(format!("Not saved: {err}")),
        }
        self.redraw();
    }

    fn set_config(&mut self, config: Config) {
        cnde_ui::anim::set_speed(config.animations.speed as f32);
        if config.appearance != self.ui.config.appearance {
            self.res.theme = theme_for(&config, self.screen);
            self.res.icons.set_theme(&config.appearance.icon_theme);
            self.ui.frosted = config.appearance.material == cnde_config::Material::Frosted;
        }
        self.ui.config = config;
        self.redraw();
    }

    fn show_error(&mut self, message: String) {
        warn!(message);
        self.ui.error = Some(message);
        if let Some(token) = self.error_timer.take() {
            self.loop_handle.remove(token);
        }
        self.error_timer = self
            .loop_handle
            .insert_source(Timer::from_duration(ERROR_TIMEOUT), |_, _, app| {
                app.error_timer = None;
                app.ui.error = None;
                app.redraw();
                TimeoutAction::Drop
            })
            .ok();
    }

    fn clear_error(&mut self) {
        self.ui.error = None;
        if let Some(token) = self.error_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Re-read config.toml after something else changed it.
    fn reload_config(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        match Config::load_or_create(path) {
            Ok(config) if config != self.ui.config => {
                info!("config changed on disk");
                self.set_config(config);
            }
            Ok(_) => {}
            Err(err) => warn!(%err, "config on disk has an error; showing the last good one"),
        }
    }

    fn update_displays(&mut self) {
        let mut displays: Vec<Display> = self
            .outputs
            .outputs()
            .filter_map(|output| {
                let info = self.outputs.info(&output)?;
                let name = info.name.clone()?;
                let mut parts = Vec::new();
                let model = format!("{} {}", info.make, info.model);
                if !model.trim().is_empty() && !model.contains("Unknown") {
                    parts.push(model.trim().to_string());
                }
                let current = info.modes.iter().find(|m| m.current);
                if let Some(mode) = current {
                    parts.push(format!(
                        "{}×{} at {:.0} Hz",
                        mode.dimensions.0,
                        mode.dimensions.1,
                        mode.refresh_rate as f64 / 1000.0
                    ));
                }
                let triple = |m: &smithay_client_toolkit::output::Mode| {
                    (m.dimensions.0, m.dimensions.1, m.refresh_rate)
                };
                let (x, y) = info.logical_position.unwrap_or(info.location);
                let (w, h) = info.logical_size.unwrap_or_else(|| {
                    let (w, h) = current.map_or((1920, 1080), |m| m.dimensions);
                    let s = info.scale_factor.max(1);
                    (w / s, h / s)
                });
                Some(Display {
                    name,
                    description: parts.join(" · "),
                    x,
                    y,
                    w,
                    h,
                    modes: info.modes.iter().map(triple).collect(),
                    current: current.map(triple),
                    preferred: info.modes.iter().find(|m| m.preferred).map(triple),
                })
            })
            .collect();
        displays.sort_by(|a, b| a.name.cmp(&b.name));
        if displays != self.ui.displays {
            self.ui.displays = displays;
            if self.arranging.is_none() {
                self.ui.arranged = None;
            }
            self.redraw();
        }
    }

    fn row(&self, index: usize) -> Option<&pages::Row> {
        self.painted.rows.get(index)
    }

    fn press(&mut self, position: (f64, f64), serial: u32) {
        let target = self.painted.hit(position);

        // An open dropdown takes the click: a choice, or closing it.
        if let Some(key) = self.ui.popover.take() {
            if let Some(Target::Choice(i)) = target
                && let Some(row) = self.painted.rows.iter().find(|r| r.key == key)
                && let Control::Dropdown(choices) = &row.control
                && let Some(choice) = choices.get(i)
            {
                let value = choice.value.clone();
                self.write(&[(key, value)]);
            }
            self.redraw();
            return;
        }

        if let Some(edge) = self.resize_edge(position)
            && let Some(seat) = &self.seat_handle
        {
            self.window.resize(seat, serial, edge);
            return;
        }

        // Clicking anywhere else finishes typing.
        let same_entry = matches!(
            (target, &self.ui.edit),
            (Some(Target::Entry(i)), Some(edit)) if self.row(i).is_some_and(|r| r.key == edit.key)
        );
        if !same_entry {
            self.commit_edit();
        }

        let Some(target) = target else {
            return;
        };
        match target {
            Target::Page(page) => {
                if self.ui.page != page {
                    // Stop looking for devices once their page is left.
                    if self.ui.page == Page::Bluetooth {
                        self.bluetooth_command(bluetooth::Command::Discover(false));
                    }
                    self.ui.page = page;
                    self.ui.wifi_details = None;
                    self.res
                        .anim
                        .restart(view::AnimKey::Page, 0.0, 1.0, cnde_ui::anim::MEDIUM);
                    self.ui.scroll = 0.0;
                    if page == Page::Wifi {
                        self.open_wifi();
                    }
                    if page == Page::Bluetooth {
                        self.open_bluetooth();
                    }
                    if page == Page::Power {
                        self.open_power();
                    }
                    if page == Page::Displays {
                        self.ui.brightness = backlight::read();
                    }
                }
            }
            Target::Back => self.open_sub_page(None),
            Target::Close => self.exit.stop(),
            Target::Minimize => self.window.set_minimized(),
            Target::Maximize => {
                if self.maximized {
                    self.window.unset_maximized();
                } else {
                    self.window.set_maximized();
                }
            }
            Target::Header => {
                let now = Instant::now();
                if self
                    .last_header_click
                    .is_some_and(|t| now - t < DOUBLE_CLICK)
                {
                    self.last_header_click = None;
                    if self.maximized {
                        self.window.unset_maximized();
                    } else {
                        self.window.set_maximized();
                    }
                } else {
                    self.last_header_click = Some(now);
                    if let Some(seat) = &self.seat_handle {
                        self.window.move_(seat, serial);
                    }
                }
            }
            Target::Row(i) | Target::Switch(i) | Target::Dropdown(i) => {
                let Some(row) = self.row(i).cloned() else {
                    return;
                };
                match &row.control {
                    Control::Network { .. } => match &row.key {
                        Key::Wifi(WifiKey::Network(ssid)) => self.wifi_clicked(&ssid.clone()),
                        Key::Wifi(WifiKey::Saved(ssid)) => self.open_sub_page(Some(ssid.clone())),
                        _ => {}
                    },
                    Control::Switch { on, off } => {
                        let value = if row.is_on() { off } else { on };
                        self.write(&[(row.key.clone(), value.clone())]);
                    }
                    Control::Dropdown(_) => {
                        self.ui.popover = Some(row.key.clone());
                        self.res.anim.restart(
                            view::AnimKey::Popover,
                            0.0,
                            1.0,
                            cnde_ui::anim::SHORT,
                        );
                    }
                    _ => {}
                }
            }
            Target::Segment(i, j) => {
                if let Some(row) = self.row(i).cloned()
                    && let Control::Segmented(choices) = &row.control
                    && let Some(choice) = choices.get(j)
                    && choice.value != row.value
                {
                    self.write(&[(row.key.clone(), choice.value.clone())]);
                }
            }
            Target::Slider(i) => {
                if let Some(row) = self.row(i)
                    && let Some((_, rect)) = self.painted.sliders.iter().find(|(r, _)| *r == i)
                {
                    let key = row.key.clone();
                    self.dragging = Some((key.clone(), *rect));
                    self.ui.dragging = Some(key);
                    self.drag(position.0);
                }
            }
            Target::Entry(i) => {
                if let Some(row) = self.row(i).cloned()
                    && self.ui.edit.as_ref().is_none_or(|e| e.key != row.key)
                {
                    let text = match &row.value {
                        Value::Int(v) => v.to_string(),
                        Value::Float(v) => v.to_string(),
                        other => other.as_str().to_string(),
                    };
                    self.ui.edit = Some(Edit {
                        key: row.key.clone(),
                        cursor: text.chars().count(),
                        text,
                        int: matches!(row.control, Control::Text { int: true, .. }),
                    });
                }
            }
            Target::Reset(i) => {
                if let Some(row) = self.row(i) {
                    let key = row.key.clone();
                    self.write(&[(key, Value::Unset)]);
                }
            }
            Target::Wallpaper(index) => {
                let light = match index {
                    None => Value::Unset,
                    Some(i) => match self.ui.thumbs.get(i) {
                        Some(thumb) => Value::Str(tilde(&thumb.path)),
                        None => return,
                    },
                };
                // One picture for both styles, like GNOME's picker.
                self.write(&[
                    (Key::Path("wallpaper.light"), light),
                    (Key::Path("wallpaper.dark"), Value::Unset),
                ]);
            }
            Target::Swatch(i, n) => {
                let hex = pages::SWATCHES.as_flattened().get(n);
                if let Some(row) = self.row(i)
                    && let Some(hex) = hex
                {
                    let key = row.key.clone();
                    self.ui.edit = None;
                    self.write(&[(key, Value::Str(hex.to_string()))]);
                }
            }
            Target::Screen(i) => {
                if let Some(d) = self.ui.displays.get(i) {
                    let at = self
                        .ui
                        .arranged
                        .as_ref()
                        .and_then(|a| a.get(i).copied())
                        .unwrap_or((d.x, d.y));
                    self.arranging = Some((position, at));
                    self.ui.arranging = Some(i);
                    if self.ui.arranged.is_none() {
                        self.ui.arranged =
                            Some(self.ui.displays.iter().map(|d| (d.x, d.y)).collect());
                    }
                }
            }
            Target::AddWallpaper => self.add_wallpaper(),
            Target::Advanced => {
                let page = self.ui.page;
                if !self.ui.advanced.remove(&page) {
                    self.ui.advanced.insert(page);
                }
            }
            Target::Choice(_) => {}
        }
        self.redraw();
    }

    /// Move the dragged monitor with the pointer, snapped against the others.
    fn arrange_drag(&mut self, position: (f64, f64)) {
        let (Some((grab, start)), Some(i), Some(arranged)) =
            (self.arranging, self.ui.arranging, self.ui.arranged.as_mut())
        else {
            return;
        };
        let scale = self.painted.arrange_scale.max(0.001) as f64;
        let want = (
            start.0 + ((position.0 - grab.0) / scale).round() as i32,
            start.1 + ((position.1 - grab.1) / scale).round() as i32,
        );
        let screens: Vec<arrange::Screen> = self
            .ui
            .displays
            .iter()
            .zip(arranged.iter())
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, (d, &(x, y)))| arrange::Screen {
                x,
                y,
                w: d.w,
                h: d.h,
            })
            .collect();
        let Some(d) = self.ui.displays.get(i) else {
            return;
        };
        // Edges line up within about 10 window px.
        let snap = (10.0 / scale).round() as i32;
        let placed = arrange::place(&screens, d.w, d.h, want, snap);
        if arranged[i] != placed {
            arranged[i] = placed;
            self.redraw();
        }
    }

    /// Dropped: save every monitor's position.
    fn arrange_drop(&mut self) {
        self.arranging = None;
        self.ui.arranging = None;
        let Some(mut positions) = self.ui.arranged.clone() else {
            return;
        };
        let mut before: Vec<(i32, i32)> = self.ui.displays.iter().map(|d| (d.x, d.y)).collect();
        arrange::normalize(&mut positions);
        arrange::normalize(&mut before);
        if positions == before {
            self.ui.arranged = None;
            self.redraw();
            return;
        }
        let changes: Vec<(Key, Value)> = self
            .ui
            .displays
            .iter()
            .zip(&positions)
            .map(|(d, &(x, y))| {
                (
                    Key::Output {
                        name: d.name.clone(),
                        field: "position",
                    },
                    Value::Point(x as i64, y as i64),
                )
            })
            .collect();
        self.write(&changes);
    }

    /// Set the dragged slider from the pointer's x.
    fn drag(&mut self, x: f64) {
        let Some((key, rect)) = self.dragging.clone() else {
            return;
        };
        // The slider's row: a setting can also have a switch row (animations
        // on/off and their speed share one key).
        let Some(row) = self
            .painted
            .rows
            .iter()
            .find(|r| r.key == key && matches!(r.control, Control::Slider { .. }))
        else {
            return;
        };
        let Control::Slider {
            min,
            max,
            step,
            int,
            ..
        } = row.control
        else {
            return;
        };
        let t = Painter::scale_value(rect, x) as f64;
        let v = ((min + (max - min) * t) / step).round() * step;
        let v = v.clamp(min, max);
        let value = if int {
            Value::Int(v.round() as i64)
        } else {
            Value::Float((v * 100.0).round() / 100.0)
        };
        if value != row.value {
            self.write(&[(key, value)]);
        }
    }

    fn commit_edit(&mut self) {
        let Some(edit) = self.ui.edit.take() else {
            return;
        };
        // Passwords are taken exactly as typed; the join form's other
        // fields aren't config either.
        if let Key::Wifi(key @ WifiKey::Join(field)) = &edit.key {
            let text = if *field == JoinField::Password {
                edit.text.clone()
            } else {
                edit.text.trim().to_string()
            };
            self.write_wifi(&key.clone(), &Value::Str(text));
            self.redraw();
            return;
        }
        let text = edit.text.trim();
        let value = if edit.int {
            if text.is_empty() {
                Value::Unset
            } else {
                match text.parse::<i64>() {
                    Ok(v) => Value::Int(v),
                    Err(_) => {
                        self.show_error(format!("Not saved: \"{text}\" isn't a whole number"));
                        self.redraw();
                        return;
                    }
                }
            }
        } else {
            Value::Str(text.to_string())
        };
        let current = self
            .painted
            .rows
            .iter()
            .find(|r| r.key == edit.key)
            .map(|r| r.value.clone());
        if current.as_ref() != Some(&value) {
            self.write(&[(edit.key, value)]);
        }
        self.redraw();
    }

    fn key(&mut self, event: KeyEvent) {
        let ctrl = self.modifiers.ctrl;
        if ctrl && matches!(event.keysym, Keysym::q | Keysym::w) {
            self.exit.stop();
            return;
        }
        if event.keysym == Keysym::Escape {
            if self.ui.popover.take().is_some() || self.ui.edit.take().is_some() {
                self.redraw();
            } else if self.ui.sub_page().is_some() {
                self.open_sub_page(None);
            }
            return;
        }
        let Some(edit) = &mut self.ui.edit else {
            return;
        };
        let len = edit.text.chars().count();
        let byte = |text: &str, chars: usize| {
            text.char_indices()
                .nth(chars)
                .map_or(text.len(), |(i, _)| i)
        };
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => {
                let joining = matches!(edit.key, Key::Wifi(WifiKey::Join(_)));
                let key = edit.key.clone();
                self.commit_edit();
                // In the join form: connect once it's complete, else on to
                // the next field.
                if joining {
                    if self
                        .ui
                        .wifi_join
                        .as_ref()
                        .is_some_and(|j| j.missing().is_none())
                        || !self.edit_next(&key)
                    {
                        self.submit_join();
                    }
                    self.redraw();
                }
                return;
            }
            Keysym::Tab | Keysym::ISO_Left_Tab => {
                let key = edit.key.clone();
                self.commit_edit();
                self.edit_next(&key);
                self.redraw();
                return;
            }
            Keysym::BackSpace if edit.cursor > 0 => {
                let at = byte(&edit.text, edit.cursor - 1);
                edit.text.remove(at);
                edit.cursor -= 1;
            }
            Keysym::Delete if edit.cursor < len => {
                let at = byte(&edit.text, edit.cursor);
                edit.text.remove(at);
            }
            Keysym::Left => edit.cursor = edit.cursor.saturating_sub(1),
            Keysym::Right => edit.cursor = (edit.cursor + 1).min(len),
            Keysym::Home => edit.cursor = 0,
            Keysym::End => edit.cursor = len,
            _ => {
                let Some(text) = event
                    .utf8
                    .filter(|t| !ctrl && !t.chars().any(char::is_control))
                else {
                    return;
                };
                let at = byte(&edit.text, edit.cursor);
                edit.text.insert_str(at, &text);
                edit.cursor += text.chars().count();
            }
        }
        self.redraw();
    }

    /// Resize arrows over the edges, the plain arrow elsewhere. The cursor
    /// must be set again on every enter.
    fn update_cursor(&mut self, entered: bool) {
        let icon = match self.resize_edge(self.pointer_pos) {
            Some(ResizeEdge::TopLeft) => CursorIcon::NwResize,
            Some(ResizeEdge::TopRight) => CursorIcon::NeResize,
            Some(ResizeEdge::BottomLeft) => CursorIcon::SwResize,
            Some(ResizeEdge::BottomRight) => CursorIcon::SeResize,
            Some(ResizeEdge::Left) => CursorIcon::WResize,
            Some(ResizeEdge::Right) => CursorIcon::EResize,
            Some(ResizeEdge::Top) => CursorIcon::NResize,
            Some(ResizeEdge::Bottom) => CursorIcon::SResize,
            _ => CursorIcon::Default,
        };
        if (entered || icon != self.cursor)
            && let Some(pointer) = &self.pointer
        {
            self.cursor = icon;
            let _ = pointer.set_cursor(&self.conn, icon);
        }
    }

    /// Start typing in the text field after the one for `key`; false if
    /// there's none.
    fn edit_next(&mut self, key: &Key) -> bool {
        let rows = &self.painted.rows;
        let Some(at) = rows.iter().position(|r| r.key == *key) else {
            return false;
        };
        let next = rows[at + 1..].iter().find(|r| {
            matches!(
                r.control,
                Control::Text { .. } | Control::Password | Control::Color { .. }
            )
        });
        if let Some(row) = next.cloned() {
            let text = match &row.value {
                Value::Int(v) => v.to_string(),
                Value::Float(v) => v.to_string(),
                other => other.as_str().to_string(),
            };
            self.edit_field(row.key.clone(), text);
            if let Some(edit) = &mut self.ui.edit {
                edit.int = matches!(row.control, Control::Text { int: true, .. });
            }
            return true;
        }
        false
    }

    fn resize_edge(&self, (x, y): (f64, f64)) -> Option<ResizeEdge> {
        if self.maximized {
            return None;
        }
        let (w, h) = (self.size.0 as f64, self.size.1 as f64);
        let b = RESIZE_BORDER;
        let (left, right, top, bottom) = (x < b, x > w - b, y < b, y > h - b);
        Some(match (left, right, top, bottom) {
            (true, _, true, _) => ResizeEdge::TopLeft,
            (_, true, true, _) => ResizeEdge::TopRight,
            (true, _, _, true) => ResizeEdge::BottomLeft,
            (_, true, _, true) => ResizeEdge::BottomRight,
            (true, ..) => ResizeEdge::Left,
            (_, true, ..) => ResizeEdge::Right,
            (_, _, true, _) => ResizeEdge::Top,
            (_, _, _, true) => ResizeEdge::Bottom,
            _ => return None,
        })
    }
}

fn theme_for(config: &Config, screen: Option<cnde_ui::Color>) -> Theme {
    Theme::for_screen(&config.appearance, screen)
}

/// Follow the compositor's edges event for the adaptive palette's colour:
/// the window against a monitor's top, else the wallpaper. Read on a
/// thread; nothing without cnde's socket.
fn watch_screen(events: channel::Sender<Option<cnde_ui::Color>>) {
    use std::io::BufRead;
    let Some(path) = std::env::var_os(cnde_ipc::SOCKET_ENV) else {
        return;
    };
    let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) else {
        return;
    };
    let _ = std::thread::Builder::new().name("ipc".into()).spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.split(b'\n').map_while(Result::ok) {
            if let Some(cnde_ipc::Event::Edges { outputs }) = cnde_ipc::decode(&line) {
                let screen = cnde_ui::tint::screen(
                    outputs.iter().find_map(|e| e.top_tint),
                    outputs.iter().find_map(|e| e.wallpaper_tint),
                );
                if events.send(screen).is_err() {
                    return;
                }
            }
        }
    });
}

/// "~/…" for paths in the home directory, as hand-written configs do.
fn tilde(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Reload when config.toml changes (by hand, or another settings window).
fn watch_config(app: &mut App) {
    let Some(dir) = app
        .config_path
        .as_deref()
        .and_then(|p| p.parent().map(PathBuf::from))
    else {
        return;
    };
    let watcher = || -> rustix::io::Result<OwnedFd> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        inotify::add_watch(
            &fd,
            &dir,
            WatchFlags::CLOSE_WRITE | WatchFlags::MOVED_TO | WatchFlags::CREATE,
        )?;
        Ok(fd)
    };
    let Ok(fd) = watcher() else {
        warn!("can't watch the config; hand edits show after a restart");
        return;
    };
    let result = app.loop_handle.insert_source(
        Generic::new(fd, Interest::READ, Mode::Level),
        |_, fd, app| {
            let mut buf = [0u8; 4096];
            loop {
                match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) | Err(Errno::AGAIN) => break,
                    Ok(_) | Err(Errno::INTR) => continue,
                    Err(_) => break,
                }
            }
            app.reload_config();
            Ok(PostAction::Continue)
        },
    );
    if let Err(err) = result {
        warn!(?err, "can't watch the config");
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit.stop();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        if let (Some(w), Some(h)) = configure.new_size {
            self.size = (w.get().max(MIN_SIZE.0), h.get().max(MIN_SIZE.1));
        }
        self.maximized = configure.is_maximized();
        // is_tiled() wants all four edges; snapped to a side is three.
        self.ui.rounded = !(configure.is_maximized()
            || configure.is_fullscreen()
            || configure
                .state
                .intersects(smithay_client_toolkit::reexports::csd_frame::WindowState::TILED));
        if !self.configured {
            self.configured = true;
            self.choose_shm_format();
        }
        // Paint at the display's pace: a drag sends a configure per pointer
        // motion, and painting each one at once queues frames up behind the
        // pointer. The frame callback paints the latest size; if it doesn't
        // come soon (hidden, say), paint anyway.
        if !self.frame_pending {
            self.redraw();
            return;
        }
        self.dirty = true;
        if !self.configure_waiting {
            self.configure_waiting = true;
            let _ = self.loop_handle.insert_source(
                Timer::from_duration(CONFIGURE_WAIT),
                |_, _, app| {
                    app.configure_waiting = false;
                    if app.dirty {
                        app.frame_pending = false;
                        app.redraw();
                    }
                    TimeoutAction::Drop
                },
            );
        }
    }
}

/// User data for the viewporter and fractional-scale objects.
struct ScaleData;

impl Dispatch2<WpViewporter, App> for ScaleData {
    fn event(
        &self,
        _: &mut App,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpViewport, App> for ScaleData {
    fn event(
        &self,
        _: &mut App,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpFractionalScaleManagerV1, App> for ScaleData {
    fn event(
        &self,
        _: &mut App,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpFractionalScaleV1, App> for ScaleData {
    fn event(
        &self,
        app: &mut App,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        // In 120ths.
        let scale = scale as f64 / 120.0;
        if app.fractional != Some(scale) {
            app.fractional = Some(scale);
            app.redraw();
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if factor != self.scale {
            self.scale = factor.max(1);
            self.redraw();
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        if self.dirty {
            self.redraw();
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
        self.update_displays();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.update_displays();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.update_displays();
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
        if capability == Capability::Pointer
            && self.pointer.is_none()
            && let Some(surface) = self.cursor_surface.take()
        {
            self.pointer = self
                .seat
                .get_pointer_with_theme::<_, ()>(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    surface,
                    ThemeSpec::default(),
                )
                .ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self
                .seat
                .get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.loop_handle.clone(),
                    Box::new(|app, _, event| app.key(event)),
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
            pointer.pointer().release();
        }
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
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
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_pos = event.position;
                    let entered = matches!(event.kind, PointerEventKind::Enter { .. });
                    self.update_cursor(entered);
                    if self.dragging.is_some() {
                        self.drag(event.position.0);
                        continue;
                    }
                    if self.arranging.is_some() {
                        self.arrange_drag(event.position);
                        continue;
                    }
                    let hover = self.painted.hit(event.position);
                    if hover != self.ui.hover {
                        self.ui.hover = hover;
                        self.redraw();
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.ui.hover.take().is_some() {
                        self.redraw();
                    }
                }
                PointerEventKind::Press {
                    button: BTN_LEFT,
                    serial,
                    ..
                } => self.press(event.position, serial),
                PointerEventKind::Release {
                    button: BTN_LEFT, ..
                } => {
                    self.dragging = None;
                    self.ui.dragging = None;
                    if self.arranging.is_some() {
                        self.arrange_drop();
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    if self.ui.popover.is_some() {
                        continue;
                    }
                    // Wheel clicks come as 15 px steps; make them bigger.
                    let delta = if vertical.discrete != 0 || vertical.value120 != 0 {
                        vertical.absolute as f32 * 3.0
                    } else {
                        vertical.absolute as f32
                    };
                    let scroll = (self.ui.scroll + delta).clamp(0.0, self.painted.max_scroll);
                    if scroll != self.ui.scroll {
                        self.ui.scroll = scroll;
                        self.redraw();
                    }
                }
                _ => {}
            }
        }
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
        self.key(event);
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
    /// ABGR8888 (our RGBA, zero-copy) if offered, else ARGB8888 with a swap.
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
