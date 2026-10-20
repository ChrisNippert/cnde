//! The system tray: StatusNotifierItem apps (Discord, Steam, Nextcloud...)
//! show an icon in the top bar, with their dbusmenu as a right-click menu.
//!
//! We serve `org.kde.StatusNotifierWatcher` when nobody else does (else we
//! just host the one that's there) and read each item's properties. All of
//! the D-Bus work is on threads of its own, calls with a short timeout, and
//! the results come to the event loop as [`Event`]s: a misbehaving app can
//! only lose its own icon.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use cnde_ui::{
    Canvas, Color, Icons,
    tiny_skia::{IntSize, Pixmap},
};
use smithay_client_toolkit::{
    reexports::{
        calloop::{
            LoopHandle,
            channel::{self, Sender},
        },
        protocols::xdg::shell::client::xdg_positioner::{Anchor, Gravity},
    },
    seat::pointer::PointerEventKind,
};
use tracing::{debug, info, warn};
use zbus::{
    blocking::fdo::{DBusProxy, PropertiesProxy},
    blocking::{Connection, MessageIterator},
    fdo::{RequestNameFlags, RequestNameReply},
    names::{BusName, InterfaceName},
    zvariant::{OwnedValue, Value},
};

use cnde_config::TrayMode;

use crate::{
    App,
    menu::{Action, Entry, Placement},
};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
const DEFAULT_PATH: &str = "/StatusNotifierItem";
/// A call that takes longer than this is given up on.
const TIMEOUT: Duration = Duration::from_secs(1);
/// Icons bigger than this (a side, in pixels) are ignored.
const PIXMAP_MAX: i32 = 512;
/// Menus nest no deeper than this.
const MENU_DEPTH: usize = 8;

type Props = HashMap<String, OwnedValue>;
type RawLayout = (i32, Props, Vec<OwnedValue>);
type Rect = (i32, i32, i32, i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Passive,
    #[default]
    Active,
    NeedsAttention,
}

/// One icon as the item sends it: ARGB32, network byte order.
#[derive(Debug, Clone, PartialEq)]
pub struct RawPixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct Item {
    /// Where it lives: `<bus name><object path>`.
    pub key: String,
    pub dest: String,
    pub path: String,
    pub title: String,
    pub tooltip: String,
    pub status: Status,
    pub icon_name: String,
    /// `icon_name` found in the item's own `IconThemePath`.
    pub icon_file: Option<String>,
    pub attention_name: String,
    pub attention_file: Option<String>,
    pub pixmaps: Vec<RawPixmap>,
    pub attention_pixmaps: Vec<RawPixmap>,
    /// Its dbusmenu object, if it has one.
    pub menu: Option<String>,
    /// Left click opens the menu (there's nothing to activate).
    pub item_is_menu: bool,
}

/// An item in the bar, with its icon ready to draw.
pub struct TrayItem {
    pub item: Item,
    /// The pixmap drawn last: its pixel size, whether it was the attention
    /// one, and the converted pixels.
    cache: Option<(u32, bool, Option<Pixmap>)>,
}

impl TrayItem {
    fn new(item: Item) -> Self {
        Self { item, cache: None }
    }

    pub fn visible(&self) -> bool {
        self.item.status != Status::Passive
    }

    /// Draw the icon `size` logical pixels square at (`x`, `y`).
    pub fn draw(
        &mut self,
        canvas: &mut Canvas<'_>,
        icons: &mut Icons,
        (x, y, size): (f32, f32, f32),
        fg: Color,
    ) {
        let item = &self.item;
        let attention = item.status == Status::NeedsAttention
            && (!item.attention_name.is_empty() || !item.attention_pixmaps.is_empty());
        let (name, file, pixmaps) = if attention {
            (
                &item.attention_name,
                &item.attention_file,
                &item.attention_pixmaps,
            )
        } else {
            (&item.icon_name, &item.icon_file, &item.pixmaps)
        };
        let named = file.as_deref().unwrap_or(name);
        if !named.is_empty() {
            // Symbolic icons take the bar's text colour; the rest keep theirs.
            let drawn = if name.ends_with("-symbolic") && file.is_none() {
                canvas.icon(icons, named, x, y, size, fg)
            } else {
                canvas.app_icon(icons, named, x, y, size)
                    || canvas.icon(icons, named, x, y, size, fg)
            };
            if drawn {
                return;
            }
        }
        let px = (size * canvas.scale()).round().max(1.0) as u32;
        if self
            .cache
            .as_ref()
            .is_none_or(|(p, a, _)| *p != px || *a != attention)
        {
            let pixmap = pick_pixmap(pixmaps, px as i32).and_then(pixmap_from_argb);
            self.cache = Some((px, attention, pixmap));
        }
        match self.cache.as_ref().and_then(|(_, _, p)| p.as_ref()) {
            Some(pixmap) => canvas.image(pixmap.as_ref(), x, y, size, size, 0.0),
            // Nothing usable: a dot, so there's still something to click.
            None => canvas.circle(
                x + size / 2.0,
                y + size / 2.0,
                size / 4.0,
                fg.with_alpha(fg.a * 0.6),
            ),
        }
    }
}

/// Which tray items go inline in the bar and which behind the chevron
/// (indices into `items`, visible ones only). Collapsed keeps the ones
/// wanting attention in view.
pub fn split(items: &[TrayItem], mode: TrayMode) -> (Vec<usize>, Vec<usize>) {
    let shown = (0..items.len()).filter(|&i| items[i].visible());
    match mode {
        TrayMode::Off => (Vec::new(), Vec::new()),
        TrayMode::Inline => (shown.collect(), Vec::new()),
        TrayMode::Collapsed => shown.partition(|&i| items[i].item.status == Status::NeedsAttention),
    }
}

/// Whether a left click should open the item's menu right away rather than
/// try `Activate` first: it says so (`ItemIsMenu`), or it's a
/// libappindicator item, whose `Activate` does nothing (KDE opens the
/// menu for these too).
pub fn left_click_opens_menu(item: &Item) -> bool {
    item.menu.is_some()
        && (item.item_is_menu || item.path.starts_with("/org/ayatana/NotificationItem/"))
}

/// A dbusmenu entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuNode {
    pub id: i32,
    pub label: String,
    pub separator: bool,
    pub enabled: bool,
    pub visible: bool,
    /// A check mark or radio dot: whether it's on.
    pub toggle: Option<bool>,
    pub children: Vec<MenuNode>,
}

#[derive(Debug)]
pub enum Event {
    Ready(Connection),
    /// Seen for the first time, or changed.
    Item(Item),
    Gone(String),
    /// The menu of `key` was fetched; open it below `rect` on `panel`.
    Menu {
        key: String,
        panel: usize,
        rect: Rect,
        serial: u32,
        root: MenuNode,
    },
}

#[derive(Default)]
pub struct Tray {
    tx: Option<Sender<Event>>,
    conn: Option<Connection>,
    stop: Option<Arc<AtomicBool>>,
    starting: bool,
    pub items: Vec<TrayItem>,
}

/// Set up the channel from D-Bus; the watcher itself starts if enabled.
pub fn setup(handle: &LoopHandle<'static, App>) -> Tray {
    let (tx, rx) = channel::channel::<Event>();
    let inserted = handle.insert_source(rx, |event, _, app: &mut App| {
        if let channel::Event::Msg(event) = event {
            app.tray_event(event);
        }
    });
    Tray {
        tx: inserted.is_ok().then_some(tx),
        ..Tray::default()
    }
}

/// Everything the manager thread reacts to.
enum Msg {
    /// A service string as `RegisterStatusNotifierItem` gets it, and who
    /// sent it.
    Register(String, Option<String>),
    /// A bus name lost its owner.
    NameGone(String),
    /// An item signal from this unique name.
    Changed(String),
    /// A watcher we only host says this one went.
    Unregister(String),
}

fn connect() -> zbus::Result<Connection> {
    zbus::blocking::connection::Builder::session()?
        .method_timeout(TIMEOUT)
        .build()
}

/// Split a registered service into (bus name, object path). A path alone
/// means the sender's unique name; a bare name uses the usual path.
fn split_service(service: &str, sender: Option<&str>) -> Option<(String, String)> {
    if service.starts_with('/') {
        return Some((sender?.to_string(), service.to_string()));
    }
    match service.split_once('/') {
        Some((name, path)) if !name.is_empty() => Some((name.into(), format!("/{path}"))),
        Some(_) => None,
        None if service.is_empty() => None,
        None => Some((service.into(), DEFAULT_PATH.into())),
    }
}

/// The watcher's side of the bus.
struct Watcher {
    msgs: mpsc::Sender<Msg>,
    items: Arc<Mutex<Vec<String>>>,
    host: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string());
        let _ = self.msgs.send(Msg::Register(service, sender));
    }

    fn register_status_notifier_host(&self, _service: String) {
        self.host.store(true, Ordering::Relaxed);
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().map(|i| i.clone()).unwrap_or_default()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        self.host.load(Ordering::Relaxed)
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

fn emit(conn: &Connection, member: &str, service: &str) {
    if let Err(err) = conn.emit_signal(None::<&str>, WATCHER_PATH, WATCHER, member, &service) {
        debug!(%err, member, "tray signal failed");
    }
}

fn signals(interface: &'static str) -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(interface)?
        .build())
}

/// The tray's thread: claim the names, then keep the item list.
fn serve(tx: Sender<Event>, stop: Arc<AtomicBool>) -> zbus::Result<()> {
    let conn = connect()?;
    let (msgs, inbox) = mpsc::channel::<Msg>();
    let items = Arc::new(Mutex::new(Vec::<String>::new()));
    let host = Arc::new(AtomicBool::new(false));
    conn.object_server().at(
        WATCHER_PATH,
        Watcher {
            msgs: msgs.clone(),
            items: items.clone(),
            host: host.clone(),
        },
    )?;
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    conn.request_name(host_name.as_str())?;
    let reply = conn.request_name_with_flags(WATCHER, RequestNameFlags::DoNotQueue.into())?;
    let ours = matches!(reply, RequestNameReply::PrimaryOwner);

    watch(&conn, signals("org.freedesktop.DBus")?, &stop, {
        let msgs = msgs.clone();
        move |msg| {
            let (name, _old, new): (String, String, String) = msg.body().deserialize().ok()?;
            new.is_empty()
                .then(|| msgs.send(Msg::NameGone(name)).ok())?
        }
    })?;
    watch(&conn, signals(ITEM_IFACE)?, &stop, {
        let msgs = msgs.clone();
        move |msg| {
            let sender = msg.header().sender()?.to_string();
            msgs.send(Msg::Changed(sender)).ok()
        }
    })?;
    if ours {
        host.store(true, Ordering::Relaxed);
        emit(&conn, "StatusNotifierHostRegistered", "");
        info!("serving the status notifier watcher");
    } else {
        info!("hosting another status notifier watcher");
        join_watcher(&conn, &host_name, &msgs, &stop)?;
    }
    let _ = tx.send(Event::Ready(conn.clone()));

    // key -> the unique name that owns it.
    let mut owners: HashMap<String, String> = HashMap::new();
    let dbus = DBusProxy::new(&conn)?;
    while !stop.load(Ordering::Relaxed) {
        let Ok(msg) = inbox.recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        match msg {
            Msg::Register(service, sender) => {
                let Some((dest, path)) = split_service(&service, sender.as_deref()) else {
                    continue;
                };
                let key = format!("{dest}{path}");
                if owners.contains_key(&key) {
                    continue;
                }
                let owner = if dest.starts_with(':') {
                    Some(dest.clone())
                } else {
                    BusName::try_from(dest.as_str())
                        .ok()
                        .and_then(|name| dbus.get_name_owner(name).ok())
                        .map(|o| o.to_string())
                };
                let Some(owner) = owner else {
                    debug!(%dest, "tray item has no owner");
                    continue;
                };
                debug!(%key, %owner, "tray item registered");
                owners.insert(key.clone(), owner);
                if ours && let Ok(mut list) = items.lock() {
                    list.push(key.clone());
                    emit(&conn, "StatusNotifierItemRegistered", &key);
                }
                fetch(&conn, &tx, dest, path);
            }
            Msg::NameGone(name) => {
                let prefix = format!("{name}/");
                let gone: Vec<String> = owners
                    .iter()
                    .filter(|(key, owner)| **owner == name || key.starts_with(&prefix))
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in gone {
                    drop_item(&conn, &tx, &mut owners, &items, ours, &key);
                }
            }
            Msg::Unregister(service) => {
                if let Some((dest, path)) = split_service(&service, None) {
                    let key = format!("{dest}{path}");
                    drop_item(&conn, &tx, &mut owners, &items, ours, &key);
                }
            }
            Msg::Changed(sender) => {
                for key in owners.iter().filter(|(_, o)| **o == sender).map(|(k, _)| k) {
                    if let Some((dest, path)) = key.find('/').map(|i| key.split_at(i)) {
                        fetch(&conn, &tx, dest.into(), path.into());
                    }
                }
            }
        }
    }
    let _ = conn.release_name(WATCHER);
    let _ = conn.release_name(host_name.as_str());
    Ok(())
}

fn drop_item(
    conn: &Connection,
    tx: &Sender<Event>,
    owners: &mut HashMap<String, String>,
    items: &Mutex<Vec<String>>,
    ours: bool,
    key: &str,
) {
    if owners.remove(key).is_none() {
        return;
    }
    debug!(%key, "tray item gone");
    if ours {
        if let Ok(mut list) = items.lock() {
            list.retain(|k| k != key);
        }
        emit(conn, "StatusNotifierItemUnregistered", key);
    }
    let _ = tx.send(Event::Gone(key.into()));
}

/// Run `handle` on every message matching `rule`, on a thread, until `stop`.
fn watch(
    conn: &Connection,
    rule: zbus::MatchRule<'static>,
    stop: &Arc<AtomicBool>,
    handle: impl Fn(&zbus::Message) -> Option<()> + Send + 'static,
) -> zbus::Result<()> {
    let iter = MessageIterator::for_match_rule(rule, conn, Some(256))?;
    let stop = stop.clone();
    let _ = std::thread::Builder::new()
        .name("tray-watch".into())
        .spawn(move || {
            for msg in iter.flatten() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                handle(&msg);
            }
        });
    Ok(())
}

/// Another program serves the watcher: tell it we're a host, take the
/// items it already has and follow its signals.
fn join_watcher(
    conn: &Connection,
    host_name: &str,
    msgs: &mpsc::Sender<Msg>,
    stop: &Arc<AtomicBool>,
) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(WATCHER)?
        .interface(WATCHER)?
        .build();
    watch(conn, rule, stop, {
        let msgs = msgs.clone();
        move |msg| {
            let member = msg.header().member()?.to_string();
            let service: String = msg.body().deserialize().ok()?;
            match member.as_str() {
                "StatusNotifierItemRegistered" => msgs.send(Msg::Register(service, None)).ok(),
                "StatusNotifierItemUnregistered" => msgs.send(Msg::Unregister(service)).ok(),
                _ => None,
            }
        }
    })?;
    if let Err(err) = conn.call_method(
        Some(WATCHER),
        WATCHER_PATH,
        Some(WATCHER),
        "RegisterStatusNotifierHost",
        &host_name,
    ) {
        warn!(%err, "can't register as a tray host");
    }
    let props = PropertiesProxy::builder(conn)
        .destination(WATCHER)?
        .path(WATCHER_PATH)?
        .build()?;
    let existing = props
        .get(
            InterfaceName::from_static_str_unchecked(WATCHER),
            "RegisteredStatusNotifierItems",
        )
        .ok()
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default();
    for service in existing {
        let _ = msgs.send(Msg::Register(service, None));
    }
    Ok(())
}

/// Read an item's properties on a thread of its own.
fn fetch(conn: &Connection, tx: &Sender<Event>, dest: String, path: String) {
    let (conn, tx) = (conn.clone(), tx.clone());
    let _ = std::thread::Builder::new()
        .name("tray-item".into())
        .spawn(move || match read_item(&conn, &dest, &path) {
            Ok(item) => {
                debug!(
                    key = %item.key, title = %item.title, icon = %item.icon_name,
                    status = ?item.status, pixmaps = item.pixmaps.len(), menu = ?item.menu,
                    "tray item"
                );
                let _ = tx.send(Event::Item(item));
            }
            Err(err) => debug!(%err, %dest, "can't read tray item"),
        });
}

fn read_item(conn: &Connection, dest: &str, path: &str) -> zbus::Result<Item> {
    let props = PropertiesProxy::builder(conn)
        .destination(dest.to_string())?
        .path(path.to_string())?
        .build()?
        .get_all(InterfaceName::from_static_str_unchecked(ITEM_IFACE))?;
    Ok(item_from_props(dest, path, &props))
}

fn str_prop(props: &Props, name: &str) -> String {
    props
        .get(name)
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .unwrap_or_default()
        .to_string()
}

fn pixmaps_prop(props: &Props, name: &str) -> Vec<RawPixmap> {
    props
        .get(name)
        .and_then(|v| Vec::<(i32, i32, Vec<u8>)>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|(width, height, argb)| RawPixmap {
            width,
            height,
            argb,
        })
        .collect()
}

fn item_from_props(dest: &str, path: &str, props: &Props) -> Item {
    let theme_path = str_prop(props, "IconThemePath");
    let icon_name = str_prop(props, "IconName");
    let attention_name = str_prop(props, "AttentionIconName");
    let status = match str_prop(props, "Status").as_str() {
        "Passive" => Status::Passive,
        "NeedsAttention" => Status::NeedsAttention,
        _ => Status::Active,
    };
    let tooltip = props
        .get("ToolTip")
        .and_then(|v| {
            <(String, Vec<(i32, i32, Vec<u8>)>, String, String)>::try_from(v.try_clone().ok()?).ok()
        })
        .map(|(_, _, title, _)| title)
        .unwrap_or_default();
    let menu = props
        .get("Menu")
        .and_then(|v| v.downcast_ref::<zbus::zvariant::ObjectPath>().ok())
        .map(|p| p.to_string())
        .filter(|p| p != "/" && p != "/NO_DBUSMENU");
    Item {
        key: format!("{dest}{path}"),
        dest: dest.into(),
        path: path.into(),
        title: str_prop(props, "Title"),
        tooltip,
        status,
        icon_file: find_in_theme_path(&theme_path, &icon_name),
        attention_file: find_in_theme_path(&theme_path, &attention_name),
        icon_name,
        attention_name,
        pixmaps: pixmaps_prop(props, "IconPixmap"),
        attention_pixmaps: pixmaps_prop(props, "AttentionIconPixmap"),
        menu,
        item_is_menu: props
            .get("ItemIsMenu")
            .and_then(|v| v.downcast_ref::<bool>().ok())
            .unwrap_or(false),
    }
}

/// `name` as a file under the item's own icon directory (a few levels
/// down, as themes lay them out), SVG before PNG.
fn find_in_theme_path(dir: &str, name: &str) -> Option<String> {
    if dir.is_empty() || name.is_empty() || name.starts_with('/') {
        return None;
    }
    fn search(dir: &Path, files: &[String], depth: u32) -> Option<PathBuf> {
        for file in files {
            let path = dir.join(file);
            if path.is_file() {
                return Some(path);
            }
        }
        if depth == 0 {
            return None;
        }
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .find_map(|e| search(&e.path(), files, depth - 1))
    }
    let files = [format!("{name}.svg"), format!("{name}.png")];
    search(Path::new(dir), &files, 3).map(|p| p.to_string_lossy().into_owned())
}

/// The pixmap whose size is the least that's at least `want` pixels (the
/// biggest if none is), skipping empty, oversized or short ones.
pub fn pick_pixmap(pixmaps: &[RawPixmap], want: i32) -> Option<&RawPixmap> {
    let usable = |p: &&RawPixmap| {
        p.width > 0
            && p.height > 0
            && p.width <= PIXMAP_MAX
            && p.height <= PIXMAP_MAX
            && p.argb.len() == (p.width * p.height * 4) as usize
    };
    let side = |p: &RawPixmap| p.width.max(p.height);
    pixmaps
        .iter()
        .filter(usable)
        .filter(|p| side(p) >= want)
        .min_by_key(|p| side(p))
        .or_else(|| pixmaps.iter().filter(usable).max_by_key(|p| side(p)))
}

/// ARGB32 in network byte order (A, R, G, B per pixel) to tiny-skia's
/// premultiplied RGBA.
pub fn pixmap_from_argb(raw: &RawPixmap) -> Option<Pixmap> {
    let size = IntSize::from_wh(raw.width.try_into().ok()?, raw.height.try_into().ok()?)?;
    let mut data = Vec::with_capacity(raw.argb.len());
    for px in raw.argb.as_chunks::<4>().0 {
        let a = px[0] as u32;
        let premul = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        data.extend_from_slice(&[premul(px[1]), premul(px[2]), premul(px[3]), px[0]]);
    }
    Pixmap::from_vec(data, size)
}

/// One dbusmenu node, with its children (to `depth` levels).
fn parse_node(raw: RawLayout, depth: usize) -> MenuNode {
    let (id, props, children) = raw;
    let string = |name: &str| {
        props
            .get(name)
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .unwrap_or_default()
            .to_string()
    };
    let flag = |name: &str| {
        props
            .get(name)
            .and_then(|v| v.downcast_ref::<bool>().ok())
            .unwrap_or(true)
    };
    let toggle = match string("toggle-type").as_str() {
        "checkmark" | "radio" => Some(
            props
                .get("toggle-state")
                .and_then(|v| v.downcast_ref::<i32>().ok())
                .unwrap_or(0)
                == 1,
        ),
        _ => None,
    };
    let children = if depth == 0 {
        Vec::new()
    } else {
        children
            .into_iter()
            .filter_map(|child| {
                let value: Value = child.into();
                // Children come wrapped in a variant.
                let value = match value {
                    Value::Value(inner) => *inner,
                    other => other,
                };
                RawLayout::try_from(value).ok()
            })
            .map(|raw| parse_node(raw, depth - 1))
            .collect()
    };
    MenuNode {
        id,
        label: mnemonic(&string("label")),
        separator: string("type") == "separator",
        enabled: flag("enabled"),
        visible: flag("visible"),
        toggle,
        children,
    }
}

/// "_File" is "File" with an accelerator; "__" is a literal underscore.
fn mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '_' {
            out.push(c);
        } else if chars.peek() == Some(&'_') {
            chars.next();
            out.push('_');
        }
    }
    out
}

/// A dbusmenu as the shell's menu entries. Submenus are flattened under a
/// dim heading; disabled items are dim and can't be clicked.
pub fn menu_entries(nodes: &[MenuNode], key: &str) -> Vec<Entry> {
    fn add(out: &mut Vec<Entry>, nodes: &[MenuNode], key: &str) {
        for node in nodes.iter().filter(|n| n.visible) {
            if node.separator {
                if !matches!(out.last(), None | Some(Entry::Separator)) {
                    out.push(Entry::Separator);
                }
            } else if !node.children.is_empty() {
                out.push(Entry::Section(node.label.clone()));
                add(out, &node.children, key);
            } else if !node.enabled {
                out.push(Entry::Section(node.label.clone()));
            } else {
                out.push(Entry::Item {
                    label: node.label.clone(),
                    action: Action::Tray(key.into(), node.id),
                    check: node.toggle,
                });
            }
        }
    }
    let mut out = Vec::new();
    add(&mut out, nodes, key);
    while matches!(out.last(), Some(Entry::Separator)) {
        out.pop();
    }
    out
}

/// Fetch the menu: `AboutToShow` (apps fill it in then), then the layout.
fn read_menu(conn: &Connection, item: &Item) -> zbus::Result<MenuNode> {
    let path = item.menu.as_deref().unwrap_or("/");
    let dest = item.dest.as_str();
    let _ = conn.call_method(Some(dest), path, Some(MENU_IFACE), "AboutToShow", &(0i32,));
    let reply = conn.call_method(
        Some(dest),
        path,
        Some(MENU_IFACE),
        "GetLayout",
        &(0i32, -1i32, Vec::<&str>::new()),
    )?;
    let (_revision, root): (u32, RawLayout) = reply.body().deserialize()?;
    Ok(parse_node(root, MENU_DEPTH))
}

fn fetch_menu(
    conn: &Connection,
    tx: &Sender<Event>,
    item: Item,
    panel: usize,
    rect: Rect,
    serial: u32,
) {
    if item.menu.is_none() {
        return;
    }
    match read_menu(conn, &item) {
        Ok(root) => {
            let _ = tx.send(Event::Menu {
                key: item.key,
                panel,
                rect,
                serial,
                root,
            });
        }
        Err(err) => debug!(%err, "can't read tray menu"),
    }
}

/// A fire-and-forget method call on the item.
fn call_item<B>(conn: Connection, item: Item, method: &'static str, body: B)
where
    B: zbus::export::serde::Serialize + zbus::zvariant::DynamicType + Send + 'static,
{
    let _ = std::thread::Builder::new()
        .name("tray-call".into())
        .spawn(move || {
            let reply = conn.call_method(
                Some(item.dest.as_str()),
                item.path.as_str(),
                Some(ITEM_IFACE),
                method,
                &body,
            );
            if let Err(err) = reply {
                debug!(%err, method, "tray call failed");
            }
        });
}

impl App {
    /// Start the watcher if the tray is on, stop it if it's off.
    pub fn sync_tray(&mut self) {
        let t = &mut self.tray;
        if self.config.panel.tray != TrayMode::Off {
            if t.conn.is_none() && !t.starting {
                let Some(tx) = t.tx.clone() else {
                    return;
                };
                t.starting = true;
                let stop = Arc::new(AtomicBool::new(false));
                t.stop = Some(stop.clone());
                let _ = std::thread::Builder::new()
                    .name("tray".into())
                    .spawn(move || {
                        if let Err(err) = serve(tx, stop) {
                            warn!(%err, "can't serve the tray");
                        }
                    });
            }
        } else if t.conn.is_some() || t.starting {
            info!("tray off");
            if let Some(stop) = t.stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            t.conn = None;
            t.starting = false;
            t.items.clear();
            self.redraw_panels();
        }
    }

    pub fn tray_event(&mut self, event: Event) {
        match event {
            Event::Ready(conn) => {
                self.tray.starting = false;
                if self.config.panel.tray != TrayMode::Off {
                    self.tray.conn = Some(conn);
                }
            }
            Event::Item(item) => {
                if self.config.panel.tray == TrayMode::Off {
                    return;
                }
                match self.tray.items.iter_mut().find(|t| t.item.key == item.key) {
                    Some(existing) => *existing = TrayItem::new(item),
                    None => self.tray.items.push(TrayItem::new(item)),
                }
                self.redraw_panels();
            }
            Event::Gone(key) => {
                self.tray.items.retain(|t| t.item.key != key);
                self.redraw_panels();
            }
            Event::Menu {
                key,
                panel,
                rect,
                serial,
                root,
            } => {
                let entries = menu_entries(&root.children, &key);
                debug!(%key, entries = entries.len(), "tray menu");
                if entries.is_empty() {
                    return;
                }
                let Some(p) = self.panels.get(panel) else {
                    return;
                };
                let (parent, scale) = (p.layer.clone(), p.scale());
                self.open_menu(
                    &parent,
                    serial,
                    Placement {
                        rect,
                        anchor: Anchor::Bottom,
                        gravity: Gravity::Bottom,
                    },
                    entries,
                    scale,
                );
                if self.menu.is_some() {
                    self.tray_menu_key = Some(key);
                }
            }
        }
    }

    /// A press or wheel notch on tray icon `i` of panel `panel`; true if
    /// the tray took it.
    pub fn tray_pointer(&mut self, panel: usize, i: usize, kind: &PointerEventKind) -> bool {
        match self.panels.get(panel).and_then(|p| p.tray_rect(i)) {
            Some(rect) => self.tray_press(panel, i, rect, kind),
            None => false,
        }
    }

    /// As `tray_pointer`, with the menu hanging from `rect` (the icon, or
    /// the chevron when the click came from its popover).
    pub fn tray_press(
        &mut self,
        panel: usize,
        i: usize,
        rect: Rect,
        kind: &PointerEventKind,
    ) -> bool {
        const BTN_LEFT: u32 = 0x110;
        const BTN_RIGHT: u32 = 0x111;
        const BTN_MIDDLE: u32 = 0x112;
        let (Some(conn), Some(tx), Some(entry)) = (
            self.tray.conn.clone(),
            self.tray.tx.clone(),
            self.tray.items.get(i),
        ) else {
            return false;
        };
        let item = entry.item.clone();
        let (x, y) = (rect.0, rect.1 + rect.3);
        match *kind {
            PointerEventKind::Press { button, serial, .. } if button == BTN_LEFT => {
                if self.tray_menu_toggle(&item.key) {
                    return true;
                }
                let name = std::thread::Builder::new().name("tray-call".into());
                let _ = name.spawn(move || {
                    if left_click_opens_menu(&item) {
                        return fetch_menu(&conn, &tx, item, panel, rect, serial);
                    }
                    let reply = conn.call_method(
                        Some(item.dest.as_str()),
                        item.path.as_str(),
                        Some(ITEM_IFACE),
                        "Activate",
                        &(x, y),
                    );
                    // Not activatable, whatever the reason: show the menu.
                    if let Err(err) = reply {
                        debug!(%err, "tray activate failed");
                        fetch_menu(&conn, &tx, item, panel, rect, serial);
                    }
                });
                true
            }
            PointerEventKind::Press { button, .. } if button == BTN_MIDDLE => {
                call_item(conn, item, "SecondaryActivate", (x, y));
                true
            }
            PointerEventKind::Press { button, serial, .. } if button == BTN_RIGHT => {
                if self.tray_menu_toggle(&item.key) {
                    return true;
                }
                let _ = std::thread::Builder::new()
                    .name("tray-menu".into())
                    .spawn(move || fetch_menu(&conn, &tx, item, panel, rect, serial));
                true
            }
            PointerEventKind::Axis { vertical, .. } => {
                let notches = if vertical.value120 != 0 {
                    vertical.value120 as f64 / 120.0
                } else {
                    vertical.absolute / 60.0
                };
                let delta = notches.round() as i32;
                if delta != 0 {
                    call_item(conn, item, "Scroll", (delta, "vertical"));
                }
                true
            }
            _ => false,
        }
    }

    /// A press on the bar closes the tray's menu and popover (the compositor
    /// doesn't dismiss popups for clicks on the shell's own surfaces). They're
    /// marked as just dismissed, so a press on the icon or chevron that opened
    /// them closes rather than reopens.
    pub fn tray_close_on_bar_press(&mut self) {
        let now = std::time::Instant::now();
        if self.tray_popover.take().is_some() {
            self.tray_popover_dismissed = Some(now);
            self.redraw_panels();
        }
        if let Some(key) = self.tray_menu_key.take() {
            debug!(key, "tray menu closed by a bar press");
            if self.menu.is_some() {
                self.close_menu();
            }
            self.tray_menu_dismissed = Some((key, now));
        }
    }

    /// A click on an icon whose menu is open (or was just closed by this
    /// very click, which the compositor delivers as a dismissal first)
    /// closes it rather than opening it again. True if it did.
    fn tray_menu_toggle(&mut self, key: &str) -> bool {
        let just_dismissed = self
            .tray_menu_dismissed
            .take()
            .is_some_and(|(k, at)| k == key && at.elapsed() < Duration::from_millis(300));
        let open = self.tray_menu_key.as_deref() == Some(key) && self.menu.is_some();
        if open {
            debug!(key, "tray menu toggled closed");
            self.close_menu();
        }
        just_dismissed || open
    }

    /// Debug aid (CNDE_SHELL_OPEN=tray_menu): right-click the first icon.
    pub fn tray_debug_menu(&mut self, serial: u32) -> bool {
        let Some(i) = self.panels.first().and_then(|p| p.tray_at_first()) else {
            return false;
        };
        let press = PointerEventKind::Press {
            time: 0,
            button: 0x111,
            serial,
        };
        self.tray_pointer(0, i, &press)
    }

    /// A menu entry was chosen.
    pub fn tray_menu_click(&mut self, key: &str, id: i32) {
        let (Some(conn), Some(entry)) = (
            self.tray.conn.clone(),
            self.tray.items.iter().find(|t| t.item.key == key),
        ) else {
            return;
        };
        let item = entry.item.clone();
        let _ = std::thread::Builder::new()
            .name("tray-call".into())
            .spawn(move || {
                let reply = conn.call_method(
                    Some(item.dest.as_str()),
                    item.menu.as_deref().unwrap_or("/"),
                    Some(MENU_IFACE),
                    "Event",
                    &(id, "clicked", Value::from(0i32), 0u32),
                );
                if let Err(err) = reply {
                    debug!(%err, "tray menu event failed");
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(w: i32, h: i32, argb: [u8; 4]) -> RawPixmap {
        RawPixmap {
            width: w,
            height: h,
            argb: argb.repeat((w * h) as usize),
        }
    }

    #[test]
    fn argb_becomes_premultiplied_rgba() {
        // Opaque red, then half-transparent green.
        let opaque = pixmap_from_argb(&raw(1, 1, [255, 255, 0, 0])).unwrap();
        assert_eq!(opaque.data(), &[255, 0, 0, 255]);
        let half = pixmap_from_argb(&raw(1, 1, [128, 0, 255, 0])).unwrap();
        assert_eq!(half.data(), &[0, 128, 0, 128]);
        let clear = pixmap_from_argb(&raw(1, 1, [0, 255, 255, 255])).unwrap();
        assert_eq!(clear.data(), &[0, 0, 0, 0]);
    }

    #[test]
    fn bad_pixmaps_are_refused() {
        let short = RawPixmap {
            width: 2,
            height: 2,
            argb: vec![0; 8],
        };
        assert!(pick_pixmap(&[short], 16).is_none());
        assert!(pick_pixmap(&[raw(0, 0, [0; 4])], 16).is_none());
        assert!(pick_pixmap(&[raw(600, 1, [0; 4])], 16).is_none());
    }

    #[test]
    fn closest_size_at_least_wanted() {
        let sizes = [
            raw(16, 16, [255; 4]),
            raw(32, 32, [255; 4]),
            raw(64, 64, [255; 4]),
        ];
        assert_eq!(pick_pixmap(&sizes, 24).unwrap().width, 32);
        assert_eq!(pick_pixmap(&sizes, 32).unwrap().width, 32);
        assert_eq!(pick_pixmap(&sizes, 8).unwrap().width, 16);
        // Bigger than any: the biggest.
        assert_eq!(pick_pixmap(&sizes, 128).unwrap().width, 64);
    }

    #[test]
    fn left_click_menu_decision() {
        let item = |menu: Option<&str>, is_menu: bool, path: &str| Item {
            menu: menu.map(Into::into),
            item_is_menu: is_menu,
            path: path.into(),
            ..Item::default()
        };
        let plain = "/StatusNotifierItem";
        assert!(left_click_opens_menu(&item(Some("/Menu"), true, plain)));
        assert!(!left_click_opens_menu(&item(Some("/Menu"), false, plain)));
        // No menu to open, whatever it claims.
        assert!(!left_click_opens_menu(&item(None, true, plain)));
        let ayatana = "/org/ayatana/NotificationItem/steam";
        assert!(left_click_opens_menu(&item(Some("/Menu"), false, ayatana)));
        assert!(!left_click_opens_menu(&item(None, false, ayatana)));
    }

    #[test]
    fn collapsed_tray_keeps_attention_in_view() {
        let mk = |status| {
            TrayItem::new(Item {
                status,
                ..Item::default()
            })
        };
        let items = [
            mk(Status::Active),
            mk(Status::Passive),
            mk(Status::NeedsAttention),
        ];
        assert_eq!(split(&items, TrayMode::Collapsed), (vec![2], vec![0]));
        assert_eq!(split(&items, TrayMode::Inline), (vec![0, 2], vec![]));
        assert_eq!(split(&items, TrayMode::Off), (vec![], vec![]));
    }

    #[test]
    fn services_split_into_name_and_path() {
        assert_eq!(
            split_service("org.kde.item", None),
            Some(("org.kde.item".into(), "/StatusNotifierItem".into()))
        );
        assert_eq!(
            split_service("/org/ayatana/1", Some(":1.42")),
            Some((":1.42".into(), "/org/ayatana/1".into()))
        );
        assert_eq!(split_service("/org/ayatana/1", None), None);
        assert_eq!(
            split_service(":1.7/MenuBar", None),
            Some((":1.7".into(), "/MenuBar".into()))
        );
        assert_eq!(split_service("", None), None);
    }

    fn node(
        id: i32,
        props: &[(&str, Value<'static>)],
        children: Vec<Value<'static>>,
    ) -> Value<'static> {
        let dict: HashMap<String, Value<'static>> = props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        Value::from((
            id,
            dict,
            children
                .into_iter()
                .map(|c| Value::Value(Box::new(c)))
                .collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn layout_is_parsed() {
        let sub = node(
            3,
            &[("label", "S_ub".into())],
            vec![node(4, &[("label", "Inner".into())], vec![])],
        );
        let root = node(
            0,
            &[],
            vec![
                node(1, &[("label", "_Open".into())], vec![]),
                node(2, &[("type", "separator".into())], vec![]),
                sub,
                node(
                    5,
                    &[
                        ("label", "Mute".into()),
                        ("toggle-type", "checkmark".into()),
                        ("toggle-state", 1i32.into()),
                    ],
                    vec![],
                ),
                node(
                    6,
                    &[("label", "Hidden".into()), ("visible", false.into())],
                    vec![],
                ),
                node(
                    7,
                    &[("label", "Off".into()), ("enabled", false.into())],
                    vec![],
                ),
            ],
        );
        let raw = RawLayout::try_from(root).unwrap();
        let tree = parse_node(raw, MENU_DEPTH);
        assert_eq!(tree.children.len(), 6);
        assert_eq!(tree.children[0].label, "Open");
        assert!(tree.children[1].separator);
        assert_eq!(tree.children[2].children[0].label, "Inner");
        assert_eq!(tree.children[3].toggle, Some(true));
        assert!(!tree.children[4].visible);
        assert!(!tree.children[5].enabled);

        let entries = menu_entries(&tree.children, "k");
        assert_eq!(
            entries,
            vec![
                Entry::item("Open", Action::Tray("k".into(), 1)),
                Entry::Separator,
                Entry::Section("Sub".into()),
                Entry::item("Inner", Action::Tray("k".into(), 4)),
                Entry::check("Mute", true, Action::Tray("k".into(), 5)),
                Entry::Section("Off".into()),
            ]
        );
    }

    #[test]
    fn mnemonics_are_stripped() {
        assert_eq!(mnemonic("_Quit"), "Quit");
        assert_eq!(mnemonic("Snake__case"), "Snake_case");
    }
}
