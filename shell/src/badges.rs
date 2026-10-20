//! Dock icon badges from the `com.canonical.Unity.LauncherEntry` signal
//! (what Unity set up and most apps that show a launcher count still send):
//! `Update(app_uri, properties)` with `count`, `progress` and `urgent`.
//!
//! Two zbus threads listen on the session bus (the signal's `Update`, and
//! names vanishing) and hand what they hear to the main loop over a calloop
//! channel, so the dock never waits on D-Bus.

use std::collections::{HashMap, HashSet};

use smithay_client_toolkit::reexports::calloop::{
    LoopHandle,
    channel::{self, Sender},
};
use tracing::{debug, warn};
use zbus::zvariant::Value;

use crate::{App, dock::Item};

const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";

/// What's drawn on one dock icon.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Badge {
    pub count: Option<u32>,
    /// 0 to 1.
    pub progress: Option<f32>,
    pub urgent: bool,
}

impl Badge {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The properties one signal carried; apps send only what changed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Delta {
    count: Option<i64>,
    count_visible: Option<bool>,
    progress: Option<f64>,
    progress_visible: Option<bool>,
    urgent: Option<bool>,
}

fn int(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::I64(v) => Some(*v),
        Value::I32(v) => Some(*v as i64),
        Value::U32(v) => Some(*v as i64),
        Value::U64(v) => i64::try_from(*v).ok(),
        Value::I16(v) => Some(*v as i64),
        Value::U16(v) => Some(*v as i64),
        Value::Value(inner) => int(inner),
        _ => None,
    }
}

fn float(value: &Value<'_>) -> Option<f64> {
    match value {
        Value::F64(v) => Some(*v),
        Value::Value(inner) => float(inner),
        other => int(other).map(|v| v as f64),
    }
}

fn flag(value: &Value<'_>) -> Option<bool> {
    match value {
        Value::Bool(v) => Some(*v),
        Value::Value(inner) => flag(inner),
        _ => None,
    }
}

impl Delta {
    pub fn parse<'a>(props: impl IntoIterator<Item = (&'a str, &'a Value<'a>)>) -> Self {
        let mut delta = Self::default();
        for (key, value) in props {
            match key {
                "count" => delta.count = int(value),
                "count-visible" => delta.count_visible = flag(value),
                "progress" => delta.progress = float(value),
                "progress-visible" => delta.progress_visible = flag(value),
                "urgent" => delta.urgent = flag(value),
                _ => {}
            }
        }
        delta
    }
}

/// An app's properties as last set (a signal may change just one).
#[derive(Debug, Clone, PartialEq, Default)]
struct Props {
    count: i64,
    count_visible: bool,
    progress: f64,
    progress_visible: bool,
    urgent: bool,
}

impl Props {
    fn apply(&mut self, delta: &Delta) {
        if let Some(v) = delta.count {
            self.count = v;
        }
        if let Some(v) = delta.count_visible {
            self.count_visible = v;
        }
        if let Some(v) = delta.progress {
            self.progress = v;
        }
        if let Some(v) = delta.progress_visible {
            self.progress_visible = v;
        }
        if let Some(v) = delta.urgent {
            self.urgent = v;
        }
    }

    fn badge(&self) -> Badge {
        Badge {
            count: (self.count_visible && self.count > 0)
                .then(|| self.count.min(u32::MAX as i64) as u32),
            progress: self
                .progress_visible
                .then(|| self.progress.clamp(0.0, 1.0) as f32),
            urgent: self.urgent,
        }
    }
}

/// `application://firefox.desktop` -> `firefox.desktop`.
pub fn desktop_id(uri: &str) -> Option<&str> {
    uri.strip_prefix("application://")
        .filter(|id| !id.is_empty())
}

/// The text in a count pill: "99+" past two digits.
pub fn count_text(count: u32) -> String {
    if count > 99 {
        "99+".into()
    } else {
        count.to_string()
    }
}

pub enum Event {
    Update {
        /// The sender's unique bus name.
        sender: String,
        id: String,
        delta: Delta,
    },
    /// This unique name left the bus.
    Gone(String),
}

#[derive(Default)]
pub struct Badges {
    /// Per desktop ID: who sent it, and what they said.
    entries: HashMap<String, (String, Props)>,
    /// Apps with windows at the last sync, to see which stopped.
    running: HashSet<String>,
}

impl Badges {
    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Update { sender, id, delta } => {
                let entry = self.entries.entry(id).or_default();
                entry.0 = sender;
                let before = entry.1.badge();
                entry.1.apply(&delta);
                before != entry.1.badge()
            }
            Event::Gone(name) => {
                let before = self.entries.len();
                self.entries.retain(|_, (sender, _)| *sender != name);
                self.entries.len() != before
            }
        }
    }

    /// Put each icon's badge on it, and forget the badges of apps that were
    /// running and no longer are.
    pub fn apply(&mut self, items: &mut [Item]) {
        let running: HashSet<String> = items
            .iter()
            .filter(|i| !i.windows.is_empty())
            .filter_map(|i| i.app.clone())
            .collect();
        for stopped in self.running.difference(&running) {
            self.entries.remove(stopped);
        }
        self.running = running;
        for item in items {
            item.badge = item
                .app
                .as_ref()
                .and_then(|id| self.entries.get(id))
                .map(|(_, props)| props.badge())
                .unwrap_or_default();
        }
    }
}

/// Start listening; events arrive in the main loop.
pub fn setup(handle: &LoopHandle<'static, App>) -> Badges {
    let (tx, rx) = channel::channel::<Event>();
    let inserted = handle.insert_source(rx, |event, _, app: &mut App| {
        if let channel::Event::Msg(event) = event
            && app.badges.event(event)
        {
            app.sync_dock();
        }
    });
    if inserted.is_ok() {
        let _ = std::thread::Builder::new()
            .name("launcher-badges".into())
            .spawn(move || {
                if let Err(err) = listen(tx) {
                    warn!(%err, "can't listen for launcher badges");
                }
            });
    }
    Badges::default()
}

fn listen(tx: Sender<Event>) -> zbus::Result<()> {
    use zbus::{MatchRule, blocking::MessageIterator, message::Type};

    let conn = zbus::blocking::Connection::session()?;
    let updates = MessageIterator::for_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(INTERFACE)?
            .member("Update")?
            .build(),
        &conn,
        None,
    )?;
    let names = MessageIterator::for_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .build(),
        &conn,
        None,
    )?;
    let gone = tx.clone();
    let _ = std::thread::Builder::new()
        .name("launcher-badges-names".into())
        .spawn(move || {
            for message in names.flatten() {
                let Ok((_name, old, new)) =
                    message.body().deserialize::<(String, String, String)>()
                else {
                    continue;
                };
                if new.is_empty() && !old.is_empty() && gone.send(Event::Gone(old)).is_err() {
                    break;
                }
            }
        });
    for message in updates.flatten() {
        let Some(sender) = message.header().sender().map(|s| s.to_string()) else {
            continue;
        };
        let body = message.body();
        let Ok((uri, props)) = body.deserialize::<(String, HashMap<String, Value<'_>>)>() else {
            continue;
        };
        let Some(id) = desktop_id(&uri) else {
            continue;
        };
        debug!(%sender, id, "launcher entry update");
        let delta = Delta::parse(props.iter().map(|(k, v)| (k.as_str(), v)));
        let id = id.to_string();
        if tx.send(Event::Update { sender, id, delta }).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(props: &[(&str, Value<'static>)]) -> Delta {
        Delta::parse(props.iter().map(|(k, v)| (*k, v)))
    }

    #[test]
    fn properties_become_a_badge() {
        let mut props = Props::default();
        props.apply(&parse(&[
            ("count", Value::I64(3)),
            ("count-visible", Value::Bool(true)),
            ("progress", Value::F64(0.4)),
            ("progress-visible", Value::Bool(true)),
            ("urgent", Value::Bool(true)),
        ]));
        assert_eq!(
            props.badge(),
            Badge {
                count: Some(3),
                progress: Some(0.4),
                urgent: true
            }
        );
    }

    #[test]
    fn hidden_and_empty_parts_are_left_off() {
        let mut props = Props::default();
        props.apply(&parse(&[("count", Value::I64(5))]));
        assert!(props.badge().is_empty());
        props.apply(&parse(&[("count-visible", Value::Bool(true))]));
        assert_eq!(props.badge().count, Some(5));
        // A later signal changes only what it names.
        props.apply(&parse(&[("count", Value::I64(0))]));
        assert_eq!(props.badge().count, None);
        props.apply(&parse(&[
            ("progress", Value::F64(7.0)),
            ("progress-visible", Value::Bool(true)),
        ]));
        assert_eq!(props.badge().progress, Some(1.0));
    }

    #[test]
    fn odd_integer_types_still_count() {
        let delta = parse(&[("count", Value::I32(12)), ("progress", Value::I32(1))]);
        assert_eq!(delta.count, Some(12));
        assert_eq!(delta.progress, Some(1.0));
    }

    #[test]
    fn uris_and_counts() {
        assert_eq!(
            desktop_id("application://firefox.desktop"),
            Some("firefox.desktop")
        );
        assert_eq!(desktop_id("file:///x"), None);
        assert_eq!(count_text(7), "7");
        assert_eq!(count_text(99), "99");
        assert_eq!(count_text(100), "99+");
    }

    #[test]
    fn vanishing_senders_take_their_badges() {
        let mut badges = Badges::default();
        let update = |sender: &str, id: &str| Event::Update {
            sender: sender.into(),
            id: id.into(),
            delta: parse(&[("urgent", Value::Bool(true))]),
        };
        assert!(badges.event(update(":1.5", "a.desktop")));
        assert!(badges.event(update(":1.6", "b.desktop")));
        assert!(badges.event(Event::Gone(":1.5".into())));
        assert!(!badges.entries.contains_key("a.desktop"));
        assert!(badges.entries.contains_key("b.desktop"));
        assert!(!badges.event(Event::Gone(":1.9".into())));
    }
}
