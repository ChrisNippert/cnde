//! cnde-share-picker: the "what do you want to share?" window for screen
//! sharing. xdg-desktop-portal-wlr runs it as its chooser (`chooser_type =
//! simple`): it shows a card per monitor with a thumbnail, prints the name
//! of the one picked on stdout and exits 0, or exits 1 when cancelled.
//!
//! Thumbnails are one ext-image-copy-capture frame of each monitor, taken
//! before the overlay opens (so the picker isn't in them) and shrunk on
//! the CPU. If the compositor can't capture, the cards have no preview.

mod layout;
mod picker;
mod thumb;

use std::time::Duration;

use cnde_config::Config;
use cnde_ui::{
    Canvas, Color, Fonts, Icons, Painter, Rect, Theme, Weight,
    tiny_skia::{IntSize, Pixmap},
};
use layout::{CARD_PAD, CARD_W, Hit, THUMB_H, layout};
use picker::{Nav, Outcome, Picker, report};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, LoopSignal,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, Dispatch, QueueHandle, WEnum,
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
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::Buffer, slot::SlotPool},
};
use tracing::{debug, info, warn};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

/// Thumbnails are shrunk to this wide (enough for a 2x screen's card).
const THUMB_PIXELS: usize = 640;
/// How long to wait for the compositor's frames before showing the cards
/// without them.
const CAPTURE_WAIT: Duration = Duration::from_millis(2500);

/// One monitor and its capture.
struct Shot {
    name: String,
    /// "Dell U2723QE" or similar, if the monitor says.
    model: String,
    /// "3840 x 2160", from the monitor's current mode.
    resolution: String,
    output: wl_output::WlOutput,
    size: (u32, u32),
    format: Option<wl_shm::Format>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    buffer: Option<Buffer>,
    done: bool,
    thumb: Option<Pixmap>,
}

struct App {
    registry: RegistryState,
    outputs: OutputState,
    seat: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    exit: LoopSignal,
    code: Option<i32>,

    copy_manager: Option<ExtImageCopyCaptureManagerV1>,
    source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    shots: Vec<Shot>,
    picker: Picker,

    layer: Option<LayerSurface>,
    size: (u32, u32),
    scale: i32,
    hover: Hit,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    theme: Theme,
    fonts: Fonts,
    icons: Icons,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // stdout is the answer; logs go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let config = cnde_config::default_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| Config::parse(&text).ok())
        .unwrap_or_default();
    let theme = Theme::for_appearance(&config.appearance);
    let mut icons = Icons::new();
    icons.set_theme(&config.appearance.icon_theme);

    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
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
        loop_handle: event_loop.handle(),
        exit: event_loop.get_signal(),
        code: None,
        copy_manager: globals.bind(&qh, 1..=1, ()).ok(),
        source_manager: globals.bind(&qh, 1..=1, ()).ok(),
        shots: Vec::new(),
        picker: Picker::new(Vec::new()),
        layer: None,
        size: (0, 0),
        scale: 1,
        hover: Hit::Outside,
        keyboard: None,
        pointer: None,
        fonts: Fonts::new(theme.font_families),
        theme,
        icons,
    };
    // Learn about the monitors (names, modes) before anything else.
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;
    app.start();

    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle())?;
    // Frames that never come shouldn't keep the picker away.
    event_loop
        .handle()
        .insert_source(Timer::from_duration(CAPTURE_WAIT), |_, _, app| {
            if app.layer.is_none() {
                warn!("monitor capture timed out; showing the cards without previews");
                app.open();
            }
            TimeoutAction::Drop
        })?;
    event_loop.run(None, &mut app, |_| {})?;
    let _ = conn.flush();
    std::process::exit(app.code.unwrap_or(1));
}

impl App {
    /// List the monitors and ask for a frame of each; with nothing to
    /// capture with, open right away.
    fn start(&mut self) {
        let mut outputs = self.outputs.outputs().collect::<Vec<_>>();
        outputs.sort_by_key(|o| self.outputs.info(o).map_or(0, |i| i.id));
        for output in outputs {
            let Some(info) = self.outputs.info(&output) else {
                continue;
            };
            let Some(name) = info.name.clone() else {
                continue;
            };
            let model = [&info.make, &info.model]
                .into_iter()
                .filter(|s| !s.is_empty() && *s != "Unknown")
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let resolution = info
                .modes
                .iter()
                .find(|m| m.current)
                .map(|m| format!("{} \u{d7} {}", m.dimensions.0, m.dimensions.1))
                .unwrap_or_default();
            self.scale = self.scale.max(info.scale_factor);
            self.shots.push(Shot {
                name,
                model,
                resolution,
                output,
                size: (0, 0),
                format: None,
                session: None,
                frame: None,
                buffer: None,
                done: false,
                thumb: None,
            });
        }
        self.picker = Picker::new(self.shots.iter().map(|s| s.name.clone()).collect());
        info!(outputs = self.shots.len(), "choosing what to share");
        if self.shots.is_empty() {
            self.finish(Outcome::Cancel);
            return;
        }
        // Testing aid for debug builds: a nested compositor can only serve one
        // capture before its window surface is lost, and the test takes its own.
        let skip =
            cfg!(debug_assertions) && std::env::var_os("CNDE_SHARE_PICKER_NO_PREVIEW").is_some();
        let (Some(sources), Some(copy), false) = (&self.source_manager, &self.copy_manager, skip)
        else {
            debug!("no image capture here; no previews");
            self.open();
            return;
        };
        for (i, shot) in self.shots.iter_mut().enumerate() {
            let source = sources.create_source(&shot.output, &self.qh, ());
            shot.session = Some(copy.create_session(
                &source,
                ext_image_copy_capture_manager_v1::Options::empty(),
                &self.qh,
                i,
            ));
            source.destroy();
        }
    }

    /// The compositor sent the buffer constraints: ask for the frame.
    fn request_frame(&mut self, index: usize) {
        let Some(shot) = self.shots.get_mut(index) else {
            return;
        };
        let (Some(session), Some(format)) = (&shot.session, shot.format) else {
            return self.capture_done(index, None);
        };
        if shot.frame.is_some() || shot.size.0 == 0 || shot.size.1 == 0 {
            return;
        }
        let (w, h) = (shot.size.0 as i32, shot.size.1 as i32);
        let Ok((buffer, _)) = self.pool.create_buffer(w, h, w * 4, format) else {
            return self.capture_done(index, None);
        };
        let frame = session.create_frame(&self.qh, index);
        frame.attach_buffer(buffer.wl_buffer());
        frame.damage_buffer(0, 0, w, h);
        frame.capture();
        shot.frame = Some(frame);
        shot.buffer = Some(buffer);
    }

    fn frame_ready(&mut self, index: usize) {
        let Some(shot) = self.shots.get_mut(index) else {
            return;
        };
        let size = (shot.size.0 as usize, shot.size.1 as usize);
        let thumb = shot
            .buffer
            .as_ref()
            .and_then(|b| b.canvas(&mut self.pool))
            .and_then(|pixels| thumb::downscale(pixels, size, size.0 * 4, THUMB_PIXELS))
            .and_then(|(w, h, data)| Pixmap::from_vec(data, IntSize::from_wh(w, h)?));
        self.capture_done(index, thumb);
    }

    /// This monitor's capture is over (with a picture or without).
    fn capture_done(&mut self, index: usize, thumb: Option<Pixmap>) {
        let Some(shot) = self.shots.get_mut(index) else {
            return;
        };
        if let Some(frame) = shot.frame.take() {
            frame.destroy();
        }
        if let Some(session) = shot.session.take() {
            session.destroy();
        }
        shot.buffer = None;
        shot.done = true;
        debug!(output = %shot.name, preview = thumb.is_some(), "monitor captured");
        shot.thumb = thumb;
        if self.shots.iter().all(|s| s.done) {
            self.open();
        }
    }

    fn open(&mut self) {
        if self.layer.is_some() || self.code.is_some() {
            return;
        }
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            self.compositor.create_surface(&self.qh),
            Layer::Overlay,
            Some("cnde-share-picker"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        self.layer = Some(layer);
    }

    fn finish(&mut self, outcome: Outcome) {
        if self.code.is_some() {
            return;
        }
        let code = report(&mut std::io::stdout().lock(), &outcome).unwrap_or(1);
        info!(?outcome, "done");
        self.code = Some(code);
        self.exit.stop();
    }

    fn pointer_at(&mut self, pos: (f64, f64)) {
        let columns = layout((self.size.0 as f32, self.size.1 as f32), self.shots.len()).columns;
        self.picker.set_columns(columns);
        let hit = self.layout().hit(pos);
        let changed = self.picker.hover(hit) || hit != self.hover;
        self.hover = hit;
        if changed {
            self.draw();
        }
    }

    fn layout(&self) -> layout::Layout {
        layout((self.size.0 as f32, self.size.1 as f32), self.shots.len())
    }

    fn key(&mut self, event: KeyEvent) {
        self.picker.set_columns(self.layout().columns);
        let nav = match event.keysym {
            Keysym::Left => Some(Nav::Left),
            Keysym::Right | Keysym::Tab => Some(Nav::Right),
            Keysym::Up => Some(Nav::Up),
            Keysym::Down => Some(Nav::Down),
            _ => None,
        };
        if let Some(nav) = nav {
            self.picker.navigate(nav);
            self.draw();
            return;
        }
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                if let Some(outcome) = self.picker.confirm() {
                    self.finish(outcome);
                }
            }
            Keysym::Escape => self.finish(Outcome::Cancel),
            _ => {}
        }
    }

    fn draw(&mut self) {
        let Some(layer) = &self.layer else {
            return;
        };
        let (lw, lh) = self.size;
        if lw == 0 || lh == 0 {
            return;
        }
        let scale = self.scale;
        let (w, h) = (lw as i32 * scale, lh as i32 * scale);
        let (buffer, pixels) = match self
            .pool
            .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
        {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate a buffer");
                return;
            }
        };
        pixels.fill(0);
        if let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) {
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: &self.theme,
            };
            let view = View {
                shots: &self.shots,
                selected: self.picker.selected,
                hover: self.hover,
            };
            paint(&mut p, (lw as f32, lh as f32), &view);
        }
        // The canvas draws RGBA; the buffer wants BGRA.
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        let wl = layer.wl_surface();
        wl.set_buffer_scale(scale);
        wl.damage_buffer(0, 0, w, h);
        if buffer.attach_to(wl).is_ok() {
            wl.commit();
        }
    }
}

struct View<'a> {
    shots: &'a [Shot],
    selected: usize,
    hover: Hit,
}

fn paint(p: &mut Painter<'_, '_>, size: (f32, f32), v: &View<'_>) {
    let t = p.theme;
    let l = layout(size, v.shots.len());
    p.canvas.clear(Color::rgba(0, 0, 0, 0.5));

    let panel = l.panel;
    let shadow = Color::rgba(0, 0, 0, 0.18);
    p.canvas.rounded_rect(
        panel.x,
        panel.y + 6.0,
        panel.w,
        panel.h,
        t.qs_radius,
        shadow,
    );
    p.canvas.rounded_rect(
        panel.x,
        panel.y,
        panel.w,
        panel.h,
        t.qs_radius,
        t.qs_background.with_alpha(0.97),
    );
    p.label(
        "Share your screen",
        l.title.0,
        l.title.1,
        20.0,
        Weight::Bold,
        t.popup_text,
    );
    p.label(
        "Pick the screen to show to the app",
        l.title.0,
        l.title.1 + 28.0,
        13.0,
        Weight::Normal,
        t.qs_dim,
    );

    for (i, (card, shot)) in l.cards.iter().zip(v.shots).enumerate() {
        let selected = i == v.selected;
        let fill = if selected {
            t.qs_tile.mix(t.accent, 0.22)
        } else {
            t.qs_tile
        };
        p.canvas
            .rounded_rect(card.x, card.y, card.w, card.h, t.qs_tile_radius, fill);
        let (tx, ty, tw) = (
            card.x + CARD_PAD,
            card.y + CARD_PAD,
            CARD_W - 2.0 * CARD_PAD,
        );
        let inner = (t.qs_tile_radius - 3.0).max(2.0);
        match &shot.thumb {
            Some(thumb) => p.canvas.image(thumb.as_ref(), tx, ty, tw, THUMB_H, inner),
            None => {
                p.canvas
                    .rounded_rect(tx, ty, tw, THUMB_H, inner, t.qs_toggle_off);
                let label = "No preview";
                let lw = p.measure(label, 13.0, Weight::Normal);
                p.label_v(
                    label,
                    tx + (tw - lw) / 2.0,
                    ty + THUMB_H / 2.0,
                    tw,
                    13.0,
                    Weight::Normal,
                    t.qs_dim,
                );
            }
        }
        if selected {
            p.canvas.stroke_rounded_rect(
                card.x,
                card.y,
                card.w,
                card.h,
                t.qs_tile_radius,
                2.5,
                t.accent,
            );
        }
        let text_y = ty + THUMB_H + 10.0;
        p.label_v(
            &shot.name,
            tx,
            text_y + 9.0,
            tw,
            15.0,
            Weight::Bold,
            t.popup_text,
        );
        let detail = [shot.model.as_str(), shot.resolution.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        p.label_v(
            &detail,
            tx,
            text_y + 30.0,
            tw,
            12.5,
            Weight::Normal,
            t.qs_dim,
        );
    }

    let cancel = l.cancel;
    let rect = Rect::new(cancel.x, cancel.y, cancel.w, cancel.h);
    p.button(rect, if v.hover == Hit::Cancel { 1.0 } else { 0.0 });
    let lw = p.measure("Cancel", 14.0, Weight::Normal);
    p.label_v(
        "Cancel",
        rect.x + (rect.w - lw) / 2.0,
        rect.y + rect.h / 2.0,
        rect.w,
        14.0,
        Weight::Normal,
        t.popup_text,
    );
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.finish(Outcome::Cancel);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.size = configure.new_size;
        self.picker.set_columns(self.layout().columns);
        self.draw();
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
        self.scale = factor.max(1);
        self.draw();
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
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
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
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
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
                    self.pointer_at(event.position)
                }
                PointerEventKind::Press {
                    button: BTN_LEFT, ..
                } => {
                    self.pointer_at(event.position);
                    let hit = self.layout().hit(event.position);
                    if let Some(outcome) = self.picker.click(hit) {
                        self.finish(outcome);
                    }
                }
                _ => {}
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

// The capture objects: their events carry the monitor's index as user data.
impl Dispatch<ExtImageCopyCaptureSessionV1, usize> for App {
    fn event(
        app: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        let Some(shot) = app.shots.get_mut(index) else {
            return;
        };
        match event {
            Event::BufferSize { width, height } => shot.size = (width, height),
            Event::ShmFormat { format } => {
                // Either of the two 8-bit formats we can read; ARGB first.
                match format {
                    WEnum::Value(f @ wl_shm::Format::Argb8888) => shot.format = Some(f),
                    WEnum::Value(f @ wl_shm::Format::Xrgb8888) if shot.format.is_none() => {
                        shot.format = Some(f)
                    }
                    _ => {}
                }
            }
            Event::Done => app.request_frame(index),
            Event::Stopped => app.capture_done(index, None),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, usize> for App {
    fn event(
        app: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => app.frame_ready(index),
            Event::Failed { reason } => {
                debug!(?reason, "monitor capture failed");
                app.capture_done(index, None);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtOutputImageCaptureSourceManagerV1,
        _: <ExtOutputImageCaptureSourceManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureManagerV1,
        _: <ExtImageCopyCaptureManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_registry!(App);
delegate_dispatch2!(App);
