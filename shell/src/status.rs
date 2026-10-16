//! System status for the panel: battery (UPower), network (NetworkManager or
//! iwd, ARCHITECTURE.md §5.2) and volume (PipeWire via `pactl`).
//!
//! Each source runs on its own thread, blocked on change notifications (no
//! polling), and sends `Update`s to the main loop only when its value
//! actually changes. A missing service just means that item isn't shown.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::debug;
use zbus::{
    blocking::{
        Connection,
        fdo::{DBusProxy, ObjectManagerProxy, PropertiesProxy},
    },
    names::{BusName, InterfaceName},
    zvariant::{OwnedObjectPath, OwnedValue},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Battery(Option<Battery>),
    Network(Network),
    Volume(Option<Volume>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Battery {
    pub percent: f64,
    pub charging: bool,
    pub full: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Network {
    /// No network service we know how to talk to.
    #[default]
    Unavailable,
    Disconnected,
    Connecting,
    Wired,
    Wifi {
        ssid: String,
        /// 0-100.
        strength: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume {
    pub percent: u32,
    pub muted: bool,
}

pub fn start(tx: Sender<Update>) {
    spawn("status-battery", tx.clone(), battery);
    spawn("status-network", tx.clone(), network);
    spawn("status-volume", tx, volume);
}

fn spawn(name: &str, tx: Sender<Update>, run: fn(&Sender<Update>) -> zbus::Result<()>) {
    let name = name.to_string();
    let _ = thread::Builder::new().name(name.clone()).spawn(move || {
        if let Err(err) = run(&tx) {
            debug!(source = %name, %err, "status source unavailable");
        }
    });
}

/// Sends `update` only if it differs from the last one sent.
struct Dedup<'a> {
    tx: &'a Sender<Update>,
    last: Option<Update>,
}

impl<'a> Dedup<'a> {
    fn new(tx: &'a Sender<Update>) -> Self {
        Self { tx, last: None }
    }

    fn send(&mut self, update: Update) {
        if self.last.as_ref() != Some(&update) {
            let _ = self.tx.send(update.clone());
            self.last = Some(update);
        }
    }
}

fn props<'a>(
    conn: &Connection,
    destination: &'a str,
    path: &'a str,
) -> zbus::Result<PropertiesProxy<'a>> {
    PropertiesProxy::builder(conn)
        .destination(destination)?
        .path(path)?
        .build()
}

fn get_all(proxy: &PropertiesProxy, interface: &str) -> Option<HashMap<String, OwnedValue>> {
    proxy.get_all(InterfaceName::try_from(interface).ok()?).ok()
}

fn value<T: TryFrom<OwnedValue>>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    T::try_from(map.get(key)?.try_clone().ok()?).ok()
}

// --- Battery: UPower's DisplayDevice is the combined laptop battery.

fn battery(tx: &Sender<Update>) -> zbus::Result<()> {
    let conn = Connection::system()?;
    let device = props(
        &conn,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower/devices/DisplayDevice",
    )?;
    let read = || -> Option<Battery> {
        let all = get_all(&device, "org.freedesktop.UPower.Device")?;
        if !value::<bool>(&all, "IsPresent")? {
            return None; // desktops: no battery, no icon
        }
        let state: u32 = value(&all, "State").unwrap_or(0);
        Some(Battery {
            percent: value(&all, "Percentage").unwrap_or(0.0),
            charging: state == 1, // Charging
            full: state == 4,     // FullyCharged
        })
    };
    let mut out = Dedup::new(tx);
    out.send(Update::Battery(read()));
    for _ in device.receive_properties_changed()? {
        out.send(Update::Battery(read()));
    }
    Ok(())
}

// --- Network: NetworkManager if running, else iwd.

fn network(tx: &Sender<Update>) -> zbus::Result<()> {
    let conn = Connection::system()?;
    let dbus = DBusProxy::new(&conn)?;
    let running = |name: &str| {
        BusName::try_from(name)
            .ok()
            .and_then(|name| dbus.name_has_owner(name).ok())
            .unwrap_or(false)
    };
    if running("org.freedesktop.NetworkManager") {
        network_manager(&conn, tx)
    } else if running("net.connman.iwd") {
        iwd(&conn, tx)
    } else {
        let _ = tx.send(Update::Network(Network::Unavailable));
        Ok(())
    }
}

fn network_manager(conn: &Connection, tx: &Sender<Update>) -> zbus::Result<()> {
    const NM: &str = "org.freedesktop.NetworkManager";
    let manager = props(conn, NM, "/org/freedesktop/NetworkManager")?;
    let read = || -> Network {
        let Some(all) = get_all(&manager, NM) else {
            return Network::Unavailable;
        };
        let state: u32 = value(&all, "State").unwrap_or(0);
        match state {
            0..=20 => return Network::Disconnected,
            30..=59 => return Network::Connecting,
            _ => {}
        }
        let kind: String = value(&all, "PrimaryConnectionType").unwrap_or_default();
        if kind != "802-11-wireless" {
            return Network::Wired;
        }
        // Active connection -> its access point -> SSID and strength.
        let wifi = || -> Option<Network> {
            let active: OwnedObjectPath = value(&all, "PrimaryConnection")?;
            let active = props(conn, NM, active.as_str()).ok()?;
            let ap: OwnedObjectPath = value(
                &get_all(&active, "org.freedesktop.NetworkManager.Connection.Active")?,
                "SpecificObject",
            )?;
            let ap = get_all(
                &props(conn, NM, ap.as_str()).ok()?,
                "org.freedesktop.NetworkManager.AccessPoint",
            )?;
            let ssid: Vec<u8> = value(&ap, "Ssid").unwrap_or_default();
            Some(Network::Wifi {
                ssid: String::from_utf8_lossy(&ssid).into_owned(),
                strength: value(&ap, "Strength").unwrap_or(0),
            })
        };
        wifi().unwrap_or(Network::Wifi {
            ssid: String::new(),
            strength: 0,
        })
    };
    let mut out = Dedup::new(tx);
    out.send(Update::Network(read()));
    for _ in manager.receive_properties_changed()? {
        out.send(Update::Network(read()));
    }
    Ok(())
}

fn iwd(conn: &Connection, tx: &Sender<Update>) -> zbus::Result<()> {
    const IWD: &str = "net.connman.iwd";
    let objects = ObjectManagerProxy::builder(conn)
        .destination(IWD)?
        .path("/")?
        .build()?;
    let mut out = Dedup::new(tx);
    loop {
        let managed = objects.get_managed_objects()?;
        let station = managed
            .iter()
            .find(|(_, ifaces)| ifaces.contains_key("net.connman.iwd.Station"))
            .map(|(path, _)| path.clone());
        let Some(station_path) = station else {
            // No wireless device (yet): check again later.
            out.send(Update::Network(Network::Disconnected));
            thread::sleep(Duration::from_secs(30));
            continue;
        };
        let station = props(conn, IWD, station_path.as_str())?;
        let read = || -> Network {
            let Some(all) = get_all(&station, "net.connman.iwd.Station") else {
                return Network::Disconnected;
            };
            let state: String = value(&all, "State").unwrap_or_default();
            match state.as_str() {
                "connected" => {}
                "connecting" | "roaming" => return Network::Connecting,
                _ => return Network::Disconnected,
            }
            let Some(network): Option<OwnedObjectPath> = value(&all, "ConnectedNetwork") else {
                return Network::Disconnected;
            };
            let ssid = props(conn, IWD, network.as_str())
                .ok()
                .and_then(|n| get_all(&n, "net.connman.iwd.Network"))
                .and_then(|n| value::<String>(&n, "Name"))
                .unwrap_or_default();
            // Signal strength (in 100 * dBm) from the station's scan results.
            let strength = conn
                .call_method(
                    Some(IWD),
                    station_path.as_str(),
                    Some("net.connman.iwd.Station"),
                    "GetOrderedNetworks",
                    &(),
                )
                .ok()
                .and_then(|reply| {
                    reply
                        .body()
                        .deserialize::<Vec<(OwnedObjectPath, i16)>>()
                        .ok()
                })
                .and_then(|list| list.into_iter().find(|(path, _)| path == &network))
                .map_or(0, |(_, rssi)| dbm_to_percent(rssi as f64 / 100.0));
            Network::Wifi { ssid, strength }
        };
        out.send(Update::Network(read()));
        for _ in station.receive_properties_changed()? {
            out.send(Update::Network(read()));
        }
    }
}

/// -90 dBm and below is nothing, -50 dBm and above is full.
fn dbm_to_percent(dbm: f64) -> u8 {
    (((dbm + 90.0) / 40.0).clamp(0.0, 1.0) * 100.0).round() as u8
}

// --- Volume: PipeWire has no D-Bus API; `pactl subscribe` reports changes.

fn volume(tx: &Sender<Update>) -> zbus::Result<()> {
    let mut out = Dedup::new(tx);
    loop {
        if let Ok(mut child) = Command::new("pactl")
            .arg("subscribe")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            out.send(Update::Volume(read_volume()));
            if let Some(stdout) = child.stdout.take() {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    // "Event 'change' on sink #56", "... on server", ...
                    if line.contains("sink") || line.contains("server") {
                        out.send(Update::Volume(read_volume()));
                    }
                }
            }
            let _ = child.wait();
        }
        // No audio server (yet, e.g. PipeWire still starting at login, or it
        // restarted): hide the icon and try again shortly.
        out.send(Update::Volume(None));
        thread::sleep(Duration::from_secs(3));
    }
}

fn read_volume() -> Option<Volume> {
    let run = |args: &[&str]| -> Option<String> {
        let output = Command::new("pactl")
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    // "Volume: front-left: 26214 /  40% / -23.88 dB,   front-right: ..."
    let volume = run(&["get-sink-volume", "@DEFAULT_SINK@"])?;
    let percent = volume
        .split('/')
        .nth(1)
        .and_then(|part| part.trim().trim_end_matches('%').parse().ok())?;
    let muted = run(&["get-sink-mute", "@DEFAULT_SINK@"]).is_some_and(|m| m.contains("yes"));
    Some(Volume { percent, muted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_strength_mapping() {
        assert_eq!(dbm_to_percent(-95.0), 0);
        assert_eq!(dbm_to_percent(-70.0), 50);
        assert_eq!(dbm_to_percent(-40.0), 100);
    }
}
