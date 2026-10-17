//! cnde-lock: the lock screen. Locks with ext-session-lock (the compositor
//! then shows only this, on every monitor, until it unlocks), shows the
//! time over your wallpaper, and unlocks when PAM accepts your password.
//!
//! If it crashes while locked, the screen stays locked and the compositor
//! starts it again.

mod pam;

use std::{path::PathBuf, time::Duration};

use chrono::Local;
use cnde_config::{ColorScheme, Config};
use cnde_ui::{
    Canvas, Color, Fonts, Icons, Painter, Theme, Weight,
    tiny_skia::{IntSize, Pixmap},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, LoopSignal, channel,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use tracing::{error, info, warn};

/// PAM service: our own if installed, else login's (what most lockers fall
/// back to).
const SERVICES: [&str; 2] = ["cnde-lock", "login"];

struct Surface {
    lock_surface: SessionLockSurface,
    output: wl_output::WlOutput,
    size: (u32, u32),
    scale: i32,
    configured: bool,
}

/// What's been typed; wiped when dropped or cleared.
#[derive(Default)]
struct Secret(Vec<u8>);

impl Secret {
    fn clear(&mut self) {
        for b in &mut self.0 {
            // SAFETY: a plain write to our own memory; volatile so it stays.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        self.0.clear();
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.clear();
    }
}

struct App {
    registry: RegistryState,
    outputs: OutputState,
    seat: SeatState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    shm_format: wl_shm::Format,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    exit: LoopSignal,

    lock: Option<SessionLock>,
    locked: bool,
    surfaces: Vec<Surface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,

    theme: Theme,
    fonts: Fonts,
    icons: Icons,
    /// The wallpaper, tiny and blurred (stretched to the screen it looks
    /// like frosted glass).
    background: Option<Pixmap>,
    twelve_hour: bool,
    user: String,

    password: Secret,
    checking: bool,
    message: Option<String>,
    auth_tx: channel::Sender<Result<(), String>>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = cnde_config::default_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| Config::parse(&text).ok())
        .unwrap_or_default();
    let theme = Theme::for_appearance(&config.appearance);
    let mut icons = Icons::new();
    icons.set_theme(&config.appearance.icon_theme);

    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;

    let lock_state = SessionLockState::new(&globals, &qh);
    let shm = Shm::bind(&globals, &qh)?;

    // Passwords are checked on another thread; the answer comes back here.
    let (auth_tx, auth_rx) = channel::channel::<Result<(), String>>();
    event_loop
        .handle()
        .insert_source(auth_rx, |event, _, app: &mut App| {
            if let channel::Event::Msg(result) = event {
                app.checked(result);
            }
        })?;

    // The wallpaper, blurred, off the main thread: the lock shows at once
    // and the picture fades in when it's ready.
    let (bg_tx, bg_rx) = channel::channel::<Pixmap>();
    event_loop
        .handle()
        .insert_source(bg_rx, |event, _, app: &mut App| {
            if let channel::Event::Msg(pixmap) = event {
                app.background = Some(pixmap);
                app.draw_all();
            }
        })?;
    if let Some(path) = wallpaper_path(&config) {
        std::thread::spawn(move || {
            if let Some(pixmap) = blurred(&path) {
                let _ = bg_tx.send(pixmap);
            }
        });
    }

    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seat: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh)?,
        pool: SlotPool::new(4 * 1024 * 1024, &shm)?,
        shm,
        shm_format: wl_shm::Format::Argb8888,
        qh: qh.clone(),
        loop_handle: event_loop.handle(),
        exit: event_loop.get_signal(),
        lock: None,
        locked: false,
        surfaces: Vec::new(),
        keyboard: None,
        modifiers: Modifiers::default(),
        fonts: Fonts::new(theme.font_families),
        theme,
        icons,
        background: None,
        twelve_hour: config.panel.clock_format.contains("%I")
            || config.panel.clock_format.contains("%l")
            || config.panel.clock_format.contains("%-I"),
        user: user_name(),
        password: Secret::default(),
        checking: false,
        message: None,
        auth_tx,
    };
    app.lock = Some(lock_state.lock(&qh)?);
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;

    // The clock: redraw when the minute changes.
    event_loop
        .handle()
        .insert_source(Timer::from_duration(until_next_minute()), |_, _, app| {
            app.draw_all();
            TimeoutAction::ToDuration(until_next_minute())
        })?;

    // Testing aid for debug builds only: type these passwords (comma
    // separated) a second and a half apart, with CNDE_LOCK_TEST_PASSWORD.
    if cfg!(debug_assertions)
        && let Ok(list) = std::env::var("CNDE_LOCK_TEST_TYPE")
    {
        let mut tries: Vec<String> = list.split(',').rev().map(str::to_string).collect();
        event_loop.handle().insert_source(
            Timer::from_duration(Duration::from_millis(1500)),
            move |_, _, app| {
                let Some(next) = tries.pop() else {
                    return TimeoutAction::Drop;
                };
                app.password.clear();
                app.password.0.extend_from_slice(next.as_bytes());
                app.submit();
                TimeoutAction::ToDuration(Duration::from_millis(1500))
            },
        )?;
    }

    event_loop.run(None, &mut app, |_| {})?;
    // Make sure the unlock reaches the compositor before we go.
    let _ = conn.flush();
    Ok(())
}

fn until_next_minute() -> Duration {
    use chrono::Timelike;
    let now = Local::now();
    Duration::from_secs(60 - now.second() as u64) + Duration::from_millis(50)
}

fn user_name() -> String {
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        return user;
    }
    // From /etc/passwd by our uid (no libc dependency for one lookup).
    let uid = std::fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
        .unwrap_or(u32::MAX);
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                (fields.get(2)?.parse::<u32>().ok()? == uid).then(|| fields[0].to_string())
            })
        })
        .unwrap_or_default()
}

fn wallpaper_path(config: &Config) -> Option<PathBuf> {
    let w = &config.wallpaper;
    let path = match config.appearance.color_scheme {
        ColorScheme::Dark if !w.dark.is_empty() => &w.dark,
        _ => &w.light,
    };
    if path.is_empty() {
        return None;
    }
    Some(match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => PathBuf::from(path),
    })
}

/// The picture shrunk to a few dozen pixels across and box-blurred:
/// stretched over a screen (bilinear), it reads as frosted glass, for a
/// few kilobytes.
fn blurred(path: &std::path::Path) -> Option<Pixmap> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let small = image.thumbnail(96, 96).into_rgba8();
    let (w, h) = small.dimensions();
    let mut data = small.into_raw();
    for _ in 0..3 {
        box_blur(&mut data, w as usize, h as usize);
    }
    // Premultiplied for tiny-skia (pictures are opaque, so nothing to do
    // but be sure).
    for px in data.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    Pixmap::from_vec(data, IntSize::from_wh(w, h)?)
}

/// One pass of a 3x3 box blur, edges clamped.
fn box_blur(data: &mut [u8], w: usize, h: usize) {
    let source = data.to_vec();
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 3];
            let mut n = 0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let sx = (x as i32 + dx).clamp(0, w as i32 - 1) as usize;
                    let sy = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
                    let i = (sy * w + sx) * 4;
                    for c in 0..3 {
                        sum[c] += source[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let i = (y * w + x) * 4;
            for c in 0..3 {
                data[i + c] = (sum[c] / n) as u8;
            }
        }
    }
}

impl App {
    fn add_surface(&mut self, output: wl_output::WlOutput) {
        let Some(lock) = &self.lock else {
            return;
        };
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        let scale = self
            .outputs
            .info(&output)
            .map_or(1, |info| info.scale_factor.max(1));
        let surface = self.compositor.create_surface(&self.qh);
        let lock_surface = lock.create_lock_surface(surface, &output, &self.qh);
        self.surfaces.push(Surface {
            lock_surface,
            output,
            size: (0, 0),
            scale,
            configured: false,
        });
    }

    fn draw_all(&mut self) {
        for i in 0..self.surfaces.len() {
            self.draw(i);
        }
    }

    fn draw(&mut self, index: usize) {
        let Some(surface) = self.surfaces.get(index) else {
            return;
        };
        if !surface.configured || surface.size.0 == 0 || surface.size.1 == 0 {
            return;
        }
        let scale = surface.scale;
        let (lw, lh) = surface.size;
        let (w, h) = (lw as i32 * scale, lh as i32 * scale);
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate a buffer");
                return;
            }
        };
        if let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) {
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: &self.theme,
            };
            paint(
                &mut p,
                (lw as f32, lh as f32),
                self.background.as_ref(),
                &View {
                    twelve_hour: self.twelve_hour,
                    user: &self.user,
                    typed: self.password.0.len(),
                    checking: self.checking,
                    message: self.message.as_deref(),
                    caps_lock: self.modifiers.caps_lock,
                },
            );
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let wl = surface.lock_surface.wl_surface();
        wl.set_buffer_scale(scale);
        wl.damage_buffer(0, 0, w, h);
        if buffer.attach_to(wl).is_ok() {
            wl.commit();
        }
    }

    fn key(&mut self, event: KeyEvent) {
        if self.checking {
            return;
        }
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.submit(),
            Keysym::Escape => {
                self.password.clear();
                self.message = None;
            }
            Keysym::BackSpace => {
                // Whole characters, not bytes.
                let text = String::from_utf8_lossy(&self.password.0).into_owned();
                let keep = text.char_indices().last().map_or(0, |(i, _)| i);
                for b in &mut self.password.0[keep..] {
                    // SAFETY: as in Secret::clear.
                    unsafe { std::ptr::write_volatile(b, 0) };
                }
                self.password.0.truncate(keep);
            }
            Keysym::u if self.modifiers.ctrl => self.password.clear(),
            _ => {
                if self.modifiers.ctrl || self.modifiers.alt {
                    return;
                }
                let Some(text) = event.utf8.filter(|t| !t.chars().any(char::is_control)) else {
                    return;
                };
                self.password.0.extend_from_slice(text.as_bytes());
                self.message = None;
            }
        }
        self.draw_all();
    }

    fn submit(&mut self) {
        if self.password.0.is_empty() {
            return;
        }
        self.checking = true;
        self.message = None;
        let password = std::mem::take(&mut self.password);
        let user = self.user.clone();
        let tx = self.auth_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(check(&user, &password.0));
        });
        self.draw_all();
    }

    fn checked(&mut self, result: Result<(), String>) {
        self.checking = false;
        match result {
            Ok(()) => {
                info!("unlocking");
                if let Some(lock) = self.lock.take() {
                    lock.unlock();
                }
                self.exit.stop();
            }
            Err(err) => {
                warn!(%err, "not unlocked");
                self.message = Some(if err.contains("Authentication failure") {
                    "Wrong password".into()
                } else {
                    err
                });
                self.draw_all();
            }
        }
    }
}

fn check(user: &str, password: &[u8]) -> Result<(), String> {
    // Testing aid for debug builds only: a fixed password, never PAM (a
    // test typing wrong passwords could otherwise lock the account).
    if cfg!(debug_assertions)
        && let Ok(test) = std::env::var("CNDE_LOCK_TEST_PASSWORD")
    {
        std::thread::sleep(Duration::from_millis(300));
        return if test.as_bytes() == password {
            Ok(())
        } else {
            Err("Authentication failure".into())
        };
    }
    let service = SERVICES
        .into_iter()
        .find(|s| std::path::Path::new("/etc/pam.d").join(s).exists())
        .unwrap_or("login");
    pam::authenticate(service, user, password)
}

struct View<'a> {
    twelve_hour: bool,
    user: &'a str,
    typed: usize,
    checking: bool,
    message: Option<&'a str>,
    caps_lock: bool,
}

/// The lock screen: the blurred wallpaper (or the theme's colours), the
/// time and date, and the password pill.
fn paint(p: &mut Painter<'_, '_>, (w, h): (f32, f32), background: Option<&Pixmap>, v: &View<'_>) {
    let t = p.theme;
    match background {
        Some(image) => p.canvas.image(image.as_ref(), 0.0, 0.0, w, h, 0.0),
        None => {
            // Top to bottom, from the sidebar colour to a hint of accent,
            // in strips too thin to see.
            let bottom = t.window_background.mix(t.accent, 0.2);
            const STRIP: f32 = 2.0;
            let mut y = 0.0;
            while y < h {
                let color = t.window_sidebar.mix(bottom, y / h);
                // A little overlap so fractional scales leave no seams.
                p.canvas.rounded_rect(0.0, y, w, STRIP + 1.0, 0.0, color);
                y += STRIP;
            }
        }
    }
    // Darken for legible white text on any picture.
    p.canvas
        .rounded_rect(0.0, 0.0, w, h, 0.0, Color::rgba(0, 0, 0, 0.35));
    let white = Color::rgb(0xff, 0xff, 0xff);
    let soft = Color::rgba(0xff, 0xff, 0xff, 0.75);

    let now = Local::now();
    let time = if v.twelve_hour {
        now.format("%-I:%M").to_string()
    } else {
        now.format("%H:%M").to_string()
    };
    let date = now.format("%A, %B %-d").to_string();
    let cx = w / 2.0;
    let top = h * 0.3;
    let centered = |p: &mut Painter<'_, '_>, text: &str, cy: f32, size: f32, weight, color| {
        let tw = p.measure(text, size, weight);
        p.label_v(text, cx - tw / 2.0, cy, tw + 1.0, size, weight, color);
    };
    centered(p, &time, top, 84.0, Weight::Bold, white);
    centered(p, &date, top + 64.0, 20.0, Weight::Normal, soft);

    // The password pill.
    let pill_w = 300.0_f32.min(w - 32.0);
    let pill_h = 44.0;
    let py = h * 0.62;
    let px = cx - pill_w / 2.0;
    if !v.user.is_empty() {
        centered(p, v.user, py - 22.0, 15.0, Weight::Bold, white);
    }
    p.canvas.rounded_rect(
        px,
        py,
        pill_w,
        pill_h,
        pill_h / 2.0,
        Color::rgba(0xff, 0xff, 0xff, 0.16),
    );
    let ring = if v.message.is_some() {
        t.error
    } else if v.typed > 0 || v.checking {
        t.accent
    } else {
        Color::rgba(0xff, 0xff, 0xff, 0.3)
    };
    p.canvas
        .stroke_rounded_rect(px, py, pill_w, pill_h, pill_h / 2.0, 1.5, ring);
    let cy = py + pill_h / 2.0;
    if v.checking {
        centered(p, "Checking…", cy, 15.0, Weight::Normal, soft);
    } else if v.typed == 0 {
        centered(p, "Password", cy, 15.0, Weight::Normal, soft);
    } else {
        // A dot per byte typed, capped to fit.
        let dots = v.typed.min(((pill_w - 40.0) / 14.0) as usize);
        let span = dots as f32 * 14.0 - 6.0;
        for i in 0..dots {
            p.canvas
                .circle(cx - span / 2.0 + 4.0 + i as f32 * 14.0, cy, 4.0, white);
        }
    }

    let below = py + pill_h + 24.0;
    if let Some(message) = v.message {
        centered(p, message, below, 14.0, Weight::Normal, white);
    } else if v.caps_lock {
        centered(p, "Caps Lock is on", below, 14.0, Weight::Normal, soft);
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        info!("locked");
        self.locked = true;
        // ABGR8888 is our RGBA as is; ARGB8888 (always there) needs a swap.
        if self.shm.formats().contains(&wl_shm::Format::Abgr8888) {
            self.shm_format = wl_shm::Format::Abgr8888;
        }
        let outputs: Vec<wl_output::WlOutput> = self.outputs.outputs().collect();
        for output in outputs {
            self.add_surface(output);
        }
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        if !self.locked {
            error!("the compositor won't lock (is another locker running?)");
        }
        self.lock = None;
        self.exit.stop();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let Some(i) = self
            .surfaces
            .iter()
            .position(|s| s.lock_surface.wl_surface() == surface.wl_surface())
        else {
            return;
        };
        self.surfaces[i].size = configure.new_size;
        self.surfaces[i].configured = true;
        self.draw(i);
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
            .surfaces
            .iter()
            .position(|s| s.lock_surface.wl_surface() == surface)
        {
            self.surfaces[i].scale = factor.max(1);
            self.draw(i);
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // A monitor plugged in while locked gets a lock screen too.
        if self.locked {
            self.add_surface(output);
        }
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|s| s.output != output);
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
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
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
        let caps_changed = modifiers.caps_lock != self.modifiers.caps_lock;
        self.modifiers = modifiers;
        if caps_changed {
            self.draw_all();
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
