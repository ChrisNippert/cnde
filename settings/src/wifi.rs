//! Wi-Fi for the Settings page, through NetworkManager or iwd (whichever is
//! running) on the system bus. A worker thread makes the D-Bus calls: the
//! page sends it commands, and it sends back the whole picture after each
//! one and every few seconds while the window is open.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::{debug, warn};
use zbus::{
    blocking::{Connection, fdo::PropertiesProxy},
    names::InterfaceName,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Still looking.
    #[default]
    Unknown,
    /// Neither NetworkManager nor iwd is running.
    None,
    NetworkManager,
    Iwd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// 0-100.
    pub signal: u8,
    pub secured: bool,
    /// Saved: connects without asking for the password.
    pub known: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub backend: Backend,
    /// No Wi-Fi hardware (or it's missing from the backend).
    pub has_device: bool,
    pub enabled: bool,
    /// Connected first, then saved ones, then by signal.
    pub networks: Vec<Network>,
    /// Being connected to.
    pub connecting: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Scan,
    SetEnabled(bool),
    Connect {
        ssid: String,
        password: Option<String>,
    },
    Disconnect,
    Forget(String),
}

/// How often the list is re-read while nothing happens.
const POLL: Duration = Duration::from_secs(4);
/// How long a connection may take before it counts as failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

const NM: &str = "org.freedesktop.NetworkManager";
const IWD: &str = "net.connman.iwd";

/// Start the worker; state arrives on `tx`. Dropping the returned sender
/// stops it.
pub fn start(tx: Sender<State>) -> mpsc::Sender<Command> {
    let (commands, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("wifi".into())
        .spawn(move || {
            let conn = match Connection::system() {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(%err, "no system bus; no Wi-Fi settings");
                    let _ = tx.send(State {
                        backend: Backend::None,
                        ..State::default()
                    });
                    return;
                }
            };
            let mut worker = Worker::new(conn, tx);
            worker.run(rx);
        });
    commands
}

struct Worker {
    conn: Connection,
    tx: Sender<State>,
    backend: Backend,
    /// iwd asks its agent for passwords: what to answer.
    password: Arc<Mutex<Option<String>>>,
    agent_registered: bool,
    error: Option<String>,
    connecting: Option<String>,
}

impl Worker {
    fn new(conn: Connection, tx: Sender<State>) -> Self {
        let owned = |name: &str| -> bool {
            conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(name,),
            )
            .ok()
            .and_then(|reply| reply.body().deserialize::<bool>().ok())
            .unwrap_or(false)
        };
        let backend = if owned(NM) {
            Backend::NetworkManager
        } else if owned(IWD) {
            Backend::Iwd
        } else {
            Backend::None
        };
        Self {
            conn,
            tx,
            backend,
            password: Arc::new(Mutex::new(None)),
            agent_registered: false,
            error: None,
            connecting: None,
        }
    }

    fn run(&mut self, rx: mpsc::Receiver<Command>) {
        self.handle(Command::Scan);
        loop {
            match rx.recv_timeout(POLL) {
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => self.send(),
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn handle(&mut self, command: Command) {
        debug!(?command, "wifi");
        self.error = None;
        let result = match self.backend {
            Backend::NetworkManager => self.nm_command(&command),
            Backend::Iwd => self.iwd_command(&command),
            Backend::None | Backend::Unknown => Ok(()),
        };
        if let Err(err) = result {
            self.error = Some(describe(&command, &err));
        }
        // Connecting: keep the list current until it's up (or isn't).
        if let Command::Connect { ssid, .. } = &command
            && self.error.is_none()
        {
            self.connecting = Some(ssid.clone());
            let started = std::time::Instant::now();
            while started.elapsed() < CONNECT_TIMEOUT {
                let state = self.read();
                let up = state
                    .networks
                    .iter()
                    .any(|n| &n.ssid == ssid && n.connected);
                let _ = self.tx.send(state);
                if up {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            self.connecting = None;
            if !self
                .read()
                .networks
                .iter()
                .any(|n| &n.ssid == ssid && n.connected)
            {
                self.error = Some(format!("Couldn't connect to {ssid}"));
            }
        }
        if let Some(password) = self.password.lock().ok().as_mut() {
            **password = None;
        }
        self.send();
    }

    fn send(&self) {
        let _ = self.tx.send(self.read());
    }

    fn read(&self) -> State {
        let mut state = match self.backend {
            Backend::NetworkManager => self.nm_read().unwrap_or_default(),
            Backend::Iwd => self.iwd_read().unwrap_or_default(),
            backend => State {
                backend,
                ..State::default()
            },
        };
        state.backend = self.backend;
        state.error = self.error.clone();
        state.connecting = self.connecting.clone();
        state.networks.sort_by(|a, b| {
            (b.connected, b.known, b.signal).cmp(&(a.connected, a.known, a.signal))
        });
        state
    }

    fn props(&self, destination: &'static str, path: &str) -> zbus::Result<PropertiesProxy<'_>> {
        PropertiesProxy::builder(&self.conn)
            .destination(destination)?
            .path(path.to_string())?
            .build()
    }

    fn get_all(
        &self,
        destination: &'static str,
        path: &str,
        interface: &str,
    ) -> HashMap<String, OwnedValue> {
        self.props(destination, path)
            .ok()
            .and_then(|p| p.get_all(InterfaceName::try_from(interface).ok()?).ok())
            .unwrap_or_default()
    }

    fn call<B, R>(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
        method: &str,
        body: &B,
    ) -> zbus::Result<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply =
            self.conn
                .call_method(Some(destination), path, Some(interface), method, body)?;
        reply.body().deserialize::<R>()
    }

    // --- NetworkManager

    /// The first Wi-Fi device.
    fn nm_device(&self) -> Option<OwnedObjectPath> {
        let devices: Vec<OwnedObjectPath> = self
            .call(NM, "/org/freedesktop/NetworkManager", NM, "GetDevices", &())
            .ok()?;
        devices.into_iter().find(|path| {
            let all = self.get_all(NM, path.as_str(), "org.freedesktop.NetworkManager.Device");
            value::<u32>(&all, "DeviceType") == Some(2)
        })
    }

    /// Saved Wi-Fi connections: SSID -> settings path.
    fn nm_known(&self) -> HashMap<String, Vec<OwnedObjectPath>> {
        let mut known: HashMap<String, Vec<OwnedObjectPath>> = HashMap::new();
        let paths: Vec<OwnedObjectPath> = self
            .call(
                NM,
                "/org/freedesktop/NetworkManager/Settings",
                "org.freedesktop.NetworkManager.Settings",
                "ListConnections",
                &(),
            )
            .unwrap_or_default();
        for path in paths {
            let settings: HashMap<String, HashMap<String, OwnedValue>> = match self.call(
                NM,
                path.as_str(),
                "org.freedesktop.NetworkManager.Settings.Connection",
                "GetSettings",
                &(),
            ) {
                Ok(settings) => settings,
                Err(_) => continue,
            };
            let ssid = settings
                .get("802-11-wireless")
                .and_then(|w| value::<Vec<u8>>(w, "ssid"))
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            if let Some(ssid) = ssid {
                known.entry(ssid).or_default().push(path);
            }
        }
        known
    }

    fn nm_read(&self) -> Option<State> {
        let manager = self.get_all(NM, "/org/freedesktop/NetworkManager", NM);
        let enabled = value::<bool>(&manager, "WirelessEnabled").unwrap_or(false);
        let Some(device) = self.nm_device() else {
            return Some(State {
                enabled,
                ..State::default()
            });
        };
        let wireless = "org.freedesktop.NetworkManager.Device.Wireless";
        let active: Option<OwnedObjectPath> = value(
            &self.get_all(NM, device.as_str(), wireless),
            "ActiveAccessPoint",
        );
        let points: Vec<OwnedObjectPath> = self
            .call(NM, device.as_str(), wireless, "GetAllAccessPoints", &())
            .unwrap_or_default();
        let known = self.nm_known();
        let mut networks: Vec<Network> = Vec::new();
        for point in points {
            let all = self.get_all(
                NM,
                point.as_str(),
                "org.freedesktop.NetworkManager.AccessPoint",
            );
            let ssid = value::<Vec<u8>>(&all, "Ssid")
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            if ssid.is_empty() {
                continue;
            }
            let signal = value::<u8>(&all, "Strength").unwrap_or(0).min(100);
            let secured = value::<u32>(&all, "Flags").unwrap_or(0) & 1 != 0
                || value::<u32>(&all, "WpaFlags").unwrap_or(0) != 0
                || value::<u32>(&all, "RsnFlags").unwrap_or(0) != 0;
            let connected = active.as_ref() == Some(&point);
            // One row per name: the strongest access point.
            match networks.iter_mut().find(|n| n.ssid == ssid) {
                Some(existing) => {
                    existing.signal = existing.signal.max(signal);
                    existing.connected |= connected;
                }
                None => networks.push(Network {
                    known: known.contains_key(&ssid),
                    ssid,
                    signal,
                    secured,
                    connected,
                }),
            }
        }
        Some(State {
            has_device: true,
            enabled,
            networks,
            ..State::default()
        })
    }

    fn nm_command(&mut self, command: &Command) -> zbus::Result<()> {
        let device = self.nm_device();
        let wireless = "org.freedesktop.NetworkManager.Device.Wireless";
        match command {
            Command::Scan => {
                if let Some(device) = device {
                    let options: HashMap<&str, Value> = HashMap::new();
                    // Refused if a scan ran just now; the list is fresh then.
                    let _: zbus::Result<()> =
                        self.call(NM, device.as_str(), wireless, "RequestScan", &(options,));
                }
            }
            Command::SetEnabled(on) => {
                self.props(NM, "/org/freedesktop/NetworkManager")?.set(
                    InterfaceName::try_from(NM).map_err(zbus::Error::from)?,
                    "WirelessEnabled",
                    Value::from(*on),
                )?;
            }
            Command::Disconnect => {
                if let Some(device) = device {
                    self.call::<_, ()>(
                        NM,
                        device.as_str(),
                        "org.freedesktop.NetworkManager.Device",
                        "Disconnect",
                        &(),
                    )?;
                }
            }
            Command::Forget(ssid) => {
                for path in self.nm_known().remove(ssid).unwrap_or_default() {
                    self.call::<_, ()>(
                        NM,
                        path.as_str(),
                        "org.freedesktop.NetworkManager.Settings.Connection",
                        "Delete",
                        &(),
                    )?;
                }
            }
            Command::Connect { ssid, password } => {
                let Some(device) = device else {
                    return Err(zbus::Error::Failure("no Wi-Fi device".into()));
                };
                let root = ObjectPath::try_from("/").map_err(zbus::Error::from)?;
                let known = self.nm_known().remove(ssid).unwrap_or_default();
                match (known.first(), password) {
                    // Saved, and no new password: just bring it up.
                    (Some(path), None) => {
                        self.call::<_, (OwnedObjectPath,)>(
                            NM,
                            "/org/freedesktop/NetworkManager",
                            NM,
                            "ActivateConnection",
                            &(path, &device, &root),
                        )?;
                    }
                    _ => {
                        // A new (or re-entered) one: drop the old profile.
                        for path in &known {
                            let _: zbus::Result<()> = self.call(
                                NM,
                                path.as_str(),
                                "org.freedesktop.NetworkManager.Settings.Connection",
                                "Delete",
                                &(),
                            );
                        }
                        let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
                        settings.insert(
                            "802-11-wireless",
                            HashMap::from([("ssid", Value::from(ssid.as_bytes().to_vec()))]),
                        );
                        if let Some(password) = password {
                            settings.insert(
                                "802-11-wireless-security",
                                HashMap::from([
                                    ("key-mgmt", Value::from("wpa-psk")),
                                    ("psk", Value::from(password.as_str())),
                                ]),
                            );
                        }
                        self.call::<_, (OwnedObjectPath, OwnedObjectPath)>(
                            NM,
                            "/org/freedesktop/NetworkManager",
                            NM,
                            "AddAndActivateConnection",
                            &(settings, &device, &root),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    // --- iwd

    fn iwd_objects(
        &self,
    ) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        self.call(
            IWD,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            &(),
        )
        .unwrap_or_default()
    }

    fn iwd_read(&self) -> Option<State> {
        let objects = self.iwd_objects();
        let Some((device, device_props)) = objects
            .iter()
            .find_map(|(path, ifaces)| Some((path, ifaces.get("net.connman.iwd.Device")?)))
        else {
            return Some(State::default());
        };
        let enabled = value::<bool>(device_props, "Powered").unwrap_or(false);
        let mut networks = Vec::new();
        if enabled
            && objects
                .get(device)
                .is_some_and(|i| i.contains_key("net.connman.iwd.Station"))
        {
            let ordered: Vec<(OwnedObjectPath, i16)> = self
                .call(
                    IWD,
                    device.as_str(),
                    "net.connman.iwd.Station",
                    "GetOrderedNetworks",
                    &(),
                )
                .unwrap_or_default();
            for (path, rssi) in ordered {
                let Some(props) = objects
                    .get(&path)
                    .and_then(|i| i.get("net.connman.iwd.Network"))
                else {
                    continue;
                };
                let ssid: String = value(props, "Name").unwrap_or_default();
                if ssid.is_empty() || networks.iter().any(|n: &Network| n.ssid == ssid) {
                    continue;
                }
                let kind: String = value(props, "Type").unwrap_or_default();
                networks.push(Network {
                    ssid,
                    signal: dbm_to_percent(rssi as f64 / 100.0),
                    secured: kind != "open",
                    known: props.contains_key("KnownNetwork"),
                    connected: value::<bool>(props, "Connected").unwrap_or(false),
                });
            }
        }
        Some(State {
            has_device: true,
            enabled,
            networks,
            ..State::default()
        })
    }

    /// The network object called `ssid`.
    fn iwd_network(&self, ssid: &str) -> Option<(OwnedObjectPath, HashMap<String, OwnedValue>)> {
        self.iwd_objects()
            .into_iter()
            .find_map(|(path, mut ifaces)| {
                let props = ifaces.remove("net.connman.iwd.Network")?;
                (value::<String>(&props, "Name").as_deref() == Some(ssid)).then_some((path, props))
            })
    }

    fn iwd_command(&mut self, command: &Command) -> zbus::Result<()> {
        let objects = self.iwd_objects();
        let device = objects
            .iter()
            .find(|(_, ifaces)| ifaces.contains_key("net.connman.iwd.Device"))
            .map(|(path, _)| path.clone());
        match command {
            Command::Scan => {
                if let Some(device) = device {
                    // Refused while one is running; that's fine.
                    let _: zbus::Result<()> =
                        self.call(IWD, device.as_str(), "net.connman.iwd.Station", "Scan", &());
                }
            }
            Command::SetEnabled(on) => {
                if let Some(device) = device {
                    self.props(IWD, device.as_str())?.set(
                        InterfaceName::try_from("net.connman.iwd.Device")
                            .map_err(zbus::Error::from)?,
                        "Powered",
                        Value::from(*on),
                    )?;
                }
            }
            Command::Disconnect => {
                if let Some(device) = device {
                    self.call::<_, ()>(
                        IWD,
                        device.as_str(),
                        "net.connman.iwd.Station",
                        "Disconnect",
                        &(),
                    )?;
                }
            }
            Command::Forget(ssid) => {
                if let Some((_, props)) = self.iwd_network(ssid)
                    && let Some(known) = value::<OwnedObjectPath>(&props, "KnownNetwork")
                {
                    self.call::<_, ()>(
                        IWD,
                        known.as_str(),
                        "net.connman.iwd.KnownNetwork",
                        "Forget",
                        &(),
                    )?;
                }
            }
            Command::Connect { ssid, password } => {
                self.iwd_agent()?;
                if let Ok(mut slot) = self.password.lock() {
                    *slot = password.clone();
                }
                let Some((path, _)) = self.iwd_network(ssid) else {
                    return Err(zbus::Error::Failure(format!("{ssid} is out of range")));
                };
                // Blocks until connected (or failed); passwords come from
                // the agent.
                self.call::<_, ()>(
                    IWD,
                    path.as_str(),
                    "net.connman.iwd.Network",
                    "Connect",
                    &(),
                )?;
            }
        }
        Ok(())
    }

    /// iwd asks an agent for passwords: ours answers with what was typed.
    fn iwd_agent(&mut self) -> zbus::Result<()> {
        if self.agent_registered {
            return Ok(());
        }
        let agent = Agent {
            password: self.password.clone(),
        };
        self.conn.object_server().at(AGENT_PATH, agent)?;
        let path = ObjectPath::try_from(AGENT_PATH).map_err(zbus::Error::from)?;
        self.call::<_, ()>(
            IWD,
            "/net/connman/iwd",
            "net.connman.iwd.AgentManager",
            "RegisterAgent",
            &(path,),
        )?;
        self.agent_registered = true;
        Ok(())
    }
}

const AGENT_PATH: &str = "/org/cnde/settings/wifi_agent";

struct Agent {
    password: Arc<Mutex<Option<String>>>,
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl Agent {
    fn release(&self) {}

    fn request_passphrase(&self, _network: ObjectPath<'_>) -> zbus::fdo::Result<String> {
        self.password
            .lock()
            .ok()
            .and_then(|p| p.clone())
            .ok_or_else(|| zbus::fdo::Error::Failed("no password".into()))
    }

    fn cancel(&self, _reason: String) {}
}

fn value<T: TryFrom<OwnedValue>>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    T::try_from(map.get(key)?.try_clone().ok()?).ok()
}

fn dbm_to_percent(dbm: f64) -> u8 {
    // -90 dBm is unusable, -30 is as good as it gets.
    (((dbm + 90.0) / 60.0) * 100.0).clamp(0.0, 100.0) as u8
}

/// What went wrong, in words for the page.
fn describe(command: &Command, err: &zbus::Error) -> String {
    let text = err.to_string();
    let denied = text.contains("NotAuthorized")
        || text.contains("PermissionDenied")
        || text.contains("AccessDenied")
        || text.contains("not authorized");
    let what = match command {
        Command::Scan => "Couldn't look for networks".to_string(),
        Command::SetEnabled(true) => "Couldn't turn Wi-Fi on".to_string(),
        Command::SetEnabled(false) => "Couldn't turn Wi-Fi off".to_string(),
        Command::Connect { ssid, .. } => format!("Couldn't connect to {ssid}"),
        Command::Disconnect => "Couldn't disconnect".to_string(),
        Command::Forget(ssid) => format!("Couldn't forget {ssid}"),
    };
    if denied {
        format!("{what}: not allowed (polkit)")
    } else {
        warn!(%text, "wifi");
        what
    }
}

/// Diagnostics, read-only (scans and lists, never connects):
/// `cargo test -p cnde-settings reads_this_machine -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn reads_this_machine() {
    use smithay_client_toolkit::reexports::calloop::{EventLoop, channel};
    let mut event_loop: EventLoop<()> = EventLoop::try_new().unwrap();
    let (tx, rx) = channel::channel::<State>();
    event_loop
        .handle()
        .insert_source(rx, |event, _, _| {
            if let channel::Event::Msg(state) = event {
                eprintln!(
                    "backend={:?} device={} enabled={} networks={} connected={} error={:?}",
                    state.backend,
                    state.has_device,
                    state.enabled,
                    state.networks.len(),
                    state.networks.iter().filter(|n| n.connected).count(),
                    state.error
                );
            }
        })
        .unwrap();
    let _commands = start(tx);
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(6) {
        event_loop
            .dispatch(Some(Duration::from_millis(500)), &mut ())
            .unwrap();
    }
}
