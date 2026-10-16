//! Things quick settings asks the system to do, carried out on a worker
//! thread so the UI never waits on a process or D-Bus call.
//!
//! Volume changes coalesce: dragging the slider queues a stream of values,
//! and only the latest is applied, so a drag doesn't launch a process per
//! pointer event.

use std::{
    collections::VecDeque,
    process::{Command, Stdio},
    sync::mpsc::{Receiver, Sender, channel},
    thread,
};

use tracing::warn;
use zbus::blocking::{Connection, fdo::PropertiesProxy};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Percent of the default output.
    SetVolume(u32),
    ToggleMute,
    ToggleMicMute,
    SetWifi(bool),
    /// Percent of the panel's maximum brightness.
    SetBrightness(u32),
    SetPowerProfile(crate::status::PowerProfile),
    Suspend,
    Reboot,
    PowerOff,
}

#[derive(Clone)]
pub struct Actions(Sender<Action>);

impl Actions {
    pub fn start() -> Self {
        let (tx, rx) = channel();
        let _ = thread::Builder::new()
            .name("actions".into())
            .spawn(move || worker(rx));
        Self(tx)
    }

    pub fn run(&self, action: Action) {
        let _ = self.0.send(action);
    }
}

fn worker(rx: Receiver<Action>) {
    let mut queue = VecDeque::new();
    while let Ok(first) = rx.recv() {
        queue.push_back(first);
        queue.extend(rx.try_iter());
        while let Some(action) = queue.pop_front() {
            // A later value for the same slider makes this one pointless.
            let superseded = match action {
                Action::SetVolume(_) => queue.iter().any(|a| matches!(a, Action::SetVolume(_))),
                Action::SetBrightness(_) => {
                    queue.iter().any(|a| matches!(a, Action::SetBrightness(_)))
                }
                _ => false,
            };
            if superseded {
                continue;
            }
            perform(action);
        }
    }
}

fn perform(action: Action) {
    let result = match &action {
        Action::SetVolume(percent) => {
            pactl(&["set-sink-volume", "@DEFAULT_SINK@", &format!("{percent}%")])
        }
        Action::ToggleMute => pactl(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]),
        Action::ToggleMicMute => pactl(&["set-source-mute", "@DEFAULT_SOURCE@", "toggle"]),
        Action::SetWifi(enabled) => set_wifi(*enabled),
        Action::SetBrightness(percent) => set_brightness(*percent),
        Action::SetPowerProfile(profile) => set_power_profile(*profile),
        Action::Suspend => login1("Suspend"),
        Action::Reboot => login1("Reboot"),
        Action::PowerOff => login1("PowerOff"),
    };
    if let Err(err) = result {
        warn!(?action, %err, "action failed");
    }
}

fn pactl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| err.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("pactl exited with {status}"))
}

/// systemd-logind, or elogind on non-systemd systems (same bus name).
fn login1(method: &str) -> Result<(), String> {
    let conn = Connection::system().map_err(|err| err.to_string())?;
    conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        method,
        // "interactive": let polkit ask if needed.
        &(true,),
    )
    .map(|_| ())
    .map_err(|err| err.to_string())
}

/// Set a D-Bus property on the system bus.
fn set_property(
    conn: &Connection,
    destination: &str,
    path: &str,
    interface: &str,
    name: &str,
    value: zbus::zvariant::Value<'_>,
) -> Result<(), String> {
    let props = PropertiesProxy::builder(conn)
        .destination(destination)
        .and_then(|b| b.path(path))
        .and_then(|b| b.build())
        .map_err(|err| err.to_string())?;
    let interface = interface
        .try_into()
        .map_err(|err: zbus::names::Error| err.to_string())?;
    props
        .set(interface, name, value)
        .map_err(|err| err.to_string())
}

/// The Wi-Fi radio: NetworkManager's switch if it's running, else iwd's
/// device power.
fn set_wifi(enabled: bool) -> Result<(), String> {
    let conn = Connection::system().map_err(|err| err.to_string())?;
    let nm = set_property(
        &conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
        "WirelessEnabled",
        enabled.into(),
    );
    if nm.is_ok() {
        return nm;
    }
    let objects = zbus::blocking::fdo::ObjectManagerProxy::builder(&conn)
        .destination("net.connman.iwd")
        .and_then(|b| b.path("/"))
        .and_then(|b| b.build())
        .map_err(|err| err.to_string())?;
    let devices: Vec<String> = objects
        .get_managed_objects()
        .map_err(|err| err.to_string())?
        .into_iter()
        .filter(|(_, ifaces)| ifaces.contains_key("net.connman.iwd.Device"))
        .map(|(path, _)| path.to_string())
        .collect();
    if devices.is_empty() {
        return Err("no NetworkManager or iwd Wi-Fi device".into());
    }
    for path in devices {
        set_property(
            &conn,
            "net.connman.iwd",
            &path,
            "net.connman.iwd.Device",
            "Powered",
            enabled.into(),
        )?;
    }
    Ok(())
}

/// Write the backlight directly (needs write access to the sysfs file,
/// which cnde's laptop setup grants the video group), else ask logind,
/// which allows it for the active session.
fn set_brightness(percent: u32) -> Result<(), String> {
    let backlight = crate::status::read_backlight().ok_or("no backlight")?;
    // Never all the way to black: a 0% panel looks like a dead screen.
    let raw = ((percent.clamp(1, 100) as f64 / 100.0) * backlight.max as f64).round() as u32;
    let raw = raw.max(1);
    let direct = std::fs::write(backlight.path.join("brightness"), raw.to_string());
    if direct.is_ok() {
        return Ok(());
    }
    let device = backlight
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let conn = Connection::system().map_err(|err| err.to_string())?;
    conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1/session/auto",
        Some("org.freedesktop.login1.Session"),
        "SetBrightness",
        &("backlight", device.as_str(), raw),
    )
    .map(|_| ())
    .map_err(|err| {
        format!(
            "can't write {} ({}) and logind refused ({err}); give the video group write access",
            backlight.path.display(),
            direct.err().map(|e| e.to_string()).unwrap_or_default()
        )
    })
}

fn set_power_profile(profile: crate::status::PowerProfile) -> Result<(), String> {
    let conn = Connection::system().map_err(|err| err.to_string())?;
    set_property(
        &conn,
        "net.hadess.PowerProfiles",
        "/net/hadess/PowerProfiles",
        "net.hadess.PowerProfiles",
        "ActiveProfile",
        profile.dbus_name().into(),
    )
}
