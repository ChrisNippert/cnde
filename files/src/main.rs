//! cnde-files: the file manager. `cnde-files [PATH]` opens PATH (default:
//! your home folder).
//!
//! Anything that touches the disk (listing folders, drive sizes, ejecting)
//! runs on a thread and reports back through one calloop channel, so the
//! window never waits on a slow disk.

mod folder;
mod model;
mod pages;
mod search;
mod view;

use std::{
    os::{fd::OwnedFd, unix::process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use cnde_config::Config;
use cnde_ui::{Canvas, Icons, Theme};
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
    folder::{Folder, Location},
    model::Entry,
    pages::Pages,
    search::{Keyed, Search},
    view::{Painted, Place, Res, Target, Ui, ViewMode},
};

const DEFAULT_SIZE: (u32, u32) = (1000, 660);
const MIN_SIZE: (u32, u32) = (640, 420);
/// How close to the window's edge a press resizes it.
const RESIZE_BORDER: f64 = 6.0;
const ERROR_TIMEOUT: Duration = Duration::from_secs(6);
/// Two clicks this close together are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// While resizing, a configure waits this long for the last frame to show
/// before painting anyway.
const CONFIGURE_WAIT: Duration = Duration::from_millis(40);
/// Mouse side buttons (linux/input-event-codes.h).
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// A wheel click scrolls this many logical px.
const WHEEL_STEP: f32 = 60.0;
/// How quickly smooth scrolling catches up with where the wheel sent it.
const SCROLL_EASE: f32 = 0.07;
const DRIVE_POLL: Duration = Duration::from_secs(3);

/// What the background threads report.
enum Event {
    /// Part of a folder's listing (generation, entries).
    Batch(u64, Vec<Entry>),
    Done(u64, std::io::Result<()>),
    Drives(Vec<model::Drive>),
    Trash(u64),
    Places(Vec<Place>),
    Recent(Vec<pages::RecentRow>),
    Error(String),
    Screen(Option<cnde_ui::Color>),
}

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
    events: channel::Sender<Event>,

    ui: Ui,
    res: Res,
    painted: Painted,
    config_path: Option<PathBuf>,

    /// Which listing is current; batches from older ones are dropped.
    generation: u64,
    cancel: Arc<AtomicBool>,
    back: Vec<Location>,
    forward: Vec<Location>,
    /// A name to select once the listing has it (the folder we came from).
    reveal: Option<String>,
    scroll_target: f32,
    scrolling: bool,
    last_paint: Instant,
    last_click: Option<(Instant, usize)>,

    size: (u32, u32),
    scale: i32,
    fractional: Option<f64>,
    viewport: Option<WpViewport>,
    _fractional_object: Option<WpFractionalScaleV1>,
    configured: bool,
    maximized: bool,
    frame_pending: bool,
    dirty: bool,

    seat_handle: Option<wl_seat::WlSeat>,
    pointer: Option<ThemedPointer>,
    cursor_surface: Option<wl_surface::WlSurface>,
    cursor: CursorIcon,
    configure_waiting: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    pointer_pos: (f64, f64),
    pointer_inside: bool,
    last_header_click: Option<Instant>,
    error_timer: Option<RegistrationToken>,
    screen: Option<cnde_ui::Color>,
}

/// Why the Wayland connection is dead, if it is.
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

/// The folder to open from the command line: a path or a file:// URI,
/// made absolute (a file opens its folder).
/// The folder named on the command line; none shows the Overview.
fn start_path() -> Option<PathBuf> {
    let arg = std::env::args().nth(1)?;
    let arg = match arg.strip_prefix("file://") {
        Some(rest) => percent_decode(rest),
        None => arg,
    };
    let mut path = PathBuf::from(arg);
    if path.is_relative()
        && let Ok(cwd) = std::env::current_dir()
    {
        path = cwd.join(path);
    }
    if path.is_file() {
        path.pop();
    }
    Some(path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let start = start_path().map_or(Location::Overview, Location::Dir);

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
    let theme = Theme::for_screen(&config.appearance, None);
    let frosted = config.appearance.material == cnde_config::Material::Frosted;
    cnde_ui::anim::set_speed(config.animations.speed as f32);
    let mut icons = Icons::new();
    icons.set_theme(&config.appearance.icon_theme);

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let cursor_surface = compositor.create_surface(&qh);
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestClient, &qh);
    window.set_title("Files");
    window.set_app_id("cnde-files");
    window.set_min_size(Some(MIN_SIZE));
    // Fractional scale draws at the exact pixel size; without both globals,
    // integer scale.
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

    let (tx, rx) = channel::channel::<Event>();
    event_loop
        .handle()
        .insert_source(rx, |event, _, app: &mut App| {
            if let channel::Event::Msg(event) = event {
                app.event(event);
            }
        })?;
    watch_screen(tx.clone());
    start_places(tx.clone(), home.clone());
    start_drives(tx.clone());

    let ui = Ui {
        config,
        location: Location::Overview,
        folder: Folder::new(),
        mode: ViewMode::List,
        info_panel: false,
        drives: Vec::new(),
        trash: None,
        places: vec![Place {
            name: "Home".into(),
            path: home.clone(),
            icon: "user-home-symbolic",
        }],
        home,
        can_back: false,
        can_forward: false,
        hover: None,
        hover_row: None,
        scroll: 0.0,
        side_scroll: 0.0,
        error: None,
        rounded: true,
        frosted,
        keyboard_focus: false,
        pages: Pages::default(),
        search: Search::default(),
    };
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
        events: tx,
        ui,
        res: Res::new(theme, icons),
        painted: Painted::default(),
        config_path,
        generation: 0,
        cancel: Arc::new(AtomicBool::new(false)),
        back: Vec::new(),
        forward: Vec::new(),
        reveal: None,
        scroll_target: 0.0,
        scrolling: false,
        last_paint: Instant::now(),
        last_click: None,
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
        pointer_inside: false,
        last_header_click: None,
        error_timer: None,
        screen: None,
    };
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;
    watch_config(&mut app);
    app.show(start);

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

/// The sidebar's folders: Home and the XDG user directories that exist.
fn start_places(tx: channel::Sender<Event>, home: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("places".into())
        .spawn(move || {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            let text = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();
            let configured = |key: &str| -> Option<PathBuf> {
                let line = text
                    .lines()
                    .find(|l| l.trim_start().starts_with(&format!("{key}=")))?;
                let value = line.split_once('=')?.1.trim().trim_matches('"');
                Some(match value.strip_prefix("$HOME") {
                    Some(rest) => PathBuf::from(format!("{}{rest}", home.display())),
                    None => PathBuf::from(value),
                })
            };
            let mut places = vec![Place {
                name: "Home".into(),
                path: home.clone(),
                icon: "user-home-symbolic",
            }];
            for (key, default, icon) in [
                (
                    "XDG_DOCUMENTS_DIR",
                    "Documents",
                    "folder-documents-symbolic",
                ),
                ("XDG_DOWNLOAD_DIR", "Downloads", "folder-download-symbolic"),
                ("XDG_PICTURES_DIR", "Pictures", "folder-pictures-symbolic"),
                ("XDG_MUSIC_DIR", "Music", "folder-music-symbolic"),
                ("XDG_VIDEOS_DIR", "Videos", "folder-videos-symbolic"),
            ] {
                let path = configured(key).unwrap_or_else(|| home.join(default));
                if path != home && path.is_dir() {
                    let name = path
                        .file_name()
                        .map_or_else(|| default.into(), |n| n.to_string_lossy().into_owned());
                    places.push(Place { name, path, icon });
                }
            }
            let _ = tx.send(Event::Places(places));
        });
}

/// Drive sizes every few seconds, and the Trash's size, off the main thread.
fn start_drives(tx: channel::Sender<Event>) {
    let _ = std::thread::Builder::new()
        .name("drives".into())
        .spawn(move || {
            let mut last = None;
            let mut last_trash = None;
            let mut tick = 0u32;
            loop {
                let drives = model::drives();
                let key: Vec<_> = drives
                    .iter()
                    .map(|d| {
                        (
                            d.name.clone(),
                            d.mount.clone(),
                            d.total,
                            d.free,
                            d.removable,
                        )
                    })
                    .collect();
                if last.as_ref() != Some(&key) {
                    last = Some(key);
                    if tx.send(Event::Drives(drives)).is_err() {
                        return;
                    }
                }
                if tick.is_multiple_of(5) {
                    let size = model::trash_size();
                    if last_trash != Some(size) {
                        last_trash = Some(size);
                        if tx.send(Event::Trash(size)).is_err() {
                            return;
                        }
                    }
                }
                tick += 1;
                std::thread::sleep(DRIVE_POLL);
            }
        });
}

fn copy_drive(d: &model::Drive) -> model::Drive {
    model::Drive {
        name: d.name.clone(),
        mount: d.mount.clone(),
        device: d.device.clone(),
        fs_type: d.fs_type.clone(),
        total: d.total,
        free: d.free,
        removable: d.removable,
    }
}

impl App {
    fn event(&mut self, event: Event) {
        match event {
            Event::Batch(generation, entries) if generation == self.generation => {
                self.ui.folder.add(entries);
                self.reveal_pending();
                self.redraw();
            }
            Event::Done(generation, result) if generation == self.generation => {
                self.ui.folder.loading = false;
                if let Err(err) = result {
                    self.ui.folder.error = Some(format!("Can't open this folder: {err}"));
                }
                self.reveal_pending();
                self.redraw();
            }
            Event::Batch(..) | Event::Done(..) => {}
            Event::Drives(drives) => {
                self.ui.drives = drives;
                self.redraw();
            }
            Event::Trash(size) => {
                self.ui.trash = Some(size);
                self.redraw();
            }
            Event::Recent(rows) => {
                self.ui.pages.recent = Some(rows);
                self.redraw();
            }
            Event::Places(places) => {
                self.ui.places = places;
                self.redraw();
            }
            Event::Error(message) => self.show_error(message),
            Event::Screen(screen) => {
                if screen != self.screen {
                    self.screen = screen;
                    if self.ui.config.appearance.adaptive() {
                        self.res.theme = Theme::for_screen(&self.ui.config.appearance, screen);
                        self.res.text.clear();
                        self.redraw();
                    }
                }
            }
        }
    }

    /// Select the folder we came from once it's in the listing.
    fn reveal_pending(&mut self) {
        let Some(name) = &self.reveal else {
            return;
        };
        let found = self
            .ui
            .folder
            .visible
            .iter()
            .position(|&i| self.ui.folder.entries[i].name == *name);
        if let Some(pos) = found {
            self.reveal = None;
            self.ui.folder.select_only(pos);
            self.ensure_visible(pos);
        }
    }

    /// Start listing `path` on a thread; whatever was loading stops.
    fn open_dir(&mut self, path: PathBuf) {
        debug!(path = %path.display(), "opening folder");
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        let old = &self.ui.folder;
        let mut folder = Folder::new();
        folder.sort = old.sort;
        folder.descending = old.descending;
        folder.show_hidden = old.show_hidden;
        folder.query = old.query.clone();
        self.ui.folder = folder;
        self.ui.hover_row = None;
        let (generation, cancel, tx) = (self.generation, self.cancel.clone(), self.events.clone());
        let spawned = std::thread::Builder::new()
            .name("listing".into())
            .spawn(move || {
                let result = model::list_dir(&path, &cancel, |batch| {
                    let _ = tx.send(Event::Batch(generation, batch));
                });
                let _ = tx.send(Event::Done(generation, result));
            });
        if spawned.is_err() {
            self.ui.folder.loading = false;
            self.ui.folder.error = Some("Can't start listing this folder".into());
        }
    }

    /// Read the recent files on a thread, for the Overview and Recent pages.
    fn load_recent(&mut self) {
        let (tx, home) = (self.events.clone(), self.ui.home.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Event::Recent(pages::load_recent(&home)));
        });
    }

    /// Show `location`, without touching the history.
    fn show(&mut self, location: Location) {
        self.ui.search.clear();
        self.ui.folder.query.clear();
        if matches!(location, Location::Overview | Location::Recent) {
            self.load_recent();
        }
        match &location {
            Location::Dir(path) => self.open_dir(path.clone()),
            _ => {
                self.cancel.store(true, Ordering::Relaxed);
                self.generation += 1;
                self.ui.folder = Folder::new();
                self.ui.folder.loading = false;
            }
        }
        self.ui.location = location;
        self.ui.can_back = !self.back.is_empty();
        self.ui.can_forward = !self.forward.is_empty();
        self.ui.scroll = 0.0;
        self.scroll_target = 0.0;
        self.redraw();
    }

    /// Go somewhere new, with a way back.
    fn go(&mut self, location: Location) {
        if location == self.ui.location {
            return;
        }
        self.back.push(self.ui.location.clone());
        self.forward.clear();
        self.show(location);
    }

    fn go_back(&mut self) {
        if let Some(previous) = self.back.pop() {
            self.forward.push(self.ui.location.clone());
            self.show(previous);
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.ui.location.clone());
            self.show(next);
        }
    }

    fn go_parent(&mut self) {
        let Location::Dir(path) = &self.ui.location else {
            return;
        };
        if let Some(parent) = path.parent() {
            self.reveal = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let parent = parent.to_path_buf();
            self.go(Location::Dir(parent));
        }
    }

    /// Open the row at `pos`: folders navigate, files go to xdg-open.
    fn open_row(&mut self, pos: usize) {
        let Some(entry) = self.ui.folder.entry(pos) else {
            return;
        };
        let (path, is_dir) = (entry.path.clone(), entry.is_dir);
        self.open_path(path, is_dir);
    }

    /// Folders navigate, files go to xdg-open.
    fn open_path(&mut self, path: PathBuf, is_dir: bool) {
        if is_dir {
            self.go(Location::Dir(path));
        } else {
            let tx = self.events.clone();
            match Command::new("xdg-open")
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
            {
                // Reaped on a thread, so it never lingers as a zombie.
                Ok(mut child) => {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                Err(err) => {
                    let _ = tx.send(Event::Error(format!("Can't run xdg-open: {err}")));
                }
            }
        }
    }

    fn eject(&mut self, index: usize) {
        let Some(drive) = self.ui.drives.get(index).map(copy_drive) else {
            return;
        };
        let tx = self.events.clone();
        std::thread::spawn(move || match model::eject(&drive) {
            Ok(()) => {
                let _ = tx.send(Event::Drives(model::drives()));
            }
            Err(err) => {
                let _ = tx.send(Event::Error(format!("Can't eject {}: {err}", drive.name)));
            }
        });
    }

    fn list_height(&self) -> f32 {
        view::list_rect(self.size.0 as f32, self.size.1 as f32).h
    }

    /// Scroll the list by `delta` logical px: eased for a wheel, at once for
    /// a touchpad.
    fn scroll_by(&mut self, delta: f32, smooth: bool) {
        let max = view::max_scroll(&self.ui, self.size.1 as f32);
        self.scroll_target = (self.scroll_target + delta).clamp(0.0, max);
        if !smooth {
            self.ui.scroll = self.scroll_target;
        }
        self.redraw();
    }

    /// Scroll so row `pos` shows.
    fn ensure_visible(&mut self, pos: usize) {
        let (top, bottom) = (
            pos as f32 * view::ROW_H,
            (pos + 1) as f32 * view::ROW_H + 8.0,
        );
        let h = self.list_height();
        let max = view::max_scroll(&self.ui, self.size.1 as f32);
        let mut target = self.scroll_target;
        if top < target {
            target = top;
        } else if bottom > target + h {
            target = bottom - h;
        }
        self.scroll_target = target.clamp(0.0, max);
    }

    fn redraw(&mut self) {
        self.ui.pages.width = self.size.0 as f32;
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let started = Instant::now();
        // Smooth scrolling: ease towards the target, one step a frame.
        let max = view::max_scroll(&self.ui, self.size.1 as f32);
        self.scroll_target = self.scroll_target.min(max);
        self.ui.scroll = self.ui.scroll.min(max);
        self.ui.side_scroll = self
            .ui
            .side_scroll
            .min(view::side_max_scroll(&self.ui, self.size.1 as f32));
        let diff = self.scroll_target - self.ui.scroll;
        if diff.abs() > 0.3 {
            let dt = if self.scrolling {
                (started - self.last_paint).as_secs_f32().clamp(0.001, 0.05)
            } else {
                1.0 / 60.0
            };
            self.ui.scroll += diff * (1.0 - (-dt / SCROLL_EASE).exp());
            self.scrolling = true;
            self.dirty = true;
        } else {
            self.ui.scroll = self.scroll_target;
            self.scrolling = false;
        }
        self.last_paint = started;
        // The row under a still pointer changes as the list moves.
        self.ui.hover_row = self.row_at(self.pointer_pos);

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
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        debug!(
            paint_ms = painted_in.as_secs_f64() * 1000.0,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
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

    fn set_config(&mut self, config: Config) {
        cnde_ui::anim::set_speed(config.animations.speed as f32);
        if config.appearance != self.ui.config.appearance {
            self.res.theme = Theme::for_screen(&config.appearance, self.screen);
            self.res.icons.set_theme(&config.appearance.icon_theme);
            self.res.text.clear();
            self.ui.frosted = config.appearance.material == cnde_config::Material::Frosted;
        }
        self.ui.config = config;
        self.redraw();
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

    /// The list row at a window position, if it's over one.
    fn row_at(&self, (x, y): (f64, f64)) -> Option<usize> {
        if !self.pointer_inside {
            return None;
        }
        let list = self.painted.list;
        if !list.contains((x, y)) || self.painted.hit((x, y)).is_some() {
            return None;
        }
        let pos = ((y as f32 - list.y - 4.0 + self.ui.scroll) / view::ROW_H).floor();
        (pos >= 0.0 && (pos as usize) < self.ui.folder.visible.len()).then_some(pos as usize)
    }

    fn toggle_maximized(&mut self) {
        if self.maximized {
            self.window.unset_maximized();
        } else {
            self.window.set_maximized();
        }
    }

    fn press(&mut self, position: (f64, f64), serial: u32) {
        self.ui.keyboard_focus = false;
        self.ui.search.focused = false;
        if let Some(edge) = self.resize_edge(position)
            && let Some(seat) = &self.seat_handle
        {
            self.window.resize(seat, serial, edge);
            return;
        }
        let Some(target) = self.painted.hit(position) else {
            self.press_list(position);
            return;
        };
        match target {
            Target::Back => self.go_back(),
            Target::Forward => self.go_forward(),
            Target::Crumb(i) => {
                if let Some(path) = self.painted.crumbs.get(i).cloned() {
                    self.go(Location::Dir(path));
                }
            }
            Target::Mode(mode) => self.ui.mode = mode,
            Target::Info => self.ui.info_panel = !self.ui.info_panel,
            Target::Menu => {}
            Target::Search => {
                // Searching needs a folder: the Overview and Recent have none.
                if !matches!(self.ui.location, Location::Dir(_)) {
                    self.go(Location::Dir(self.ui.home.clone()));
                }
                self.ui.search.focused = true;
                self.ui.search.cursor = self.ui.search.query.chars().count();
            }
            Target::RecentFile(i) => {
                let row = self.ui.pages.recent.as_ref().and_then(|r| r.get(i));
                if let Some((path, is_dir)) = row.map(|r| (r.path.clone(), r.is_dir)) {
                    self.open_path(path, is_dir);
                }
            }
            Target::RecentFolder(i) => {
                let row = self.ui.pages.recent.as_ref().and_then(|r| r.get(i));
                if let Some((dir, name)) = row.and_then(|r| {
                    Some((r.path.parent()?.to_path_buf(), r.name.clone()))
                }) {
                    self.reveal = Some(name);
                    self.go(Location::Dir(dir));
                }
            }
            Target::Close => self.exit.stop(),
            Target::Minimize => self.window.set_minimized(),
            Target::Maximize => self.toggle_maximized(),
            Target::Header => {
                let now = Instant::now();
                if self
                    .last_header_click
                    .is_some_and(|t| now - t < DOUBLE_CLICK)
                {
                    self.last_header_click = None;
                    self.toggle_maximized();
                } else {
                    self.last_header_click = Some(now);
                    if let Some(seat) = &self.seat_handle {
                        self.window.move_(seat, serial);
                    }
                }
            }
            Target::Overview => self.go(Location::Overview),
            Target::Recent => self.go(Location::Recent),
            Target::Place(i) => {
                if let Some(place) = self.ui.places.get(i) {
                    let path = place.path.clone();
                    self.go(Location::Dir(path));
                }
            }
            Target::Drive(i) => {
                if let Some(drive) = self.ui.drives.get(i) {
                    let path = drive.mount.clone();
                    self.go(Location::Dir(path));
                }
            }
            Target::Eject(i) => self.eject(i),
            Target::Connect => self.show_error("Network locations aren't available yet".into()),
            Target::Trash => self.go(Location::Dir(model::trash_dir())),
            Target::Sort(key) => self.ui.folder.set_sort(key),
        }
        self.redraw();
    }

    /// A press in the list: select, or open on a double click.
    fn press_list(&mut self, position: (f64, f64)) {
        let Some(pos) = self.row_at(position) else {
            if self.painted.list.contains(position) {
                self.ui.folder.clear_selection();
                self.redraw();
            }
            return;
        };
        let (ctrl, shift) = (self.modifiers.ctrl, self.modifiers.shift);
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(t, p)| p == pos && now - t < DOUBLE_CLICK);
        if ctrl {
            self.ui.folder.toggle(pos);
        } else if shift {
            self.ui.folder.select_range(pos);
        } else {
            self.ui.folder.select_only(pos);
        }
        if double && !ctrl && !shift {
            self.last_click = None;
            self.open_row(pos);
        } else {
            self.last_click = Some((now, pos));
        }
        self.redraw();
    }

    fn key(&mut self, event: KeyEvent) {
        let (ctrl, alt, shift) = (
            self.modifiers.ctrl,
            self.modifiers.alt,
            self.modifiers.shift,
        );
        let page = (self.list_height() / view::ROW_H).floor().max(1.0) as isize - 1;
        let mut moved = None;
        let in_dir = matches!(self.ui.location, Location::Dir(_));
        let keyed = self
            .ui
            .search
            .key(event.keysym, event.utf8.as_deref(), ctrl, alt, in_dir);
        match keyed {
            Keyed::Ignored => {}
            Keyed::Handled => {
                self.redraw();
                return;
            }
            Keyed::Changed => {
                self.ui.folder.set_query(&self.ui.search.query);
                self.ui.keyboard_focus = true;
                self.ui.scroll = 0.0;
                self.scroll_target = 0.0;
                self.redraw();
                return;
            }
            Keyed::Open => {
                if let Some(pos) = self.ui.folder.cursor {
                    self.open_row(pos);
                }
                self.redraw();
                return;
            }
        }
        match event.keysym {
            Keysym::q | Keysym::w if ctrl => {
                self.exit.stop();
                return;
            }
            Keysym::a | Keysym::A if ctrl => self.ui.folder.select_all(),
            Keysym::h | Keysym::H if ctrl => {
                let show = !self.ui.folder.show_hidden;
                self.ui.folder.set_hidden(show);
            }
            Keysym::F5 | Keysym::r if matches!(self.ui.location, Location::Dir(_)) => {
                if let Location::Dir(path) = self.ui.location.clone() {
                    self.open_dir(path);
                }
            }
            Keysym::Left if alt => self.go_back(),
            Keysym::Right if alt => self.go_forward(),
            Keysym::Up if alt => self.go_parent(),
            Keysym::BackSpace => self.go_parent(),
            Keysym::Escape => self.ui.folder.clear_selection(),
            Keysym::Up => moved = self.ui.folder.step(-1, shift),
            Keysym::Down => moved = self.ui.folder.step(1, shift),
            Keysym::Page_Up => moved = self.ui.folder.step(-page, shift),
            Keysym::Page_Down => moved = self.ui.folder.step(page, shift),
            Keysym::Home => moved = self.ui.folder.go_to(0, shift),
            Keysym::End => {
                let last = self.ui.folder.visible.len().saturating_sub(1);
                moved = self.ui.folder.go_to(last, shift);
            }
            Keysym::Return | Keysym::KP_Enter => {
                if let Some(pos) = self.ui.folder.cursor {
                    self.open_row(pos);
                }
            }
            _ => return,
        }
        self.ui.keyboard_focus = true;
        if let Some(pos) = moved {
            self.ensure_visible(pos);
        }
        self.redraw();
    }

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

/// Follow the compositor's edges event for the adaptive palette's colour.
/// Read on a thread; nothing without cnde's socket.
fn watch_screen(events: channel::Sender<Event>) {
    use std::io::BufRead;
    let Some(path) = std::env::var_os(cnde_ipc::SOCKET_ENV) else {
        return;
    };
    let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || {
            let reader = std::io::BufReader::new(stream);
            for line in reader.split(b'\n').map_while(Result::ok) {
                if let Some(cnde_ipc::Event::Edges { outputs }) = cnde_ipc::decode(&line) {
                    let screen =
                        cnde_ui::tint::screen(outputs.iter().find_map(|e| e.wallpaper_tint));
                    if events.send(Event::Screen(screen)).is_err() {
                        return;
                    }
                }
            }
        });
}

/// Reload when config.toml changes (by hand, or the Settings app).
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
        self.ui.rounded = !(configure.is_maximized()
            || configure.is_fullscreen()
            || configure
                .state
                .intersects(smithay_client_toolkit::reexports::csd_frame::WindowState::TILED));
        if !self.configured {
            self.configured = true;
            self.shm_format = if self.shm.formats().contains(&wl_shm::Format::Abgr8888) {
                wl_shm::Format::Abgr8888
            } else {
                wl_shm::Format::Argb8888
            };
        }
        // Paint at the display's pace while resizing.
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
                    self.pointer_inside = true;
                    let entered = matches!(event.kind, PointerEventKind::Enter { .. });
                    self.update_cursor(entered);
                    let hover = self.painted.hit(event.position);
                    let row = self.row_at(event.position);
                    if hover != self.ui.hover || row != self.ui.hover_row {
                        self.ui.hover = hover;
                        self.ui.hover_row = row;
                        self.redraw();
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.pointer_inside = false;
                    if self.ui.hover.take().is_some() | self.ui.hover_row.take().is_some() {
                        self.redraw();
                    }
                }
                PointerEventKind::Press {
                    button: BTN_LEFT,
                    serial,
                    ..
                } => self.press(event.position, serial),
                PointerEventKind::Press {
                    button: BTN_SIDE, ..
                } => self.go_back(),
                PointerEventKind::Press {
                    button: BTN_EXTRA, ..
                } => self.go_forward(),
                PointerEventKind::Axis {
                    vertical, source, ..
                } => {
                    let wheel = vertical.discrete != 0 || vertical.value120 != 0;
                    let delta = if wheel {
                        vertical.absolute as f32 * (WHEEL_STEP / 15.0)
                    } else {
                        vertical.absolute as f32
                    };
                    if event.position.0 < view::SIDE_W as f64 {
                        let max = view::side_max_scroll(&self.ui, self.size.1 as f32);
                        let scroll = (self.ui.side_scroll + delta).clamp(0.0, max);
                        if scroll != self.ui.side_scroll {
                            self.ui.side_scroll = scroll;
                            self.redraw();
                        }
                    } else {
                        // A wheel eases; a touchpad already is smooth.
                        self.scroll_by(
                            delta,
                            wheel && source != Some(wl_pointer::AxisSource::Finger),
                        );
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

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
delegate_dispatch2!(App);
