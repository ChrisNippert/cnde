//! Notifications: the shell is the session's notification server
//! (org.freedesktop.Notifications) and shows them as cards at the top
//! right, newest first. A click runs the app's default action, a right
//! click dismisses; most go by themselves after a few seconds.
//!
//! The D-Bus side answers on zbus's own thread and hands each one to the
//! main loop. If another server (mako, dunst) already has the name, it
//! keeps it and we show nothing. We hold the name replaceable: a daemon
//! started with `--replace` takes over and we step back, taking the name
//! again once it goes.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use cnde_ui::{
    Canvas, Painter, Rect, Weight,
    tiny_skia::{self, FilterQuality, Pixmap, PixmapPaint, Transform},
};
use smithay_client_toolkit::{
    reexports::{
        calloop::{
            LoopHandle, RegistrationToken,
            channel::{self, Sender},
            timer::{TimeoutAction, Timer},
        },
        client::protocol::{wl_shm, wl_surface::WlSurface},
    },
    compositor::FrameCallbackData,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
    },
};
use tracing::{debug, info, warn};
use zbus::{
    fdo::{RequestNameFlags, RequestNameReply},
    zvariant::{OwnedValue, Structure},
};

use crate::{
    App,
    calendar::Target,
    history::{self, Entry},
    media::Op,
};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

const WIDTH: f32 = 380.0;
/// How far a card slides in from (and out to) the right edge.
const SLIDE: f32 = 40.0;
/// Cards appear, leave and move over this long.
const ANIM: Duration = Duration::from_millis(230);
const MARGIN: i32 = 10;
const PADDING: f32 = 14.0;
const GAP: f32 = 8.0;
const ICON: f32 = 40.0;
const LINE: f32 = 19.0;
/// The stack's surface: the cards and the margin they slide in over.
const SURFACE_WIDTH: f32 = WIDTH + MARGIN as f32;
const BODY_LINES: usize = 4;
/// More than this wait their turn (still timing out).
const SHOWN: usize = 4;
/// When the app leaves it to us (expire_timeout -1).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6);
/// Images are kept at most this big (device pixels).
const IMAGE_MAX: u32 = 128;

/// Why one went away, as the spec numbers them.
#[derive(Clone, Copy)]
enum Reason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
}

pub enum Event {
    /// We have the name: the connection to keep.
    Ready(zbus::blocking::Connection),
    /// Another server took the name from us.
    Lost,
    Notify(Box<Note>),
    Close(u32),
}

pub struct Note {
    id: u32,
    app_name: String,
    summary: String,
    body: String,
    /// (key, label); "default" is the click on the card itself.
    actions: Vec<(String, String)>,
    /// An icon name or a path.
    icon: String,
    image: Option<Pixmap>,
    critical: bool,
    /// `None`: stays until dismissed.
    timeout: Option<Duration>,
    /// Stays after an action is chosen.
    resident: bool,
    /// Not worth keeping in the history (volume and the like).
    transient: bool,
    desktop_entry: Option<String>,
    sound: Option<String>,
}

/// One on screen (or waiting).
struct Card {
    note: Note,
    expire: Option<RegistrationToken>,
    /// Wrapped body, measured once.
    lines: Vec<String>,
    height: f32,
    /// When it came into view (`None`: waiting its turn).
    appear: Option<Instant>,
    /// Dismissed or expired: fading out, then gone.
    leaving: Option<Instant>,
    /// Where it sits in the stack: (from, to, since), easing between.
    y: Option<(f32, f32, Instant)>,
}

impl Card {
    fn new(note: Note, expire: Option<RegistrationToken>, lines: Vec<String>, height: f32) -> Self {
        Self {
            note,
            expire,
            lines,
            height,
            appear: None,
            leaving: None,
            y: None,
        }
    }

    /// 0-1: how far in view (it slides and fades with this).
    fn presence(&self, now: Instant, duration: Duration) -> f32 {
        use cnde_ui::anim::{ease_out, progress};
        match (self.leaving, self.appear) {
            (Some(at), _) => 1.0 - ease_out(progress(at, duration, now)),
            (None, Some(at)) => ease_out(progress(at, duration, now)),
            (None, None) => 0.0,
        }
    }

    /// Its top edge in the stack now.
    fn top(&self, now: Instant, duration: Duration) -> Option<f32> {
        let (from, to, at) = self.y?;
        let t = cnde_ui::anim::ease_out(cnde_ui::anim::progress(at, duration, now));
        Some(from + (to - from) * t)
    }

    fn animating(&self, now: Instant, duration: Duration) -> bool {
        let moving = |at: Instant| now < at + duration;
        self.leaving.is_some()
            || self.appear.is_some_and(moving)
            || self.y.is_some_and(|(_, _, at)| moving(at))
    }
}

/// Work out where each card wants to be (stacked from the top, newest
/// first, `SHOWN` of them), starting a move for any that's changed.
fn place_cards(cards: &mut [Card], now: Instant, duration: Duration) {
    let mut y = 0.0;
    let mut shown = 0;
    for card in cards.iter_mut().filter(|c| c.leaving.is_none()) {
        if shown == SHOWN {
            // Pushed out of view by a newer one: back to waiting.
            card.appear = None;
            card.y = None;
            continue;
        }
        shown += 1;
        if card.appear.is_none() {
            card.appear = Some(now);
        }
        let current = card.top(now, duration);
        match (card.y, current) {
            (Some((_, to, _)), Some(current)) if to != y => card.y = Some((current, y, now)),
            (None, _) => card.y = Some((y, y, now)),
            _ => {}
        }
        y += card.height + GAP;
    }
}

#[derive(Default)]
pub struct Notifications {
    conn: Option<zbus::blocking::Connection>,
    /// Newest first.
    cards: Vec<Card>,
    pub stack: Option<Stack>,
    /// The D-Bus thread is running (owning the name or waiting for it);
    /// setting this ends it.
    stop: Option<Arc<AtomicBool>>,
    tx: Option<Sender<Event>>,
    /// The output the compositor was last told card positions for (to
    /// frost behind them), and what it was told.
    frost_output: Option<String>,
    frost_sent: Vec<cnde_ipc::CardFrost>,
}

pub struct Stack {
    pub layer: LayerSurface,
    output: Option<String>,
    /// A frame callback is out: draw again when it comes, if still moving.
    frame_pending: bool,
    animating: bool,
    scale: i32,
    configured: bool,
    height: u32,
    /// Where things were drawn, for clicks.
    hits: Vec<Hit>,
    hover: Option<(u32, Part)>,
}

#[derive(Clone, PartialEq)]
enum Part {
    Card,
    Close,
    Action(String),
}

struct Hit {
    id: u32,
    part: Part,
    rect: Rect,
}

/// Set up the channel from D-Bus; the server itself starts if enabled.
pub fn setup(handle: &LoopHandle<'static, App>) -> Notifications {
    let (tx, rx) = channel::channel::<Event>();
    let inserted = handle.insert_source(rx, |event, _, app: &mut App| {
        if let channel::Event::Msg(event) = event {
            app.notification_event(event);
        }
    });
    Notifications {
        tx: inserted.is_ok().then_some(tx),
        ..Notifications::default()
    }
}

/// What happened to the name, as far as owning it goes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ownership {
    /// Another server has it, or nobody and we haven't asked yet.
    Out,
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Signal {
    /// The bus took the name from us.
    Lost,
    /// Nobody has the name any more.
    Free,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    Nothing,
    /// Clear our cards and stop answering.
    GiveWay,
    /// Ask for the name.
    Request,
}

/// What a bus signal about the name means for us.
fn hand_off(own: Ownership, signal: Signal) -> (Ownership, Step) {
    match (own, signal) {
        (Ownership::Owner, Signal::Lost) => (Ownership::Out, Step::GiveWay),
        (Ownership::Out, Signal::Free) => (Ownership::Out, Step::Request),
        _ => (own, Step::Nothing),
    }
}

/// Ask for the name, replaceable and without queueing.
fn claim(conn: &zbus::blocking::Connection) -> zbus::Result<bool> {
    let reply = conn.request_name_with_flags(
        NAME,
        RequestNameFlags::AllowReplacement | RequestNameFlags::DoNotQueue,
    )?;
    Ok(matches!(reply, RequestNameReply::PrimaryOwner))
}

/// Serve the name until `stop`: claim it if it's free, step back if
/// another server replaces us, claim it again when that one goes.
fn serve(tx: Sender<Event>, stop: Arc<AtomicBool>) -> zbus::Result<()> {
    let conn = zbus::blocking::Connection::session()?;
    conn.object_server().at(
        PATH,
        Server {
            tx: tx.clone(),
            next_id: AtomicU32::new(1),
        },
    )?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .build();
    let signals = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(64))?;
    let mut own = Ownership::Out;
    if claim(&conn)? {
        own = Ownership::Owner;
        let _ = tx.send(Event::Ready(conn.clone()));
    } else {
        info!("another notification server is running");
    }
    for message in signals.flatten() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let header = message.header();
        let signal = match header.member().map(|m| m.as_str()) {
            Some("NameLost") => message
                .body()
                .deserialize::<String>()
                .is_ok_and(|n| n == NAME)
                .then_some(Signal::Lost),
            Some("NameOwnerChanged") => message
                .body()
                .deserialize::<(String, String, String)>()
                .is_ok_and(|(name, _, new)| name == NAME && new.is_empty())
                .then_some(Signal::Free),
            _ => None,
        };
        let Some(signal) = signal else {
            continue;
        };
        let (next, step) = hand_off(own, signal);
        own = next;
        match step {
            Step::Nothing => {}
            Step::GiveWay => {
                info!("another notification server took over");
                let _ = tx.send(Event::Lost);
            }
            Step::Request => {
                if claim(&conn)? {
                    own = Ownership::Owner;
                    let _ = tx.send(Event::Ready(conn.clone()));
                }
            }
        }
    }
    // Give the name back for whoever's next.
    let _ = conn.release_name(NAME);
    Ok(())
}

struct Server {
    tx: Sender<Event>,
    next_id: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        };
        let hint_str = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| hints.get(*n)?.downcast_ref::<&str>().ok().map(String::from))
                .filter(|s| !s.is_empty())
        };
        let hint_bool = |name: &str| {
            hints
                .get(name)
                .and_then(|v| v.downcast_ref::<bool>().ok())
                .unwrap_or(false)
        };
        let critical = hints
            .get("urgency")
            .and_then(|v| v.downcast_ref::<u8>().ok())
            == Some(2);
        let image = ["image-data", "image_data", "icon_data"]
            .iter()
            .find_map(|n| hints.get(*n).and_then(image_data));
        // An image path, else the app's icon, else its desktop entry's.
        let icon = hint_str(&["image-path", "image_path"])
            .or_else(|| (!app_icon.is_empty()).then_some(app_icon))
            .or_else(|| hint_str(&["desktop-entry"]))
            .map(|icon| {
                icon.strip_prefix("file://")
                    .map(String::from)
                    .unwrap_or(icon)
            })
            .unwrap_or_default();
        let timeout = match expire_timeout {
            0 => None,
            _ if critical => None,
            ms if ms > 0 => Some(Duration::from_millis(ms as u64).max(Duration::from_secs(2))),
            _ => Some(DEFAULT_TIMEOUT),
        };
        let note = Note {
            id,
            app_name,
            summary,
            body,
            actions: actions
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, label]| (key.clone(), label.clone()))
                .collect(),
            icon,
            image,
            critical,
            timeout,
            resident: hint_bool("resident"),
            transient: hint_bool("transient"),
            desktop_entry: hint_str(&["desktop-entry"]),
            sound: (!hint_bool("suppress-sound"))
                .then(|| hint_str(&["sound-name"]))
                .flatten(),
        };
        let _ = self.tx.send(Event::Notify(Box::new(note)));
        id
    }

    fn close_notification(&self, id: u32) {
        let _ = self.tx.send(Event::Close(id));
    }

    fn get_capabilities(&self) -> Vec<&'static str> {
        vec!["actions", "body", "icon-static", "persistence", "sound"]
    }

    fn get_server_information(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        ("cnde", "cnde", env!("CARGO_PKG_VERSION"), "1.2")
    }
}

/// The `image-data` hint, `(iiibiiay)`: width, height, rowstride, alpha,
/// bits per sample, channels, pixels. Shrunk to at most [`IMAGE_MAX`].
fn image_data(value: &OwnedValue) -> Option<Pixmap> {
    let image = value.downcast_ref::<Structure>().ok()?;
    let fields = image.fields();
    let int = |i: usize| fields.get(i)?.downcast_ref::<i32>().ok();
    let (width, height, stride) = (int(0)?, int(1)?, int(2)?);
    let channels = int(5)?;
    if width <= 0 || height <= 0 || int(4)? != 8 || !(3..=4).contains(&channels) {
        return None;
    }
    let data: Vec<u8> = fields.get(6)?.try_clone().ok()?.try_into().ok()?;
    let (w, h, stride, channels) = (
        width as usize,
        height as usize,
        stride as usize,
        channels as usize,
    );
    if data.len() < stride * (h - 1) + w * channels {
        return None;
    }
    let mut full = Pixmap::new(w as u32, h as u32)?;
    for (y, row) in full.pixels_mut().chunks_mut(w).enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let i = y * stride + x * channels;
            let a = if channels == 4 { data[i + 3] } else { 255 };
            *pixel =
                tiny_skia::ColorU8::from_rgba(data[i], data[i + 1], data[i + 2], a).premultiply();
        }
    }
    let zoom = (IMAGE_MAX as f32 / w.max(h) as f32).min(1.0);
    if zoom >= 1.0 {
        return Some(full);
    }
    let (sw, sh) = (
        ((w as f32 * zoom).round() as u32).max(1),
        ((h as f32 * zoom).round() as u32).max(1),
    );
    let mut small = Pixmap::new(sw, sh)?;
    small.draw_pixmap(
        0,
        0,
        full.as_ref(),
        &PixmapPaint {
            quality: FilterQuality::Bicubic,
            ..PixmapPaint::default()
        },
        Transform::from_scale(zoom, zoom),
        None,
    );
    Some(small)
}

impl Stack {
    pub fn owns(&self, surface: &WlSurface) -> bool {
        self.layer.wl_surface() == surface
    }

    fn hit(&self, position: (f64, f64)) -> Option<(u32, Part)> {
        // Buttons are listed after their card: the last match is the most
        // specific.
        self.hits
            .iter()
            .rev()
            .find(|h| h.rect.contains(position))
            .map(|h| (h.id, h.part.clone()))
    }
}

impl App {
    /// Take the name if notifications are on, give it up if they're off.
    pub fn sync_notification_server(&mut self) {
        let n = &mut self.notifications;
        if self.config.notifications.enabled {
            if n.stop.is_none()
                && let Some(tx) = n.tx.clone()
            {
                let stop = Arc::new(AtomicBool::new(false));
                n.stop = Some(stop.clone());
                let _ = std::thread::Builder::new()
                    .name("notifications".into())
                    .spawn(move || {
                        if let Err(err) = serve(tx, stop) {
                            warn!(%err, "can't serve notifications");
                        }
                    });
            }
        } else if let Some(stop) = n.stop.take() {
            // The thread ends at the bus's next word and releases the name;
            // do it now too if we have it.
            stop.store(true, Ordering::Relaxed);
            if let Some(conn) = n.conn.take() {
                let _ = conn.release_name(NAME);
            }
            info!("notifications off");
            self.clear_cards();
        }
        self.status.do_not_disturb = self
            .notifications
            .conn
            .is_some()
            .then_some(self.config.notifications.do_not_disturb);
    }

    fn clear_cards(&mut self) {
        for card in std::mem::take(&mut self.notifications.cards) {
            if let Some(timer) = card.expire {
                self.loop_handle.remove(timer);
            }
        }
        self.drop_stack();
    }

    pub fn notification_event(&mut self, event: Event) {
        match event {
            Event::Ready(conn) => {
                if self.config.notifications.enabled && self.notifications.stop.is_some() {
                    info!("showing notifications");
                    self.notifications.conn = Some(conn);
                }
                self.sync_notification_server();
                self.redraw_all();
            }
            Event::Lost => {
                self.notifications.conn = None;
                self.clear_cards();
                self.sync_notification_server();
                self.redraw_all();
            }
            Event::Notify(note) => self.notify(*note),
            Event::Close(id) => self.close_notification(id, Reason::Closed),
        }
    }

    fn notify(&mut self, note: Note) {
        if self.config.notifications.history {
            self.history.push(
                Entry {
                    seq: 0,
                    id: note.id,
                    app_name: note.app_name.clone(),
                    icon: note.icon.clone(),
                    summary: note.summary.clone(),
                    body: note.body.clone(),
                    at: history::now(),
                    actions: note.actions.clone(),
                    desktop_entry: note.desktop_entry.clone(),
                },
                note.transient,
            );
            // Looking at the list as it arrives: nothing's unread.
            if self.calendar.is_some() {
                self.history.mark_read();
            }
            // Looking at the list as it arrives: nothing's unread.
            if self.calendar.is_some() {
                self.history.mark_read();
            }
            self.draw_calendar();
            self.redraw_panels();
        }
        if self.config.notifications.do_not_disturb && !note.critical {
            // Not kept anywhere yet: as if it had timed out.
            self.notification_closed(note.id, Reason::Expired);
            return;
        }
        if let Some(sound) = &note.sound {
            self.event_sound(sound);
        }
        let id = note.id;
        let lines = self.measure_body(&note);
        let height = card_height(&note, lines.len(), self.theme.button_height);
        let expire = note.timeout.and_then(|after| {
            self.loop_handle
                .insert_source(Timer::from_duration(after), move |_, _, app| {
                    // Not while the pointer's on them: it may be being read.
                    if app
                        .notifications
                        .stack
                        .as_ref()
                        .is_some_and(|s| s.hover.is_some())
                    {
                        return TimeoutAction::ToDuration(Duration::from_secs(1));
                    }
                    if let Some(card) = app.notifications.cards.iter_mut().find(|c| c.note.id == id)
                    {
                        // Removed by the timer returning Drop.
                        card.expire = None;
                    }
                    app.close_notification(id, Reason::Expired);
                    TimeoutAction::Drop
                })
                .ok()
        });
        let mut card = Card::new(note, expire, lines, height);
        let cards = &mut self.notifications.cards;
        match cards
            .iter()
            .position(|c| c.note.id == id && c.leaving.is_none())
        {
            // A replacement keeps its place.
            Some(i) => {
                card.appear = cards[i].appear;
                card.y = cards[i].y;
                if let Some(timer) = std::mem::replace(&mut cards[i], card).expire {
                    self.loop_handle.remove(timer);
                }
            }
            None => cards.insert(0, card),
        }
        self.show_notifications();
    }

    fn close_notification(&mut self, id: u32, reason: Reason) {
        let cards = &mut self.notifications.cards;
        let Some(i) = cards
            .iter()
            .position(|c| c.note.id == id && c.leaving.is_none())
        else {
            return;
        };
        // It fades out where it is (see `draw_notifications`).
        let card = &mut cards[i];
        card.leaving = Some(Instant::now());
        if let Some(timer) = card.expire.take() {
            self.loop_handle.remove(timer);
        }
        if card.appear.is_none() {
            // Never shown: nothing to fade.
            cards.remove(i);
        }
        self.notification_closed(id, reason);
        self.show_notifications();
    }

    fn notification_closed(&self, id: u32, reason: Reason) {
        self.notification_signal("NotificationClosed", &(id, reason as u32));
    }

    fn notification_signal<B>(&self, name: &str, body: &B)
    where
        B: zbus::export::serde::Serialize + zbus::zvariant::DynamicType,
    {
        if let Some(conn) = &self.notifications.conn
            && let Err(err) = conn.emit_signal(None::<&str>, PATH, NAME, name, body)
        {
            warn!(%err, "can't tell the app about its notification");
        }
    }

    fn measure_body(&mut self, note: &Note) -> Vec<String> {
        let mut pixels = [0u8; 4];
        let Some(mut canvas) = Canvas::new(&mut pixels, 1, 1, 1.0) else {
            return Vec::new();
        };
        let mut p = Painter {
            canvas: &mut canvas,
            fonts: &mut self.fonts,
            icons: &mut self.icons,
            theme: &self.theme,
        };
        crate::polkit::wrap(
            &mut p,
            &note.body,
            WIDTH - PADDING * 2.0 - ICON - 12.0,
            self.theme.font_size - 1.0,
            BODY_LINES,
        )
    }

    /// Quick settings and the calendar hang where the cards are: the cards
    /// step aside while one is open.
    fn notifications_covered(&self) -> bool {
        self.quick_settings.is_some() || self.calendar.is_some()
    }

    pub fn notifications_follow_popups(&mut self) {
        let covered = self.notifications_covered();
        if covered == self.notifications.stack.is_none() {
            return;
        }
        if covered {
            self.drop_stack();
        } else {
            self.show_notifications();
        }
    }

    /// Size the stack to the cards (creating or dropping its surface).
    fn show_notifications(&mut self) {
        let mut height = stack_height(&self.notifications.cards);
        let now = Instant::now();
        let duration = cnde_ui::anim::scaled(ANIM);
        // Cards still moving or leaving need the room they're in.
        if let Some(stack) = &self.notifications.stack
            && self
                .notifications
                .cards
                .iter()
                .any(|c| c.animating(now, duration))
        {
            height = height.max(stack.height);
        }
        if height == 0 || self.notifications_covered() {
            self.drop_stack();
            return;
        }
        if let Some(stack) = &mut self.notifications.stack {
            if stack.height != height {
                stack.height = height;
                stack.layer.set_size(SURFACE_WIDTH as u32, height);
                stack.layer.commit();
                // Drawn again when the compositor agrees to the size.
                stack.configured = false;
            } else {
                self.draw_notifications();
            }
            return;
        }
        let Some(panel) = self.panels.first() else {
            return;
        };
        let (output, scale) = (panel.output.clone(), panel.scale());
        let output_name = self.outputs.info(&output).and_then(|i| i.name);
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Top,
            Some("cnde-notifications"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
        // The cards slide in over the margin, so the surface reaches the
        // screen's edge.
        layer.set_margin(MARGIN, 0, 0, 0);
        layer.set_size(SURFACE_WIDTH as u32, height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();
        self.notifications.stack = Some(Stack {
            layer,
            output: output_name,
            frame_pending: false,
            animating: false,
            scale,
            configured: false,
            height,
            hits: Vec::new(),
            hover: None,
        });
    }

    pub fn notifications_configured(&mut self, surface: &WlSurface) {
        if let Some(stack) = &mut self.notifications.stack
            && stack.owns(surface)
        {
            stack.configured = true;
            self.draw_notifications();
        }
    }

    pub fn notifications_closed(&mut self, surface: &WlSurface) {
        if self
            .notifications
            .stack
            .as_ref()
            .is_some_and(|s| s.owns(surface))
        {
            self.drop_stack();
        }
    }

    /// Take the stack down, and the frost behind it.
    fn drop_stack(&mut self) {
        self.notifications.stack = None;
        self.send_card_frost(None, Vec::new());
    }

    /// Tell the compositor where the cards are (when that's changed), so it
    /// frosts behind them.
    fn send_card_frost(&mut self, output: Option<String>, cards: Vec<cnde_ipc::CardFrost>) {
        let n = &mut self.notifications;
        if cards == n.frost_sent && (output.is_none() || output == n.frost_output) {
            return;
        }
        let was = n.frost_output.take();
        n.frost_sent = cards.clone();
        // Moving to another output, or going: clear the old one.
        if let Some(old) = was.filter(|old| Some(old) != output.as_ref()) {
            self.ipc.send(&cnde_ipc::Request::NotificationFrost {
                output: old,
                cards: Vec::new(),
            });
        }
        if let Some(output) = output {
            self.notifications.frost_output = Some(output.clone());
            self.ipc
                .send(&cnde_ipc::Request::NotificationFrost { output, cards });
        }
    }

    /// The compositor is ready for a new frame.
    pub fn notifications_frame(&mut self, surface: &WlSurface) {
        if let Some(stack) = &mut self.notifications.stack
            && stack.owns(surface)
        {
            stack.frame_pending = false;
            if stack.animating {
                self.draw_notifications();
            }
        }
    }

    /// Pointer motion (`None`: it left) over the stack.
    pub fn notifications_motion(&mut self, position: Option<(f64, f64)>) {
        let Some(stack) = &mut self.notifications.stack else {
            return;
        };
        let hover = position.and_then(|p| stack.hit(p));
        if hover != stack.hover {
            stack.hover = hover;
            self.draw_notifications();
        }
    }

    /// A click in the clock's popover that isn't about the month.
    pub fn centre_action(&mut self, target: Target) {
        debug!(?target, "notification centre click");
        match target {
            Target::Item(seq) => {
                let Some(entry) = self.history.get(seq) else {
                    return;
                };
                // The app hears about the click; it raises itself.
                if entry.has_default() {
                    let id = entry.id;
                    self.notification_signal("ActionInvoked", &(id, "default"));
                    self.history.remove(seq);
                }
            }
            Target::Dismiss(seq) => self.history.remove(seq),
            Target::ClearApp(i) => {
                if let Some(app) = self.calendar.as_ref().and_then(|c| c.app_name(i)) {
                    let app = app.to_string();
                    self.history.clear_app(&app);
                }
            }
            Target::ClearAll => self.history.clear(),
            Target::DoNotDisturb => {
                crate::config::set_do_not_disturb(!self.config.notifications.do_not_disturb)
            }
            Target::PlayPause | Target::Next | Target::Previous => {
                if let Some(bus) = self.media.current().map(|p| p.bus.clone()) {
                    let op = match target {
                        Target::Next => Op::Next,
                        Target::Previous => Op::Previous,
                        _ => Op::PlayPause,
                    };
                    self.media.control(&bus, op);
                }
            }
            Target::Pick(i) => {
                let bus = self.media.live().get(i).map(|p| p.bus.clone());
                if bus.is_some() {
                    self.media.selected = bus;
                }
            }
            Target::Month(_) => {}
        }
        self.draw_calendar();
    }

    /// A click: `secondary` (right button) dismisses.
    pub fn notifications_press(&mut self, position: (f64, f64), secondary: bool) {
        let Some((id, part)) = self
            .notifications
            .stack
            .as_ref()
            .and_then(|s| s.hit(position))
        else {
            return;
        };
        let Some(card) = self.notifications.cards.iter().find(|c| c.note.id == id) else {
            return;
        };
        let action = match part {
            _ if secondary => None,
            Part::Close => None,
            Part::Action(key) => Some(key),
            Part::Card => card
                .note
                .actions
                .iter()
                .any(|(key, _)| key == "default")
                .then(|| "default".to_string()),
        };
        let resident = card.note.resident;
        if let Some(key) = &action {
            self.notification_signal("ActionInvoked", &(id, key.as_str()));
        }
        if action.is_none() || !resident {
            self.close_notification(id, Reason::Dismissed);
        }
    }

    fn draw_notifications(&mut self) {
        let Some(stack) = &mut self.notifications.stack else {
            return;
        };
        if !stack.configured {
            return;
        }
        let now = Instant::now();
        let duration = cnde_ui::anim::scaled(ANIM);
        place_cards(&mut self.notifications.cards, now, duration);
        let scale = stack.scale;
        let lw = WIDTH;
        let (w, h) = (SURFACE_WIDTH as i32 * scale, stack.height as i32 * scale);
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate the notifications");
                return;
            }
        };
        pixels.fill(0);
        let mut hits = Vec::new();
        let mut frost = Vec::new();
        if let Some(mut target) = tiny_skia::PixmapMut::from_bytes(pixels, w as u32, h as u32) {
            let t = &self.theme;
            for card in self.notifications.cards.iter() {
                let presence = card.presence(now, duration);
                let Some(top) = card.top(now, duration).filter(|_| presence > 0.0) else {
                    continue;
                };
                let note = &card.note;
                // Each card is drawn on its own and laid over the stack
                // with its fade, sliding in from (or out to) the right.
                let slide = (1.0 - presence) * SLIDE;
                let size = (
                    (lw * scale as f32).ceil() as u32,
                    (card.height * scale as f32).ceil() as u32,
                );
                let Some(mut scratch) = Pixmap::new(size.0, size.1) else {
                    continue;
                };
                let Some(mut canvas) =
                    Canvas::new(scratch.data_mut(), size.0, size.1, scale as f32)
                else {
                    continue;
                };
                let y = 0.0;
                let mut card_hits = Vec::new();
                let rect = Rect::new(0.0, y, lw, card.height);
                card_hits.push(Hit {
                    id: note.id,
                    part: Part::Card,
                    rect,
                });
                let hovered = stack.hover.as_ref().filter(|(id, _)| *id == note.id);
                canvas.rounded_rect(rect.x, rect.y, rect.w, rect.h, t.qs_radius, t.qs_background);
                if hovered.is_some() {
                    canvas.rounded_rect(
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h,
                        t.qs_radius,
                        t.popup_text.with_alpha(0.04),
                    );
                }
                // The picture: the app's image, else its icon.
                let (ix, iy) = (PADDING, y + PADDING);
                match &note.image {
                    Some(image) => canvas.image(image.as_ref(), ix, iy, ICON, ICON, 8.0),
                    None => {
                        if !canvas.app_icon(&mut self.icons, &note.icon, ix, iy, ICON) {
                            let mut p = Painter {
                                canvas: &mut canvas,
                                fonts: &mut self.fonts,
                                icons: &mut self.icons,
                                theme: t,
                            };
                            p.icon(
                                "preferences-system-notifications-symbolic",
                                ix + ICON / 2.0,
                                iy + ICON / 2.0,
                                24.0,
                                t.popup_text,
                            );
                        }
                    }
                }
                let mut p = Painter {
                    canvas: &mut canvas,
                    fonts: &mut self.fonts,
                    icons: &mut self.icons,
                    theme: t,
                };
                let tx = PADDING + ICON + 12.0;
                let tw = lw - tx - PADDING;
                // App name with the close button beside it, then the
                // summary and body.
                let close = Rect::new(lw - PADDING - 20.0, y + PADDING - 4.0, 24.0, 24.0);
                let close_lit = hovered.is_some_and(|(_, part)| *part == Part::Close);
                if hovered.is_some() {
                    if close_lit {
                        p.canvas.rounded_rect(
                            close.x,
                            close.y,
                            close.w,
                            close.h,
                            close.w / 2.0,
                            t.popup_text.with_alpha(0.1),
                        );
                    }
                    p.icon(
                        "window-close-symbolic",
                        close.x + close.w / 2.0,
                        close.y + close.h / 2.0,
                        14.0,
                        t.qs_dim,
                    );
                }
                card_hits.push(Hit {
                    id: note.id,
                    part: Part::Close,
                    rect: close,
                });
                let mut ty = y + PADDING;
                let small = t.font_size - 2.0;
                p.label_v(
                    &note.app_name,
                    tx,
                    ty + LINE / 2.0 - 2.0,
                    tw - close.w,
                    small,
                    Weight::Normal,
                    t.qs_dim,
                );
                ty += LINE - 2.0;
                p.label_v(
                    &note.summary,
                    tx,
                    ty + LINE / 2.0,
                    tw - 4.0,
                    t.font_size,
                    Weight::Bold,
                    t.popup_text,
                );
                ty += LINE;
                for line in &card.lines {
                    p.label_v(
                        line,
                        tx,
                        ty + LINE / 2.0,
                        tw,
                        t.font_size - 1.0,
                        Weight::Normal,
                        t.popup_text.with_alpha(0.8),
                    );
                    ty += LINE;
                }
                // Action buttons share the row under the text.
                let buttons: Vec<_> = note
                    .actions
                    .iter()
                    .filter(|(k, _)| k != "default")
                    .collect();
                if !buttons.is_empty() {
                    let bh = t.button_height;
                    let by = y + card.height - PADDING - bh;
                    let bw = (lw - PADDING * 2.0 - GAP * (buttons.len() - 1) as f32)
                        / buttons.len() as f32;
                    for (i, (key, label)) in buttons.into_iter().enumerate() {
                        let rect = Rect::new(PADDING + i as f32 * (bw + GAP), by, bw, bh);
                        let lit =
                            hovered.is_some_and(|(_, part)| *part == Part::Action(key.clone()));
                        p.button(rect, if lit { 1.0 } else { 0.0 });
                        let lw = p
                            .measure(label, t.font_size - 1.0, Weight::Normal)
                            .min(bw - 12.0);
                        p.label_v(
                            label,
                            rect.x + (rect.w - lw) / 2.0,
                            rect.y + rect.h / 2.0,
                            lw + 1.0,
                            t.font_size - 1.0,
                            Weight::Normal,
                            t.popup_text,
                        );
                        card_hits.push(Hit {
                            id: note.id,
                            part: Part::Action(key.clone()),
                            rect,
                        });
                    }
                }
                target.draw_pixmap(
                    (slide * scale as f32).round() as i32,
                    (top * scale as f32).round() as i32,
                    scratch.as_ref(),
                    &PixmapPaint {
                        opacity: presence,
                        ..PixmapPaint::default()
                    },
                    Transform::identity(),
                    None,
                );
                frost.push(cnde_ipc::CardFrost {
                    rect: [slide.into(), top.into(), lw.into(), card.height.into()],
                    alpha: presence,
                });
                // A card on its way out can't be clicked.
                if card.leaving.is_none() {
                    hits.extend(card_hits.into_iter().map(|mut hit| {
                        hit.rect.x += slide;
                        hit.rect.y += top;
                        hit
                    }));
                }
            }
        }
        self.notifications.cards.retain(|c| {
            c.leaving
                .is_none_or(|at| cnde_ui::anim::progress(at, duration, now) < 1.0)
        });
        let animating = self
            .notifications
            .cards
            .iter()
            .any(|c| c.animating(now, duration));
        stack.hits = hits;
        stack.animating = animating;
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = stack.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            if animating && !stack.frame_pending {
                surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                stack.frame_pending = true;
            }
            surface.commit();
        }
        let output = stack.output.clone();
        let settled = !animating && stack.height != stack_height(&self.notifications.cards);
        self.send_card_frost(output, frost);
        // Done moving: give back the room the cards needed on the way.
        if settled {
            self.show_notifications();
        }
    }
}

fn card_height(note: &Note, body_lines: usize, button_height: f32) -> f32 {
    // App name, summary, body.
    let text = LINE - 2.0 + LINE + body_lines as f32 * LINE;
    let mut height = PADDING + text.max(ICON) + PADDING;
    if note.actions.iter().any(|(key, _)| key != "default") {
        height += 4.0 + button_height;
    }
    height.ceil()
}

fn stack_height(cards: &[Card]) -> u32 {
    let shown: Vec<_> = cards
        .iter()
        .filter(|c| c.leaving.is_none())
        .take(SHOWN)
        .collect();
    let count = shown.len();
    if count == 0 {
        return 0;
    }
    let total: f32 = shown.iter().map(|c| c.height).sum::<f32>() + GAP * (count - 1) as f32;
    total.ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{StructureBuilder, Value};

    #[test]
    fn name_hand_off() {
        use {Ownership::*, Signal::*, Step::*};
        // Replaced while serving: step back, then take it again when free.
        assert_eq!(hand_off(Owner, Lost), (Out, GiveWay));
        assert_eq!(hand_off(Out, Free), (Out, Request));
        // Another server was there from the start: wait for it to go.
        assert_eq!(hand_off(Out, Lost), (Out, Nothing));
        // Our own name going away isn't a reason to ask again.
        assert_eq!(hand_off(Owner, Free), (Owner, Nothing));
    }

    #[test]
    fn image_data_is_read_and_shrunk() {
        // 2x1 RGB with a padded row, then a big one.
        let small = StructureBuilder::new()
            .add_field(2i32)
            .add_field(1i32)
            .add_field(8i32)
            .add_field(false)
            .add_field(8i32)
            .add_field(3i32)
            .add_field(vec![255u8, 0, 0, 0, 255, 0, 0, 0])
            .build()
            .unwrap();
        let pixmap = image_data(&OwnedValue::try_from(Value::from(small)).unwrap()).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (2, 1));
        let red = pixmap.pixel(0, 0).unwrap();
        assert_eq!((red.red(), red.green(), red.alpha()), (255, 0, 255));
        let big = StructureBuilder::new()
            .add_field(512i32)
            .add_field(256i32)
            .add_field(512i32 * 4)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(vec![200u8; 512 * 256 * 4])
            .build()
            .unwrap();
        let pixmap = image_data(&OwnedValue::try_from(Value::from(big)).unwrap()).unwrap();
        assert_eq!(
            (pixmap.width(), pixmap.height()),
            (IMAGE_MAX, IMAGE_MAX / 2)
        );
        // Too short for what it claims.
        let broken = StructureBuilder::new()
            .add_field(4i32)
            .add_field(4i32)
            .add_field(16i32)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(vec![0u8; 8])
            .build()
            .unwrap();
        assert!(image_data(&OwnedValue::try_from(Value::from(broken)).unwrap()).is_none());
    }
}
