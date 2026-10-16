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
    SetWifi(bool),
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
            // A later volume in the queue makes this one pointless.
            if matches!(action, Action::SetVolume(_))
                && queue.iter().any(|a| matches!(a, Action::SetVolume(_)))
            {
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
        Action::SetWifi(enabled) => set_wifi(*enabled),
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

/// NetworkManager's radio switch. (iwd: power the device; not yet.)
fn set_wifi(enabled: bool) -> Result<(), String> {
    let conn = Connection::system().map_err(|err| err.to_string())?;
    let props = PropertiesProxy::builder(&conn)
        .destination("org.freedesktop.NetworkManager")
        .and_then(|b| b.path("/org/freedesktop/NetworkManager"))
        .and_then(|b| b.build())
        .map_err(|err| err.to_string())?;
    let interface = "org.freedesktop.NetworkManager"
        .try_into()
        .map_err(|err: zbus::names::Error| err.to_string())?;
    props
        .set(
            interface,
            "WirelessEnabled",
            zbus::zvariant::Value::from(enabled),
        )
        .map_err(|err| err.to_string())
}
