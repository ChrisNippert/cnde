//! System status for the panel: battery (UPower), network (NetworkManager or
//! iwd, ARCHITECTURE.md §5.2), volume (PipeWire via `pactl`), power mode
//! (power-profiles-daemon) and screen brightness (the kernel's backlight).
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
    /// Whether the Wi-Fi radio is on; `None` if we can't switch it.
    WifiEnabled(Option<bool>),
    Volume(Option<Volume>),
    PowerProfiles(Option<PowerProfiles>),
}

/// power-profiles-daemon's modes, in the order they're shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerProfile {
    PowerSaver,
    Balanced,
    Performance,
}

impl PowerProfile {
    pub const ALL: [PowerProfile; 3] = [Self::PowerSaver, Self::Balanced, Self::Performance];

    pub fn dbus_name(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    fn from_dbus(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.dbus_name() == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PowerProfiles {
    pub active: PowerProfile,
    /// Performance isn't offered on every machine.
    pub available: Vec<PowerProfile>,
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
    spawn("status-volume", tx.clone(), volume);
    spawn("status-power-profiles", tx, power_profiles);
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
        let _ = tx.send(Update::WifiEnabled(None));
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
    let radio = || {
        Update::WifiEnabled(get_all(&manager, NM).and_then(|all| value(&all, "WirelessEnabled")))
    };
    let (mut out, mut out_radio) = (Dedup::new(tx), Dedup::new(tx));
    out.send(Update::Network(read()));
    out_radio.send(radio());
    for _ in manager.receive_properties_changed()? {
        out.send(Update::Network(read()));
        out_radio.send(radio());
    }
    Ok(())
}

fn iwd(conn: &Connection, tx: &Sender<Update>) -> zbus::Result<()> {
    const IWD: &str = "net.connman.iwd";
    let objects = ObjectManagerProxy::builder(conn)
        .destination(IWD)?
        .path("/")?
        .build()?;
    // Everything iwd announces (devices powering on and off, stations
    // connecting, interfaces appearing) re-reads the whole state; it's
    // small, and Dedup drops the repeats.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(IWD)?
        .build();
    let signals = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(64))?;
    let (mut out, mut out_radio) = (Dedup::new(tx), Dedup::new(tx));
    let mut read = || -> zbus::Result<()> {
        let managed = objects.get_managed_objects()?;
        // The first wireless device, and whether its radio is on.
        let device = managed
            .iter()
            .find(|(_, ifaces)| ifaces.contains_key("net.connman.iwd.Device"));
        let powered = device.and_then(|(path, _)| {
            let props = props(conn, IWD, path.as_str()).ok()?;
            value::<bool>(&get_all(&props, "net.connman.iwd.Device")?, "Powered")
        });
        out_radio.send(Update::WifiEnabled(powered));
        let station = managed
            .iter()
            .find(|(_, ifaces)| ifaces.contains_key("net.connman.iwd.Station"))
            .map(|(path, _)| path.clone());
        let network = match (powered, station) {
            (Some(true), Some(path)) => iwd_station(conn, &path),
            _ => Network::Disconnected,
        };
        out.send(Update::Network(network));
        Ok(())
    };
    read()?;
    for _ in signals {
        read()?;
    }
    Ok(())
}

fn iwd_station(conn: &Connection, station_path: &OwnedObjectPath) -> Network {
    const IWD: &str = "net.connman.iwd";
    let Some(all) = props(conn, IWD, station_path.as_str())
        .ok()
        .and_then(|station| get_all(&station, "net.connman.iwd.Station"))
    else {
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

// --- Power mode: power-profiles-daemon.

fn power_profiles(tx: &Sender<Update>) -> zbus::Result<()> {
    let conn = Connection::system()?;
    let daemon = props(
        &conn,
        "net.hadess.PowerProfiles",
        "/net/hadess/PowerProfiles",
    )?;
    let read = || -> Option<PowerProfiles> {
        let all = get_all(&daemon, "net.hadess.PowerProfiles")?;
        let active: String = value(&all, "ActiveProfile")?;
        let profiles: Vec<HashMap<String, OwnedValue>> =
            value(&all, "Profiles").unwrap_or_default();
        let mut available: Vec<PowerProfile> = profiles
            .iter()
            .filter_map(|p| PowerProfile::from_dbus(&value::<String>(p, "Profile")?))
            .collect();
        available.sort_by_key(|p| PowerProfile::ALL.iter().position(|q| q == p));
        Some(PowerProfiles {
            active: PowerProfile::from_dbus(&active)?,
            available,
        })
    };
    let mut out = Dedup::new(tx);
    out.send(Update::PowerProfiles(read()));
    for _ in daemon.receive_properties_changed()? {
        out.send(Update::PowerProfiles(read()));
    }
    Ok(())
}

// --- Brightness: /sys/class/backlight. Read on demand (when quick settings
// opens, and after we change it); there's no change notification to wait on.

#[derive(Debug, Clone, PartialEq)]
pub struct Backlight {
    pub path: std::path::PathBuf,
    pub max: u32,
    pub current: u32,
}

impl Backlight {
    pub fn percent(&self) -> u32 {
        ((self.current as f64 / self.max.max(1) as f64) * 100.0).round() as u32
    }
}

/// The laptop panel's backlight, preferring the GPU's own over firmware ones.
pub fn read_backlight() -> Option<Backlight> {
    let mut devices: Vec<std::path::PathBuf> = std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    let kind = |path: &std::path::Path| {
        std::fs::read_to_string(path.join("type"))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    devices.sort_by_key(|p| match kind(p).as_str() {
        "raw" => 0,
        "platform" => 1,
        _ => 2,
    });
    let path = devices.into_iter().next()?;
    let number = |file: &str| -> Option<u32> {
        std::fs::read_to_string(path.join(file))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    Some(Backlight {
        max: number("max_brightness")?,
        current: number("brightness")?,
        path,
    })
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
