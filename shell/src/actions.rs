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

use smithay_client_toolkit::reexports::calloop::channel::Sender as UpdateSender;
use tracing::warn;

use crate::{audio::Kind, status::Update};
use zbus::blocking::{Connection, fdo::PropertiesProxy};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Percent of the default output.
    SetVolume(u32),
    ToggleMute,
    ToggleMicMute,
    /// Percent of the default input (its gain).
    SetMicVolume(u32),
    /// Make an output or input the default and bring its streams along.
    SelectDevice(Kind, String),
    /// Switch a device to a port (headphones, speakers) and select it.
    SelectPort(Kind, String, String),
    SetWifi(bool),
    /// Percent of the panel's maximum brightness.
    SetBrightness(u32),
    SetPowerProfile(crate::status::PowerProfile),
    /// No daemon: run `power.profile_command` (the string) with the mode.
    SetCustomProfile(crate::status::PowerProfile, String),
    /// The Bluetooth adapter on or off.
    SetBluetooth(bool),
    /// Connect (true) or disconnect a paired Bluetooth device, by address.
    ConnectBluetooth(String, bool),
    Lock,
    LogOut,
    Reboot,
    PowerOff,
}

#[derive(Clone)]
pub struct Actions(Sender<Action>);

impl Actions {
    /// `updates` carries what the actions learn back to the status.
    pub fn start(updates: UpdateSender<Update>) -> Self {
        let (tx, rx) = channel();
        let _ = thread::Builder::new()
            .name("actions".into())
            .spawn(move || worker(rx, updates));
        Self(tx)
    }

    pub fn run(&self, action: Action) {
        let _ = self.0.send(action);
    }
}

fn worker(rx: Receiver<Action>, updates: UpdateSender<Update>) {
    let mut queue = VecDeque::new();
    while let Ok(first) = rx.recv() {
        queue.push_back(first);
        queue.extend(rx.try_iter());
        while let Some(action) = queue.pop_front() {
            // A later value for the same slider makes this one pointless.
            let superseded = match action {
                Action::SetVolume(_) => queue.iter().any(|a| matches!(a, Action::SetVolume(_))),
                Action::SetMicVolume(_) => {
                    queue.iter().any(|a| matches!(a, Action::SetMicVolume(_)))
                }
                Action::SetBrightness(_) => {
                    queue.iter().any(|a| matches!(a, Action::SetBrightness(_)))
                }
                _ => false,
            };
            if superseded {
                continue;
            }
            perform(action, &updates);
        }
    }
}

fn perform(action: Action, updates: &UpdateSender<Update>) {
    let result = match &action {
        Action::SetVolume(percent) => {
            pactl(&["set-sink-volume", "@DEFAULT_SINK@", &format!("{percent}%")])
        }
        Action::ToggleMute => pactl(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]),
        Action::ToggleMicMute => pactl(&["set-source-mute", "@DEFAULT_SOURCE@", "toggle"]),
        Action::SetMicVolume(percent) => pactl(&[
            "set-source-volume",
            "@DEFAULT_SOURCE@",
            &format!("{percent}%"),
        ]),
        Action::SelectDevice(kind, name) => crate::audio::select(*kind, name),
        Action::SelectPort(kind, device, port) => crate::audio::select_port(*kind, device, port),
        Action::SetWifi(enabled) => set_wifi(*enabled),
        Action::SetBrightness(percent) => set_brightness(*percent),
        Action::SetPowerProfile(profile) => set_power_profile(*profile),
        Action::SetCustomProfile(profile, command) => {
            crate::apps::run_command(&custom_profile_line(command, *profile));
            let _ = updates.send(Update::CustomProfile(*profile));
            Ok(())
        }
        Action::SetBluetooth(on) => set_bluetooth(*on),
        Action::ConnectBluetooth(address, connect) => {
            // Connecting takes seconds: don't hold up the sliders.
            let (address, connect) = (address.clone(), *connect);
            let _ = thread::Builder::new()
                .name("bluetooth-connect".into())
                .spawn(move || {
                    if let Err(err) = connect_bluetooth(&address, connect) {
                        warn!(address, connect, %err, "bluetooth failed");
                    }
                });
            Ok(())
        }
        Action::Lock => compositor(&cnde_ipc::Request::LockScreen),
        Action::LogOut => compositor(&cnde_ipc::Request::LogOut),
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

/// A one-off request to the compositor, on a connection of its own.
fn compositor(request: &cnde_ipc::Request) -> Result<(), String> {
    use std::io::Write;
    let path = std::env::var_os(cnde_ipc::SOCKET_ENV).ok_or("no CNDE_SOCKET")?;
    std::os::unix::net::UnixStream::connect(path)
        .and_then(|mut stream| stream.write_all(&cnde_ipc::encode(request)))
        .map_err(|err| err.to_string())
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
    let (name, path) = crate::status::power_profile_service(&conn);
    set_property(
        &conn,
        name,
        path,
        name,
        "ActiveProfile",
        profile.dbus_name().into(),
    )
}

/// What `sh -c` runs for a custom command: the command, then the mode.
fn custom_profile_line(command: &str, profile: crate::status::PowerProfile) -> String {
    format!("{} {}", command.trim(), profile.dbus_name())
}

/// BlueZ's objects (see `status::read_bluez`).
fn bluez_objects(conn: &Connection) -> Result<crate::status::BluezObjects, String> {
    conn.call_method(
        Some("org.bluez"),
        "/",
        Some("org.freedesktop.DBus.ObjectManager"),
        "GetManagedObjects",
        &(),
    )
    .and_then(|reply| reply.body().deserialize())
    .map_err(|err| err.to_string())
}

/// The first adapter's path (hci0 before hci1).
fn bluez_adapter(objects: &crate::status::BluezObjects) -> Option<String> {
    objects
        .iter()
        .filter(|(_, ifaces)| ifaces.contains_key("org.bluez.Adapter1"))
        .map(|(path, _)| path.to_string())
        .min()
}

fn set_bluetooth(on: bool) -> Result<(), String> {
    let conn = Connection::system().map_err(|err| err.to_string())?;
    let adapter = bluez_adapter(&bluez_objects(&conn)?).ok_or("no Bluetooth adapter")?;
    set_property(
        &conn,
        "org.bluez",
        &adapter,
        "org.bluez.Adapter1",
        "Powered",
        on.into(),
    )
}

fn connect_bluetooth(address: &str, connect: bool) -> Result<(), String> {
    let conn = Connection::system().map_err(|err| err.to_string())?;
    let objects = bluez_objects(&conn)?;
    let adapter = bluez_adapter(&objects).ok_or("no Bluetooth adapter")?;
    let device = objects
        .iter()
        .find(|(_, ifaces)| {
            ifaces.get("org.bluez.Device1").is_some_and(|props| {
                let text = |key: &str| {
                    props
                        .get(key)
                        .and_then(|v| v.try_clone().ok())
                        .and_then(|v| String::try_from(v).ok())
                };
                text("Address").as_deref() == Some(address)
                    && props
                        .get("Adapter")
                        .and_then(|v| v.try_clone().ok())
                        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
                        .is_some_and(|p| p.as_str() == adapter)
            })
        })
        .map(|(path, _)| path.to_string())
        .ok_or("no such device")?;
    conn.call_method(
        Some("org.bluez"),
        device.as_str(),
        Some("org.bluez.Device1"),
        if connect { "Connect" } else { "Disconnect" },
        &(),
    )
    .map(|_| ())
    .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::PowerProfile;

    #[test]
    fn custom_profile_line_appends_the_mode() {
        assert_eq!(
            custom_profile_line(" my-tool set ", PowerProfile::PowerSaver),
            "my-tool set power-saver"
        );
        assert_eq!(
            custom_profile_line("x", PowerProfile::Performance),
            "x performance"
        );
    }
}
