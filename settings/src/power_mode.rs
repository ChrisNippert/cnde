//! The power mode (Power Saver, Balanced, Performance) for the Power page,
//! through whichever daemon is on the system bus: power-profiles-daemon,
//! tuned-ppd or tlp-pd (`org.freedesktop.UPower.PowerProfiles`, or the older
//! `net.hadess.PowerProfiles`). A worker thread makes the D-Bus calls and
//! sends back the picture after each command and every few seconds, so a
//! change made elsewhere shows up too.

use std::{collections::HashMap, sync::mpsc, time::Duration};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::{debug, warn};
use zbus::{
    blocking::{Connection, fdo::PropertiesProxy},
    names::InterfaceName,
    zvariant::{OwnedValue, Value},
};

/// Bus name and path (the interface has the same name as the bus name),
/// newest first.
const SERVICES: [(&str, &str); 2] = [
    (
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
    ),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];

/// How often to look again.
const POLL: Duration = Duration::from_secs(3);

/// The modes in the order they're shown, as the daemons name them.
pub const MODES: [&str; 3] = ["power-saver", "balanced", "performance"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Availability {
    /// Still looking.
    #[default]
    Unknown,
    /// No daemon on the bus.
    NoService,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub availability: Availability,
    /// The daemon's name for the current mode.
    pub active: String,
    /// The modes this machine offers, in display order.
    pub available: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Look again now (the page was shown).
    Refresh,
    Set(String),
}

pub fn start(tx: Sender<State>) -> mpsc::Sender<Command> {
    let (commands, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("power-mode".into())
        .spawn(move || {
            let conn = match Connection::system() {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(%err, "no system bus; no power mode settings");
                    let _ = tx.send(State {
                        availability: Availability::NoService,
                        ..State::default()
                    });
                    return;
                }
            };
            run(&conn, &tx, rx);
        });
    commands
}

fn run(conn: &Connection, tx: &Sender<State>, rx: mpsc::Receiver<Command>) {
    let mut error = None;
    let mut last: Option<State> = None;
    loop {
        let state = read(conn, error.clone());
        if last.as_ref() != Some(&state) {
            let _ = tx.send(state.clone());
            last = Some(state);
        }
        match rx.recv_timeout(POLL) {
            Ok(Command::Set(mode)) => {
                debug!(mode, "power mode");
                error = set(conn, &mode).err().map(|err| {
                    warn!(%err, "can't set the power mode");
                    "Couldn't change the power mode".to_string()
                });
            }
            Ok(Command::Refresh) => error = None,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn props<'a>(conn: &Connection, name: &'a str, path: &'a str) -> zbus::Result<PropertiesProxy<'a>> {
    PropertiesProxy::builder(conn)
        .destination(name)?
        .path(path)?
        .build()
}

/// The first service that answers, with its properties.
fn find(conn: &Connection) -> Option<(&'static str, &'static str, HashMap<String, OwnedValue>)> {
    SERVICES.into_iter().find_map(|(name, path)| {
        let all = props(conn, name, path)
            .ok()?
            .get_all(InterfaceName::try_from(name).ok()?)
            .ok()?;
        Some((name, path, all))
    })
}

fn read(conn: &Connection, error: Option<String>) -> State {
    match find(conn) {
        Some((_, _, all)) => State {
            error,
            ..parse(&all)
        },
        None => State {
            availability: Availability::NoService,
            error,
            ..State::default()
        },
    }
}

fn set(conn: &Connection, mode: &str) -> zbus::Result<()> {
    let (name, path, _) = find(conn).ok_or_else(|| zbus::Error::Failure("no daemon".into()))?;
    props(conn, name, path)?.set(
        InterfaceName::try_from(name).map_err(zbus::Error::from)?,
        "ActiveProfile",
        Value::from(mode),
    )?;
    Ok(())
}

fn value<T: TryFrom<OwnedValue>>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    T::try_from(map.get(key)?.try_clone().ok()?).ok()
}

/// The daemon's properties as a state: `ActiveProfile` and the `Profiles`
/// list (an array of `{Profile: ...}` dictionaries), shown in the usual
/// order with any unknown mode ignored.
fn parse(all: &HashMap<String, OwnedValue>) -> State {
    let active: String = value(all, "ActiveProfile").unwrap_or_default();
    let profiles: Vec<HashMap<String, OwnedValue>> = value(all, "Profiles").unwrap_or_default();
    let names: Vec<String> = profiles
        .iter()
        .filter_map(|p| value::<String>(p, "Profile"))
        .collect();
    State {
        availability: Availability::Ready,
        available: MODES
            .iter()
            .filter(|m| names.iter().any(|n| n == *m))
            .map(|m| m.to_string())
            .collect(),
        active,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned<'a, T: Into<Value<'a>>>(v: T) -> OwnedValue {
        OwnedValue::try_from(v.into()).unwrap()
    }

    fn profile(name: &str) -> Value<'static> {
        let dict = HashMap::from([("Profile".to_string(), name.to_string())]);
        Value::from(zbus::zvariant::Dict::from(dict))
    }

    #[test]
    fn parses_the_daemons_properties() {
        let mut all = HashMap::new();
        all.insert("ActiveProfile".to_string(), owned("balanced"));
        let list =
            zbus::zvariant::Array::from(vec![profile("performance"), profile("power-saver")]);
        all.insert("Profiles".to_string(), owned(Value::from(list)));
        let state = parse(&all);
        assert_eq!(state.availability, Availability::Ready);
        assert_eq!(state.active, "balanced");
        // Shown in the usual order, and only what's offered.
        assert_eq!(state.available, ["power-saver", "performance"]);
    }

    #[test]
    fn missing_properties_leave_nothing_offered() {
        let state = parse(&HashMap::new());
        assert!(state.available.is_empty());
        assert_eq!(state.active, "");
    }

    #[test]
    fn the_new_bus_name_is_preferred() {
        assert_eq!(SERVICES[0].0, "org.freedesktop.UPower.PowerProfiles");
        assert_eq!(SERVICES[1].0, "net.hadess.PowerProfiles");
    }
}
