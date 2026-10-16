//! cnde-settings: a window for editing config.toml (ARCHITECTURE.md §8).
//!
//! It writes the same file you'd edit by hand, keeping your comments, and
//! the compositor and shell pick changes up live, as they do for hand
//! edits. Usage: `cnde-settings [page]`, e.g. `cnde-settings displays`.

mod pages;
mod view;
mod wallpapers;

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
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_dispatch2, delegate_registry,
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
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
        protocols::xdg::shell::client::xdg_toplevel::ResizeEdge,
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
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tracing::{info, warn};

use crate::{
    pages::{Control, Display, Key, Page, Value},
    view::{Edit, Painted, Res, Target, Ui},
};

const DEFAULT_SIZE: (u32, u32) = (920, 680);
const MIN_SIZE: (u32, u32) = (640, 420);
/// How close to the window's edge a press resizes it.
const RESIZE_BORDER: f64 = 6.0;
const ERROR_TIMEOUT: Duration = Duration::from_secs(6);
/// Two header clicks this close together maximize.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

struct App {
    registry: RegistryState,
    outputs: OutputState,
    seat: SeatState,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<App>,
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
    configured: bool,
    maximized: bool,
    frame_pending: bool,
    dirty: bool,

    seat_handle: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    pointer_pos: (f64, f64),
    /// The slider being dragged: its row's key, and where the slider is.
    dragging: Option<(Key, cnde_ui::Rect)>,
    last_header_click: Option<Instant>,
    error_timer: Option<RegistrationToken>,
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
    let theme = theme_for(&config);
    let fonts = Fonts::new(theme.font_families);

    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestClient, &qh);
    window.set_title("Settings");
    window.set_app_id("cnde-settings");
    window.set_min_size(Some(MIN_SIZE));
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

    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seat: SeatState::new(&globals, &qh),
        pool: SlotPool::new((DEFAULT_SIZE.0 * DEFAULT_SIZE.1 * 4) as usize, &shm)?,
        shm,
        qh: qh.clone(),
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
            dragging: None,
        },
        res: Res {
            fonts,
            icons: Icons::new(),
            theme,
            anim: view::Anim::new(),
        },
        painted: Painted::default(),
        config_path,
        size: DEFAULT_SIZE,
        scale: 1,
        configured: false,
        maximized: false,
        frame_pending: false,
        dirty: false,
        seat_handle: None,
        pointer: None,
        keyboard: None,
        modifiers: Modifiers::default(),
        pointer_pos: (0.0, 0.0),
        dragging: None,
        last_header_click: None,
        error_timer: None,
    };
    WaylandSource::new(conn, queue).insert(event_loop.handle())?;
    watch_config(&mut app);

    event_loop.run(None, &mut app, |_| {})?;
    Ok(())
}

impl App {
    fn redraw(&mut self) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let scale = self.scale.max(1);
        let (w, h) = (self.size.0 as i32 * scale, self.size.1 as i32 * scale);
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
        // Something's moving: paint again on the next frame callback.
        if self.res.anim.running() {
            self.dirty = true;
        }
        // Scrolled past the end (the page got shorter): clamp and repaint.
        if self.ui.scroll > self.painted.max_scroll {
            self.ui.scroll = self.painted.max_scroll;
            self.dirty = true;
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = self.window.wl_surface();
        surface.set_buffer_scale(scale);
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
        let Some(path) = self.config_path.clone() else {
            self.show_error("No config file: HOME isn't set".into());
            return;
        };
        let result = ConfigFile::open(&path).and_then(|mut file| {
            // What the file says now, in case it was edited by hand since.
            let before = file.config().unwrap_or_else(|_| self.ui.config.clone());
            for (key, value) in changes {
                key.write(&mut file, &before, value);
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
        if config.appearance.color_scheme != self.ui.config.appearance.color_scheme {
            self.res.theme = theme_for(&config);
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
                if let Some(mode) = info.modes.iter().find(|m| m.current) {
                    parts.push(format!(
                        "{}×{} at {:.0} Hz",
                        mode.dimensions.0,
                        mode.dimensions.1,
                        mode.refresh_rate as f64 / 1000.0
                    ));
                }
                Some(Display {
                    name,
                    description: parts.join(" · "),
                })
            })
            .collect();
        displays.sort_by(|a, b| a.name.cmp(&b.name));
        if displays != self.ui.displays {
            self.ui.displays = displays;
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
                    self.ui.page = page;
                    self.res
                        .anim
                        .restart(view::AnimKey::Page, 0.0, 1.0, cnde_ui::anim::MEDIUM);
                    self.ui.scroll = 0.0;
                }
            }
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

    /// Set the dragged slider from the pointer's x.
    fn drag(&mut self, x: f64) {
        let Some((key, rect)) = self.dragging.clone() else {
            return;
        };
        let Some(row) = self.painted.rows.iter().find(|r| r.key == key) else {
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
                self.commit_edit();
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

fn theme_for(config: &Config) -> Theme {
    Theme::for_appearance(&config.appearance)
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
        self.ui.rounded =
            !(configure.is_maximized() || configure.is_fullscreen() || configure.is_tiled());
        if !self.configured {
            self.configured = true;
            self.choose_shm_format();
        }
        // A new size must be answered with a buffer even mid-frame.
        self.frame_pending = false;
        self.redraw();
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
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat.get_pointer(qh, &seat).ok();
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
            pointer.release();
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
                    if self.dragging.is_some() {
                        self.drag(event.position.0);
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
