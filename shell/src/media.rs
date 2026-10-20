//! Media players (MPRIS) for the clock's popover: which players are on the
//! session bus and what they're playing, plus play/pause/next/previous.
//!
//! All D-Bus runs on its own threads with one-second call timeouts and
//! hands snapshots to the main loop. Signals (a player's PropertiesChanged,
//! names appearing and going) trigger a re-read of every player; while the
//! popover is open that also happens once a second, for the position.

use std::{collections::HashMap, sync::mpsc, time::Duration};

use smithay_client_toolkit::reexports::calloop::{
    LoopHandle,
    channel::{self, Sender},
};
use tracing::{debug, warn};
use zbus::{
    blocking::{Connection, fdo::DBusProxy},
    zvariant::{OwnedValue, Value},
};

use crate::App;

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Player {
    /// The well-known bus name.
    pub bus: String,
    /// "Spotify", "Firefox"…
    pub identity: String,
    pub desktop_entry: String,
    pub status: Status,
    pub title: String,
    pub artist: String,
    pub art_url: String,
    /// Microseconds.
    pub length: Option<u64>,
    pub position: u64,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_play: bool,
    pub can_pause: bool,
    /// Bumped whenever it starts or stops, or the song changes; the
    /// highest wins a tie.
    pub changed: u64,
}

impl Player {
    /// 0-1, if the song has a length.
    pub fn progress(&self) -> Option<f32> {
        let length = self.length.filter(|&l| l > 0)?;
        Some((self.position as f32 / length as f32).clamp(0.0, 1.0))
    }

    /// A local image to show: `file://` art only.
    pub fn art_path(&self) -> Option<String> {
        let path = self.art_url.strip_prefix("file://")?;
        Some(percent_decode(path))
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// (title, artist, art url, length in µs) from a Metadata dictionary.
pub fn parse_metadata(meta: &HashMap<String, OwnedValue>) -> (String, String, String, Option<u64>) {
    let text = |key: &str| -> String {
        meta.get(key)
            .and_then(|v| v.downcast_ref::<&str>().ok().map(String::from))
            .unwrap_or_default()
    };
    // xesam:artist is a list of names; some players send a plain string.
    let artist = meta
        .get("xesam:artist")
        .and_then(|v| match &**v {
            Value::Array(list) => Some(
                list.iter()
                    .filter_map(|n| n.downcast_ref::<&str>().ok().map(String::from))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    // mpris:length is an int64 by the spec, though uint64 turns up.
    let length = meta.get("mpris:length").and_then(|v| match &**v {
        Value::I64(n) => u64::try_from(*n).ok(),
        Value::U64(n) => Some(*n),
        Value::I32(n) => u64::try_from(*n).ok(),
        Value::U32(n) => Some(u64::from(*n)),
        _ => None,
    });
    (text("xesam:title"), artist, text("mpris:artUrl"), length)
}

/// The player to show: one playing beats one paused; among equals, the
/// one that changed last. Stopped ones with nothing loaded don't count.
pub fn choose(players: &[Player]) -> Option<usize> {
    players
        .iter()
        .enumerate()
        .filter(|(_, p)| p.status != Status::Stopped)
        .max_by_key(|(_, p)| (p.status == Status::Playing, p.changed))
        .map(|(i, _)| i)
}

#[derive(Debug, Clone, Copy)]
pub enum Op {
    PlayPause,
    Next,
    Previous,
}

enum Cmd {
    /// Read everything again.
    Refresh,
    /// The popover is open (position ticks) or not.
    Visible(bool),
    Control(String, Op),
}

pub enum Event {
    Players(Vec<Player>),
}

#[derive(Default)]
pub struct Media {
    tx: Option<mpsc::Sender<Cmd>>,
    ui: Option<Sender<Event>>,
    pub players: Vec<Player>,
    /// The player the user switched to (by bus name), while it's there.
    pub selected: Option<String>,
}

impl Media {
    /// The one on show.
    pub fn current(&self) -> Option<&Player> {
        if let Some(bus) = &self.selected
            && let Some(p) = self
                .players
                .iter()
                .find(|p| &p.bus == bus && p.status != Status::Stopped)
        {
            return Some(p);
        }
        choose(&self.players).map(|i| &self.players[i])
    }

    /// Players that could be shown (not stopped).
    pub fn live(&self) -> Vec<&Player> {
        self.players
            .iter()
            .filter(|p| p.status != Status::Stopped)
            .collect()
    }

    pub fn visible(&self, on: bool) {
        self.send(Cmd::Visible(on));
    }

    pub fn control(&self, bus: &str, op: Op) {
        self.send(Cmd::Control(bus.into(), op));
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

/// Set up the channel from the D-Bus threads; they start with [`App::sync_media`].
pub fn setup(handle: &LoopHandle<'static, App>) -> Media {
    let (tx, rx) = channel::channel::<Event>();
    let inserted = handle.insert_source(rx, |event, _, app: &mut App| {
        if let channel::Event::Msg(event) = event {
            app.media_event(event);
        }
    });
    Media {
        ui: inserted.is_ok().then_some(tx),
        ..Media::default()
    }
}

impl App {
    /// Start watching players if media controls are on.
    pub fn sync_media(&mut self) {
        let m = &mut self.media;
        if self.config.panel.media_controls {
            if m.tx.is_none()
                && let Some(ui) = m.ui.clone()
            {
                let (tx, rx) = mpsc::channel();
                m.tx = Some(tx.clone());
                let _ = tx.send(Cmd::Refresh);
                let _ = std::thread::Builder::new()
                    .name("mpris".into())
                    .spawn(move || {
                        if let Err(err) = run(rx, tx, ui) {
                            warn!(%err, "can't follow media players");
                        }
                    });
            }
        } else if m.tx.take().is_some() {
            // Dropping the sender ends the worker.
            m.players.clear();
        }
    }

    pub fn media_event(&mut self, event: Event) {
        let Event::Players(players) = event;
        if !self.config.panel.media_controls {
            return;
        }
        self.media.players = players;
        if self.calendar.is_some() {
            self.draw_calendar();
        } else {
            // Position ticks stop once the popover has gone.
            self.media.visible(false);
        }
    }
}

fn connect() -> zbus::Result<Connection> {
    zbus::blocking::connection::Builder::session()?
        .method_timeout(TIMEOUT)
        .build()
}

/// Watch the bus for anything a player says, and tell the worker.
fn watch(conn: &Connection, rule: zbus::MatchRule<'static>, poke: mpsc::Sender<Cmd>) {
    let Ok(signals) = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(64)) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("mpris-watch".into())
        .spawn(move || {
            for _ in signals.flatten() {
                if poke.send(Cmd::Refresh).is_err() {
                    break;
                }
            }
        });
}

fn run(cmds: mpsc::Receiver<Cmd>, poke: mpsc::Sender<Cmd>, ui: Sender<Event>) -> zbus::Result<()> {
    let conn = connect()?;
    let changes = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .path(PATH)?
        .build();
    watch(&conn, changes, poke.clone());
    let names = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns(ROOT)?
        .build();
    watch(&conn, names, poke);

    let mut state = State::default();
    let mut visible = false;
    let mut last: Option<Vec<Player>> = None;
    loop {
        let mut refresh = false;
        let mut tick = false;
        let first = if visible {
            match cmds.recv_timeout(Duration::from_secs(1)) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // The position ticks.
                    refresh = true;
                    tick = true;
                    None
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match cmds.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return Ok(()),
            }
        };
        // Everything queued is handled before one re-read.
        for cmd in first.into_iter().chain(cmds.try_iter()) {
            match cmd {
                Cmd::Refresh => refresh = true,
                Cmd::Visible(on) => {
                    refresh |= on && !visible;
                    visible = on;
                }
                Cmd::Control(bus, op) => {
                    let method = match op {
                        Op::PlayPause => "PlayPause",
                        Op::Next => "Next",
                        Op::Previous => "Previous",
                    };
                    if let Err(err) =
                        conn.call_method(Some(bus.as_str()), PATH, Some(PLAYER), method, &())
                    {
                        debug!(%err, %bus, "player didn't take {method}");
                    }
                    refresh = true;
                }
            }
        }
        if refresh {
            let players = state.read(&conn);
            // A tick is always reported: the app answers it with "stop" if the
            // popover has gone.
            if tick || last.as_ref() != Some(&players) {
                last = Some(players.clone());
                if ui.send(Event::Players(players)).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Default)]
struct State {
    counter: u64,
    previous: HashMap<String, Player>,
    /// Identity and desktop entry, asked once per player.
    names: HashMap<String, (String, String)>,
}

impl State {
    fn read(&mut self, conn: &Connection) -> Vec<Player> {
        let names = DBusProxy::new(conn)
            .ok()
            .and_then(|d| d.list_names().ok())
            .unwrap_or_default();
        let buses: Vec<String> = names
            .into_iter()
            .map(|n| n.to_string())
            .filter(|n| n.starts_with(PREFIX) && !n.ends_with(".playerctld"))
            .collect();
        let mut players = Vec::new();
        for bus in buses {
            let Some(mut player) = self.read_one(conn, &bus) else {
                continue;
            };
            // Bump the order stamp when it starts, stops or changes song.
            match self.previous.get(&bus) {
                Some(old)
                    if old.status == player.status
                        && old.title == player.title
                        && old.artist == player.artist =>
                {
                    player.changed = old.changed;
                }
                _ => {
                    self.counter += 1;
                    player.changed = self.counter;
                }
            }
            players.push(player);
        }
        self.previous = players.iter().map(|p| (p.bus.clone(), p.clone())).collect();
        self.names.retain(|bus, _| self.previous.contains_key(bus));
        players
    }

    fn read_one(&mut self, conn: &Connection, bus: &str) -> Option<Player> {
        let all = get_all(conn, bus, PLAYER)?;
        let (identity, desktop_entry) = self
            .names
            .entry(bus.to_string())
            .or_insert_with(|| {
                let root = get_all(conn, bus, ROOT).unwrap_or_default();
                let text = |key: &str| -> String {
                    root.get(key)
                        .and_then(|v| v.downcast_ref::<&str>().ok().map(String::from))
                        .unwrap_or_default()
                };
                (text("Identity"), text("DesktopEntry"))
            })
            .clone();
        let flag = |key: &str| {
            all.get(key)
                .and_then(|v| v.downcast_ref::<bool>().ok())
                .unwrap_or(false)
        };
        let status = match all
            .get("PlaybackStatus")
            .and_then(|v| v.downcast_ref::<&str>().ok())
        {
            Some("Playing") => Status::Playing,
            Some("Paused") => Status::Paused,
            _ => Status::Stopped,
        };
        let meta: HashMap<String, OwnedValue> = all
            .get("Metadata")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| HashMap::<String, OwnedValue>::try_from(v).ok())
            .unwrap_or_default();
        let (title, artist, art_url, length) = parse_metadata(&meta);
        let position = all
            .get("Position")
            .and_then(|v| v.downcast_ref::<i64>().ok())
            .and_then(|p| u64::try_from(p).ok())
            .unwrap_or(0);
        Some(Player {
            bus: bus.into(),
            identity,
            desktop_entry,
            status,
            title,
            artist,
            art_url,
            length,
            position,
            can_next: flag("CanGoNext"),
            can_previous: flag("CanGoPrevious"),
            can_play: flag("CanPlay"),
            can_pause: flag("CanPause"),
            changed: 0,
        })
    }
}

fn get_all(conn: &Connection, bus: &str, interface: &str) -> Option<HashMap<String, OwnedValue>> {
    conn.call_method(
        Some(bus),
        PATH,
        Some("org.freedesktop.DBus.Properties"),
        "GetAll",
        &(interface,),
    )
    .ok()?
    .body()
    .deserialize()
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Str};

    fn value<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
        OwnedValue::try_from(v.into()).unwrap()
    }

    #[test]
    fn metadata_is_parsed() {
        let mut meta = HashMap::new();
        meta.insert("xesam:title".to_string(), value("Song"));
        let mut artists = Array::new(&zbus::zvariant::Signature::Str);
        artists.append(Value::from("A")).unwrap();
        artists.append(Value::from("B")).unwrap();
        meta.insert("xesam:artist".to_string(), value(artists));
        meta.insert(
            "mpris:artUrl".to_string(),
            value(Str::from("file:///a%20b.png")),
        );
        meta.insert("mpris:length".to_string(), value(215_000_000i64));
        let (title, artist, art, length) = parse_metadata(&meta);
        assert_eq!(title, "Song");
        assert_eq!(artist, "A, B");
        assert_eq!(art, "file:///a%20b.png");
        assert_eq!(length, Some(215_000_000));
        let p = Player {
            art_url: art,
            ..Player::default()
        };
        assert_eq!(p.art_path().as_deref(), Some("/a b.png"));
        // Nothing sent: nothing shown, and no panic.
        let (title, artist, _, length) = parse_metadata(&HashMap::new());
        assert!(title.is_empty() && artist.is_empty() && length.is_none());
        // A string artist and an unsigned length.
        let mut meta = HashMap::new();
        meta.insert("xesam:artist".to_string(), value("Solo"));
        meta.insert("mpris:length".to_string(), value(5u64));
        let (_, artist, _, length) = parse_metadata(&meta);
        assert_eq!((artist.as_str(), length), ("Solo", Some(5)));
    }

    fn player(bus: &str, status: Status, changed: u64) -> Player {
        Player {
            bus: bus.into(),
            status,
            changed,
            ..Player::default()
        }
    }

    #[test]
    fn playing_beats_paused_then_recency() {
        let players = [
            player("a", Status::Paused, 9),
            player("b", Status::Playing, 2),
            player("c", Status::Playing, 5),
            player("d", Status::Stopped, 99),
        ];
        assert_eq!(choose(&players), Some(2));
        assert_eq!(choose(&players[..1]), Some(0));
        assert_eq!(choose(&players[3..]), None);
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn a_picked_player_stays_on_show() {
        let mut m = Media {
            players: vec![
                player("a", Status::Paused, 1),
                player("b", Status::Playing, 2),
            ],
            ..Media::default()
        };
        assert_eq!(m.current().unwrap().bus, "b");
        m.selected = Some("a".into());
        assert_eq!(m.current().unwrap().bus, "a");
        // Gone: back to the usual choice.
        m.selected = Some("zzz".into());
        assert_eq!(m.current().unwrap().bus, "b");
    }

    #[test]
    fn progress() {
        let p = Player {
            length: Some(200),
            position: 50,
            ..Player::default()
        };
        assert_eq!(p.progress(), Some(0.25));
        assert_eq!(Player::default().progress(), None);
    }
}
