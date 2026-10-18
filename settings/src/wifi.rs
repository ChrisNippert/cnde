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

/// How a network is protected, which decides what joining it asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Security {
    #[default]
    Open,
    /// OWE: encrypted, but no password.
    EnhancedOpen,
    Wep,
    /// WPA or WPA2 with a password.
    Psk,
    /// WPA3 with a password.
    Sae,
    /// Takes either (WPA2/WPA3 transition).
    PskSae,
    /// 802.1X: a username and password (or certificate) per person.
    Enterprise,
}

impl Security {
    /// What a hidden network can be joined as (WPA2/WPA3 joins as WPA2).
    pub const CHOICES: [Security; 5] = [
        Security::Open,
        Security::Psk,
        Security::Sae,
        Security::Wep,
        Security::Enterprise,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Security::Open => "Open",
            Security::EnhancedOpen => "Enhanced Open",
            Security::Wep => "WEP",
            Security::Psk => "WPA2 Personal",
            Security::Sae => "WPA3 Personal",
            Security::PskSae => "WPA2/WPA3 Personal",
            Security::Enterprise => "Enterprise",
        }
    }

    /// For config-style values (the hidden network's dropdown).
    pub fn name(self) -> &'static str {
        match self {
            Security::Open => "open",
            Security::EnhancedOpen => "owe",
            Security::Wep => "wep",
            Security::Psk => "wpa2",
            Security::Sae => "wpa3",
            Security::PskSae => "wpa2-wpa3",
            Security::Enterprise => "enterprise",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [
            Security::Open,
            Security::EnhancedOpen,
            Security::Wep,
            Security::Psk,
            Security::Sae,
            Security::PskSae,
            Security::Enterprise,
        ]
        .into_iter()
        .find(|s| s.name() == name)
    }

    /// Shows a lock.
    pub fn secured(self) -> bool {
        !matches!(self, Security::Open | Security::EnhancedOpen)
    }

    /// Joining asks for one password (or WEP key).
    pub fn takes_password(self) -> bool {
        matches!(
            self,
            Security::Wep | Security::Psk | Security::Sae | Security::PskSae
        )
    }
}

/// 802.1X methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Eap {
    #[default]
    Peap,
    Ttls,
    Tls,
    Pwd,
}

impl Eap {
    pub const ALL: [Eap; 4] = [Eap::Peap, Eap::Ttls, Eap::Tls, Eap::Pwd];

    pub fn label(self) -> &'static str {
        match self {
            Eap::Peap => "PEAP",
            Eap::Ttls => "TTLS",
            Eap::Tls => "TLS (certificate)",
            Eap::Pwd => "PWD",
        }
    }

    /// NetworkManager's name (and iwd's, upper-cased).
    pub fn name(self) -> &'static str {
        match self {
            Eap::Peap => "peap",
            Eap::Ttls => "ttls",
            Eap::Tls => "tls",
            Eap::Pwd => "pwd",
        }
    }

    /// Has an inner method inside the tunnel.
    pub fn tunnelled(self) -> bool {
        matches!(self, Eap::Peap | Eap::Ttls)
    }
}

/// The inner ("phase 2") method of PEAP and TTLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Phase2 {
    #[default]
    Mschapv2,
    Pap,
    Mschap,
    Gtc,
}

impl Phase2 {
    pub const ALL: [Phase2; 4] = [Phase2::Mschapv2, Phase2::Pap, Phase2::Mschap, Phase2::Gtc];

    pub fn label(self) -> &'static str {
        match self {
            Phase2::Mschapv2 => "MSCHAPv2",
            Phase2::Pap => "PAP",
            Phase2::Mschap => "MSCHAP",
            Phase2::Gtc => "GTC",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Phase2::Mschapv2 => "mschapv2",
            Phase2::Pap => "pap",
            Phase2::Mschap => "mschap",
            Phase2::Gtc => "gtc",
        }
    }
}

/// Everything needed to join a network that isn't saved (or to replace
/// what's saved).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Join {
    pub ssid: String,
    /// Doesn't announce itself: look for it by name.
    pub hidden: bool,
    pub security: Security,
    /// The password, WEP key, or (TLS) the private key's password.
    pub password: String,
    pub eap: Eap,
    pub phase2: Phase2,
    pub identity: String,
    pub anonymous_identity: String,
    /// The server's CA certificate file; empty: don't check, unless
    /// `domain` is set (then the system's certificates check it).
    pub ca_cert: String,
    pub domain: String,
    pub client_cert: String,
    pub private_key: String,
}

impl Join {
    /// What's missing before it can connect, in words.
    pub fn missing(&self) -> Option<&'static str> {
        if self.ssid.trim().is_empty() {
            return Some("Enter the network's name");
        }
        match self.security {
            Security::Psk | Security::Sae | Security::PskSae if self.password.len() < 8 => {
                Some("WPA passwords are at least 8 characters")
            }
            Security::Wep if self.password.is_empty() => Some("Enter the WEP key"),
            Security::Enterprise => {
                if self.identity.trim().is_empty() {
                    Some("Enter your username")
                } else if self.eap == Eap::Tls
                    && (self.client_cert.is_empty() || self.private_key.is_empty())
                {
                    Some("TLS needs your certificate and private key")
                } else if self.eap != Eap::Tls && self.password.is_empty() {
                    Some("Enter your password")
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// 0-100.
    pub signal: u8,
    pub security: Security,
    /// Saved: connects without asking for the password.
    pub known: bool,
    pub connected: bool,
    /// Seen by the last scan (saved networks show even when they aren't).
    pub in_range: bool,
    /// A saved network that's joined by itself when it's around.
    pub autoconnect: bool,
}

/// A saved NetworkManager connection (or several with the same name).
struct Saved {
    paths: Vec<OwnedObjectPath>,
    security: Security,
    autoconnect: bool,
}

/// About the connection that's up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Details {
    pub interface: String,
    /// iwd says exactly (e.g. "WPA3-Personal"); NetworkManager's comes
    /// from the access point.
    pub security: String,
    pub dbm: Option<i32>,
    pub frequency: Option<u32>,
    /// Mb/s.
    pub bitrate: Option<u32>,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
    pub mac: Option<String>,
    /// The access point's address.
    pub bssid: Option<String>,
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
    /// About the connected network.
    pub details: Option<Details>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Scan,
    SetEnabled(bool),
    /// Bring up a saved network, or an open one.
    Activate(String),
    /// Join with these details, replacing anything saved for it.
    Join(Join),
    Disconnect,
    Forget(String),
    /// Join a saved network by itself when it's around, or not.
    SetAutoConnect(String, bool),
}

impl Command {
    /// The network a connect command is for.
    fn joining(&self) -> Option<&str> {
        match self {
            Command::Activate(ssid) => Some(ssid),
            Command::Join(join) => Some(&join.ssid),
            _ => None,
        }
    }
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
    secrets: Arc<Mutex<Option<Join>>>,
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
            secrets: Arc::new(Mutex::new(None)),
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
        if let Some(ssid) = command.joining()
            && self.error.is_none()
        {
            self.connecting = Some(ssid.to_string());
            let started = std::time::Instant::now();
            while started.elapsed() < CONNECT_TIMEOUT {
                let state = self.read();
                let up = state.networks.iter().any(|n| n.ssid == ssid && n.connected);
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
                .any(|n| n.ssid == ssid && n.connected)
            {
                self.error = Some(format!(
                    "Couldn't connect to {ssid}: check the password and settings"
                ));
            }
        }
        if let Ok(mut secrets) = self.secrets.lock() {
            *secrets = None;
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
            (b.connected, b.in_range, b.known, b.signal).cmp(&(
                a.connected,
                a.in_range,
                a.known,
                a.signal,
            ))
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

    /// Saved Wi-Fi connections, by SSID.
    fn nm_known(&self) -> HashMap<String, Saved> {
        let mut known: HashMap<String, Saved> = HashMap::new();
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
            let Some(ssid) = ssid else {
                continue;
            };
            let autoconnect = settings
                .get("connection")
                .and_then(|c| value::<bool>(c, "autoconnect"))
                .unwrap_or(true);
            let security = settings
                .get("802-11-wireless-security")
                .map_or(Security::Open, |w| {
                    match value::<String>(w, "key-mgmt").as_deref() {
                        Some("wpa-psk") => Security::Psk,
                        Some("sae") => Security::Sae,
                        Some("owe") => Security::EnhancedOpen,
                        Some("wpa-eap" | "wpa-eap-suite-b-192") => Security::Enterprise,
                        _ => Security::Wep,
                    }
                });
            let saved = known.entry(ssid).or_insert(Saved {
                paths: Vec::new(),
                security,
                autoconnect: false,
            });
            saved.paths.push(path);
            saved.autoconnect |= autoconnect;
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
            let security = nm_security(
                value(&all, "Flags").unwrap_or(0),
                value(&all, "WpaFlags").unwrap_or(0),
                value(&all, "RsnFlags").unwrap_or(0),
            );
            let connected = active.as_ref() == Some(&point);
            // One row per name: the strongest access point.
            match networks.iter_mut().find(|n| n.ssid == ssid) {
                Some(existing) => {
                    existing.signal = existing.signal.max(signal);
                    existing.connected |= connected;
                }
                None => networks.push(Network {
                    known: known.contains_key(&ssid),
                    autoconnect: known.get(&ssid).is_some_and(|s| s.autoconnect),
                    ssid,
                    signal,
                    security,
                    connected,
                    in_range: true,
                }),
            }
        }
        for (ssid, saved) in known {
            if !networks.iter().any(|n| n.ssid == ssid) {
                networks.push(Network {
                    ssid,
                    signal: 0,
                    security: saved.security,
                    known: true,
                    connected: false,
                    in_range: false,
                    autoconnect: saved.autoconnect,
                });
            }
        }
        let details = active
            .filter(|path| path.as_str() != "/")
            .map(|point| self.nm_details(&device, &point));
        Some(State {
            has_device: true,
            enabled,
            networks,
            details,
            ..State::default()
        })
    }

    fn nm_details(&self, device: &OwnedObjectPath, point: &OwnedObjectPath) -> Details {
        let props = self.get_all(NM, device.as_str(), "org.freedesktop.NetworkManager.Device");
        let wireless = self.get_all(
            NM,
            device.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        );
        let ap = self.get_all(
            NM,
            point.as_str(),
            "org.freedesktop.NetworkManager.AccessPoint",
        );
        let interface: String = value(&props, "Interface").unwrap_or_default();
        let mut details = link_details(&interface);
        details.security = nm_security(
            value(&ap, "Flags").unwrap_or(0),
            value(&ap, "WpaFlags").unwrap_or(0),
            value(&ap, "RsnFlags").unwrap_or(0),
        )
        .label()
        .to_string();
        details.frequency = value(&ap, "Frequency");
        details.bssid = value(&ap, "HwAddress");
        details.bitrate = value::<u32>(&wireless, "Bitrate")
            .filter(|&kbit| kbit > 0)
            .map(|kbit| kbit / 1000);
        // NetworkManager knows the DNS servers even behind a local resolver.
        let mut dns = Vec::new();
        if let Some(config) = value::<OwnedObjectPath>(&props, "Ip4Config") {
            let ip4 = self.get_all(
                NM,
                config.as_str(),
                "org.freedesktop.NetworkManager.IP4Config",
            );
            let servers: Vec<HashMap<String, OwnedValue>> =
                value(&ip4, "NameserverData").unwrap_or_default();
            dns.extend(servers.iter().filter_map(|s| value::<String>(s, "address")));
        }
        if let Some(config) = value::<OwnedObjectPath>(&props, "Ip6Config") {
            let ip6 = self.get_all(
                NM,
                config.as_str(),
                "org.freedesktop.NetworkManager.IP6Config",
            );
            let servers: Vec<Vec<u8>> = value(&ip6, "Nameservers").unwrap_or_default();
            dns.extend(servers.into_iter().filter_map(|bytes| {
                let octets: [u8; 16] = bytes.try_into().ok()?;
                Some(std::net::Ipv6Addr::from(octets).to_string())
            }));
        }
        if !dns.is_empty() {
            details.dns = dns;
        }
        details
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
                let paths = self.nm_known().remove(ssid).map(|s| s.paths);
                for path in paths.unwrap_or_default() {
                    self.call::<_, ()>(
                        NM,
                        path.as_str(),
                        "org.freedesktop.NetworkManager.Settings.Connection",
                        "Delete",
                        &(),
                    )?;
                }
            }
            Command::Activate(ssid) => {
                let Some(device) = device else {
                    return Err(zbus::Error::Failure("no Wi-Fi device".into()));
                };
                let root = ObjectPath::try_from("/").map_err(zbus::Error::from)?;
                let paths = self.nm_known().remove(ssid).map(|s| s.paths);
                match paths.unwrap_or_default().first() {
                    Some(path) => {
                        self.call::<_, OwnedObjectPath>(
                            NM,
                            "/org/freedesktop/NetworkManager",
                            NM,
                            "ActivateConnection",
                            &(path, &device, &root),
                        )?;
                    }
                    // Not saved (open): NetworkManager works it out.
                    None => {
                        self.call::<_, (OwnedObjectPath, OwnedObjectPath)>(
                            NM,
                            "/org/freedesktop/NetworkManager",
                            NM,
                            "AddAndActivateConnection",
                            &(
                                nm_settings(&Join {
                                    ssid: ssid.clone(),
                                    ..Join::default()
                                }),
                                &device,
                                &root,
                            ),
                        )?;
                    }
                }
            }
            Command::Join(join) => {
                let Some(device) = device else {
                    return Err(zbus::Error::Failure("no Wi-Fi device".into()));
                };
                let root = ObjectPath::try_from("/").map_err(zbus::Error::from)?;
                // Replace what's saved, so the new details are the ones used.
                let paths = self.nm_known().remove(&join.ssid).map(|s| s.paths);
                for path in paths.unwrap_or_default() {
                    let _: zbus::Result<()> = self.call(
                        NM,
                        path.as_str(),
                        "org.freedesktop.NetworkManager.Settings.Connection",
                        "Delete",
                        &(),
                    );
                }
                self.call::<_, (OwnedObjectPath, OwnedObjectPath)>(
                    NM,
                    "/org/freedesktop/NetworkManager",
                    NM,
                    "AddAndActivateConnection",
                    &(nm_settings(join), &device, &root),
                )?;
            }
            Command::SetAutoConnect(ssid, on) => {
                let paths = self.nm_known().remove(ssid).map(|s| s.paths);
                for path in paths.unwrap_or_default() {
                    let iface = "org.freedesktop.NetworkManager.Settings.Connection";
                    let mut settings: HashMap<String, HashMap<String, OwnedValue>> =
                        self.call(NM, path.as_str(), iface, "GetSettings", &())?;
                    settings
                        .entry("connection".into())
                        .or_default()
                        .insert("autoconnect".into(), OwnedValue::from(*on));
                    // Secrets left out of an update are kept.
                    self.call::<_, ()>(NM, path.as_str(), iface, "Update", &(settings,))?;
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
                let known = value::<OwnedObjectPath>(props, "KnownNetwork")
                    .and_then(|path| objects.get(&path)?.get("net.connman.iwd.KnownNetwork"));
                networks.push(Network {
                    ssid,
                    signal: dbm_to_percent(rssi as f64 / 100.0),
                    security: iwd_security(&kind),
                    known: known.is_some(),
                    connected: value::<bool>(props, "Connected").unwrap_or(false),
                    in_range: true,
                    autoconnect: known
                        .is_some_and(|k| value::<bool>(k, "AutoConnect").unwrap_or(true)),
                });
            }
        }
        for props in objects
            .values()
            .filter_map(|i| i.get("net.connman.iwd.KnownNetwork"))
        {
            let ssid: String = value(props, "Name").unwrap_or_default();
            if ssid.is_empty() || networks.iter().any(|n| n.ssid == ssid) {
                continue;
            }
            let kind: String = value(props, "Type").unwrap_or_default();
            networks.push(Network {
                ssid,
                signal: 0,
                security: iwd_security(&kind),
                known: true,
                connected: false,
                in_range: false,
                autoconnect: value::<bool>(props, "AutoConnect").unwrap_or(true),
            });
        }
        let details = networks
            .iter()
            .any(|n| n.connected)
            .then(|| self.iwd_details(device, device_props));
        Some(State {
            has_device: true,
            enabled,
            networks,
            details,
            ..State::default()
        })
    }

    fn iwd_details(
        &self,
        device: &OwnedObjectPath,
        props: &HashMap<String, OwnedValue>,
    ) -> Details {
        let interface: String = value(props, "Name").unwrap_or_default();
        let mut details = link_details(&interface);
        let diagnostics: HashMap<String, OwnedValue> = self
            .call(
                IWD,
                device.as_str(),
                "net.connman.iwd.StationDiagnostic",
                "GetDiagnostics",
                &(),
            )
            .unwrap_or_default();
        details.security = value::<String>(&diagnostics, "Security")
            .unwrap_or_default()
            .replace('-', " ");
        details.frequency = value(&diagnostics, "Frequency");
        details.dbm = value::<i16>(&diagnostics, "RSSI").map(i32::from);
        details.bssid = value(&diagnostics, "ConnectedBss");
        // In 100 kbit/s.
        details.bitrate = value::<u32>(&diagnostics, "TxBitrate").map(|b| b / 10);
        details
    }

    /// The saved network called `ssid`, in range or not.
    fn iwd_known(&self, ssid: &str) -> Option<OwnedObjectPath> {
        self.iwd_objects().into_iter().find_map(|(path, ifaces)| {
            let props = ifaces.get("net.connman.iwd.KnownNetwork")?;
            (value::<String>(props, "Name").as_deref() == Some(ssid)).then_some(path)
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
                if let Some(known) = self.iwd_known(ssid) {
                    self.call::<_, ()>(
                        IWD,
                        known.as_str(),
                        "net.connman.iwd.KnownNetwork",
                        "Forget",
                        &(),
                    )?;
                }
            }
            Command::Activate(ssid) => {
                self.iwd_agent()?;
                let Some((path, _)) = self.iwd_network(ssid) else {
                    return Err(zbus::Error::Failure(format!("{ssid} is out of range")));
                };
                self.call::<_, ()>(
                    IWD,
                    path.as_str(),
                    "net.connman.iwd.Network",
                    "Connect",
                    &(),
                )?;
            }
            Command::SetAutoConnect(ssid, on) => {
                if let Some(known) = self.iwd_known(ssid) {
                    self.props(IWD, known.as_str())?.set(
                        InterfaceName::try_from("net.connman.iwd.KnownNetwork")
                            .map_err(zbus::Error::from)?,
                        "AutoConnect",
                        Value::from(*on),
                    )?;
                }
            }
            Command::Join(join) => {
                if join.security == Security::Wep {
                    return Err(zbus::Error::Failure("iwd can't join WEP networks".into()));
                }
                self.iwd_agent()?;
                if let Ok(mut slot) = self.secrets.lock() {
                    *slot = Some(join.clone());
                }
                // A saved one would answer with its old password.
                if let Some((_, props)) = self.iwd_network(&join.ssid)
                    && let Some(known) = value::<OwnedObjectPath>(&props, "KnownNetwork")
                {
                    let _: zbus::Result<()> = self.call(
                        IWD,
                        known.as_str(),
                        "net.connman.iwd.KnownNetwork",
                        "Forget",
                        &(),
                    );
                }
                // These block until connected (or failed); secrets come from
                // the agent.
                if join.hidden {
                    let Some(device) = device else {
                        return Err(zbus::Error::Failure("no Wi-Fi device".into()));
                    };
                    self.call::<_, ()>(
                        IWD,
                        device.as_str(),
                        "net.connman.iwd.Station",
                        "ConnectHiddenNetwork",
                        &(join.ssid.as_str(),),
                    )?;
                } else {
                    let Some((path, _)) = self.iwd_network(&join.ssid) else {
                        return Err(zbus::Error::Failure(format!(
                            "{} is out of range",
                            join.ssid
                        )));
                    };
                    self.call::<_, ()>(
                        IWD,
                        path.as_str(),
                        "net.connman.iwd.Network",
                        "Connect",
                        &(),
                    )?;
                }
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
            secrets: self.secrets.clone(),
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
    secrets: Arc<Mutex<Option<Join>>>,
}

impl Agent {
    fn secret<T>(&self, pick: impl FnOnce(&Join) -> Option<T>) -> zbus::fdo::Result<T> {
        self.secrets
            .lock()
            .ok()
            .and_then(|join| pick(join.as_ref()?))
            .ok_or_else(|| zbus::fdo::Error::Failed("canceled".into()))
    }
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl Agent {
    fn release(&self) {}

    fn request_passphrase(&self, _network: ObjectPath<'_>) -> zbus::fdo::Result<String> {
        self.secret(|join| (!join.password.is_empty()).then(|| join.password.clone()))
    }

    /// Enterprise: the password of a provisioned network's user.
    fn request_user_password(
        &self,
        _network: ObjectPath<'_>,
        _user: String,
    ) -> zbus::fdo::Result<String> {
        self.secret(|join| (!join.password.is_empty()).then(|| join.password.clone()))
    }

    fn request_user_name_and_password(
        &self,
        _network: ObjectPath<'_>,
    ) -> zbus::fdo::Result<(String, String)> {
        self.secret(|join| {
            (!join.identity.is_empty()).then(|| (join.identity.clone(), join.password.clone()))
        })
    }

    fn request_private_key_passphrase(
        &self,
        _network: ObjectPath<'_>,
    ) -> zbus::fdo::Result<String> {
        self.secret(|join| Some(join.password.clone()))
    }

    fn cancel(&self, _reason: String) {}
}

/// NetworkManager's connection settings for joining.
fn nm_settings(join: &Join) -> HashMap<&'static str, HashMap<&'static str, Value<'static>>> {
    let text = |s: &str| Value::from(s.trim().to_string());
    // Certificate and key settings are paths as "file://" URIs, as bytes
    // ending in a NUL.
    let file = |path: &str| {
        let mut bytes = format!("file://{}", path.trim()).into_bytes();
        bytes.push(0);
        Value::from(bytes)
    };
    let mut settings = HashMap::new();
    let mut wireless = HashMap::from([("ssid", Value::from(join.ssid.as_bytes().to_vec()))]);
    if join.hidden {
        wireless.insert("hidden", Value::from(true));
    }
    settings.insert("802-11-wireless", wireless);
    let key_mgmt = match join.security {
        Security::Open => return settings,
        Security::EnhancedOpen => "owe",
        Security::Wep => "none",
        Security::Psk | Security::PskSae => "wpa-psk",
        Security::Sae => "sae",
        Security::Enterprise => "wpa-eap",
    };
    let mut security = HashMap::from([("key-mgmt", Value::from(key_mgmt))]);
    match join.security {
        Security::Psk | Security::Sae | Security::PskSae => {
            security.insert("psk", Value::from(join.password.clone()));
        }
        Security::Wep => {
            // A key is 5 or 13 characters, or 10 or 26 hex digits; anything
            // else is a passphrase to hash.
            let hex = join.password.chars().all(|c| c.is_ascii_hexdigit());
            let key = match join.password.len() {
                5 | 13 => true,
                10 | 26 => hex,
                _ => false,
            };
            security.insert("wep-key0", Value::from(join.password.clone()));
            security.insert("wep-key-type", Value::from(if key { 1u32 } else { 2 }));
        }
        _ => {}
    }
    settings.insert("802-11-wireless-security", security);
    if join.security == Security::Enterprise {
        let mut x = HashMap::from([
            ("eap", Value::from(vec![join.eap.name().to_string()])),
            ("identity", text(&join.identity)),
        ]);
        if !join.anonymous_identity.trim().is_empty() {
            x.insert("anonymous-identity", text(&join.anonymous_identity));
        }
        if join.eap.tunnelled() {
            x.insert("phase2-auth", Value::from(join.phase2.name()));
        }
        if join.eap == Eap::Tls {
            x.insert("client-cert", file(&join.client_cert));
            x.insert("private-key", file(&join.private_key));
            x.insert("private-key-password", Value::from(join.password.clone()));
        } else {
            x.insert("password", Value::from(join.password.clone()));
        }
        if !join.ca_cert.trim().is_empty() {
            x.insert("ca-cert", file(&join.ca_cert));
        } else if !join.domain.trim().is_empty() {
            x.insert("system-ca-certs", Value::from(true));
        }
        if !join.domain.trim().is_empty() {
            x.insert("domain-suffix-match", text(&join.domain));
        }
        settings.insert("802-1x", x);
    }
    settings
}

/// From an access point's flags (NM_802_11_AP_FLAGS / _SEC).
fn nm_security(flags: u32, wpa: u32, rsn: u32) -> Security {
    const PSK: u32 = 0x100;
    const EAP: u32 = 0x200;
    const SAE: u32 = 0x400;
    const OWE: u32 = 0x800 | 0x1000;
    const EAP_SUITE_B: u32 = 0x2000;
    let keys = wpa | rsn;
    if keys & (EAP | EAP_SUITE_B) != 0 {
        Security::Enterprise
    } else if keys & PSK != 0 && keys & SAE != 0 {
        Security::PskSae
    } else if keys & SAE != 0 {
        Security::Sae
    } else if keys & PSK != 0 {
        Security::Psk
    } else if keys & OWE != 0 {
        Security::EnhancedOpen
    } else if flags & 1 != 0 {
        Security::Wep
    } else {
        Security::Open
    }
}

fn iwd_security(kind: &str) -> Security {
    match kind {
        "open" => Security::Open,
        "wep" => Security::Wep,
        // iwd picks WPA2 or WPA3 itself.
        "psk" => Security::PskSae,
        "8021x" => Security::Enterprise,
        _ => Security::Psk,
    }
}

/// Addresses, the gateway, DNS servers and the hardware address of
/// `interface`, from the kernel (`ip`, /sys and resolv.conf), whichever
/// backend manages it.
fn link_details(interface: &str) -> Details {
    let mut details = Details {
        interface: interface.to_string(),
        ..Details::default()
    };
    if interface.is_empty() || interface.contains(|c: char| c.is_whitespace() || c == '/') {
        return details;
    }
    let run = |args: &[&str]| -> String {
        std::process::Command::new("ip")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let (ipv4, ipv6) = parse_addresses(&run(&["addr", "show", "dev", interface]));
    details.ipv4 = ipv4;
    details.ipv6 = ipv6;
    details.gateway = parse_gateway(&run(&["route", "show", "default"]), interface)
        .or_else(|| parse_gateway(&run(&["-6", "route", "show", "default"]), interface));
    details.mac = std::fs::read_to_string(format!("/sys/class/net/{interface}/address"))
        .ok()
        .map(|mac| mac.trim().to_string())
        .filter(|mac| !mac.is_empty());
    details.dns = std::fs::read_to_string("/etc/resolv.conf")
        .map(|text| parse_nameservers(&text))
        .unwrap_or_default();
    details
}

/// `ip addr show` output: IPv4 and (not link-local) IPv6 addresses, with
/// their prefix lengths.
fn parse_addresses(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut v4, mut v6) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("inet"), Some(address)) => v4.push(address.to_string()),
            (Some("inet6"), Some(address)) if !address.starts_with("fe80:") => {
                v6.push(address.to_string())
            }
            _ => {}
        }
    }
    (v4, v6)
}

/// `ip route show default` output: the router `interface` goes through.
fn parse_gateway(text: &str, interface: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        let on_interface = words.windows(2).any(|w| w == ["dev", interface]);
        let via = words.windows(2).find(|w| w[0] == "via").map(|w| w[1]);
        on_interface.then_some(via).flatten().map(str::to_string)
    })
}

fn parse_nameservers(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .map(|rest| rest.trim().to_string())
        .filter(|server| !server.is_empty())
        .collect()
}

/// "5 GHz · channel 36" from a frequency in MHz.
pub fn band(frequency: u32) -> String {
    let (band, channel) = match frequency {
        2484 => ("2.4 GHz", 14),
        2412..=2472 => ("2.4 GHz", (frequency - 2407) / 5),
        5150..=5895 => ("5 GHz", (frequency - 5000) / 5),
        5955..=7115 => ("6 GHz", (frequency - 5950) / 5),
        _ => return format!("{frequency} MHz"),
    };
    format!("{band} · channel {channel}")
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
        Command::Activate(ssid) => format!("Couldn't connect to {ssid}"),
        Command::Join(join) => format!("Couldn't connect to {}", join.ssid),
        Command::Disconnect => "Couldn't disconnect".to_string(),
        Command::Forget(ssid) => format!("Couldn't forget {ssid}"),
        Command::SetAutoConnect(ssid, _) => format!("Couldn't change {ssid}"),
    };
    if denied {
        return format!("{what}: not allowed (polkit)");
    }
    warn!(%text, "wifi");
    if text.contains("NotConfigured") {
        // iwd joins enterprise networks only once they're set up.
        return format!(
            "{what}: iwd needs enterprise networks set up as root first \
             (a file in /var/lib/iwd, see iwd.network(5))"
        );
    }
    // Our own messages are already in words.
    match err {
        zbus::Error::Failure(reason) => format!("{what}: {reason}"),
        _ => what,
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
                eprintln!("details={:?}", state.details);
                for n in &state.networks {
                    eprintln!("  {:?} {}", n.security, n.signal);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_point_flags_name_the_security() {
        assert_eq!(nm_security(0, 0, 0), Security::Open);
        assert_eq!(nm_security(1, 0, 0), Security::Wep);
        assert_eq!(nm_security(1, 0, 0x188), Security::Psk);
        assert_eq!(nm_security(1, 0, 0x588), Security::PskSae);
        assert_eq!(nm_security(1, 0, 0x488), Security::Sae);
        assert_eq!(nm_security(1, 0x288, 0x288), Security::Enterprise);
        assert_eq!(nm_security(0, 0, 0x888), Security::EnhancedOpen);
    }

    fn get<'a>(
        settings: &'a HashMap<&str, HashMap<&str, Value<'static>>>,
        section: &str,
        key: &str,
    ) -> Option<&'a Value<'static>> {
        settings.get(section)?.get(key)
    }

    #[test]
    fn settings_for_each_kind_of_network() {
        let open = nm_settings(&Join {
            ssid: "Cafe".into(),
            ..Join::default()
        });
        assert!(!open.contains_key("802-11-wireless-security"));

        let wpa3 = nm_settings(&Join {
            ssid: "Home".into(),
            security: Security::Sae,
            password: "correct horse".into(),
            hidden: true,
            ..Join::default()
        });
        assert_eq!(
            get(&wpa3, "802-11-wireless-security", "key-mgmt"),
            Some(&Value::from("sae"))
        );
        assert_eq!(
            get(&wpa3, "802-11-wireless", "hidden"),
            Some(&Value::from(true))
        );

        let wep_key = nm_settings(&Join {
            ssid: "Old".into(),
            security: Security::Wep,
            password: "0123456789".into(),
            ..Join::default()
        });
        assert_eq!(
            get(&wep_key, "802-11-wireless-security", "wep-key-type"),
            Some(&Value::from(1u32))
        );
        let wep_phrase = nm_settings(&Join {
            ssid: "Old".into(),
            security: Security::Wep,
            password: "a passphrase".into(),
            ..Join::default()
        });
        assert_eq!(
            get(&wep_phrase, "802-11-wireless-security", "wep-key-type"),
            Some(&Value::from(2u32))
        );

        let eduroam = nm_settings(&Join {
            ssid: "eduroam".into(),
            security: Security::Enterprise,
            eap: Eap::Peap,
            phase2: Phase2::Mschapv2,
            identity: " me@uni.edu ".into(),
            password: "secret".into(),
            domain: "radius.uni.edu".into(),
            ..Join::default()
        });
        assert_eq!(
            get(&eduroam, "802-11-wireless-security", "key-mgmt"),
            Some(&Value::from("wpa-eap"))
        );
        assert_eq!(
            get(&eduroam, "802-1x", "identity"),
            Some(&Value::from("me@uni.edu"))
        );
        assert_eq!(
            get(&eduroam, "802-1x", "phase2-auth"),
            Some(&Value::from("mschapv2"))
        );
        assert_eq!(
            get(&eduroam, "802-1x", "system-ca-certs"),
            Some(&Value::from(true))
        );

        let tls = nm_settings(&Join {
            ssid: "Corp".into(),
            security: Security::Enterprise,
            eap: Eap::Tls,
            identity: "me".into(),
            client_cert: "/home/me/me.pem".into(),
            private_key: "/home/me/me.key".into(),
            ca_cert: "/home/me/ca.pem".into(),
            ..Join::default()
        });
        assert_eq!(
            get(&tls, "802-1x", "ca-cert"),
            Some(&Value::from(b"file:///home/me/ca.pem\0".to_vec()))
        );
        assert!(get(&tls, "802-1x", "phase2-auth").is_none());
    }

    #[test]
    fn joining_says_what_is_missing() {
        let mut join = Join {
            ssid: "Home".into(),
            security: Security::Psk,
            password: "short".into(),
            ..Join::default()
        };
        assert!(join.missing().is_some());
        join.password = "long enough".into();
        assert_eq!(join.missing(), None);
        join.security = Security::Enterprise;
        assert_eq!(join.missing(), Some("Enter your username"));
    }

    #[test]
    fn kernel_details_parse() {
        let addr = "3: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500
    link/ether 3c:a9:f4:00:11:22 brd ff:ff:ff:ff:ff:ff
    inet 10.0.0.160/24 brd 10.0.0.255 scope global dynamic wlan0
    inet6 2001:db8::5/64 scope global dynamic
    inet6 fe80::1/64 scope link";
        assert_eq!(
            parse_addresses(addr),
            (vec!["10.0.0.160/24".into()], vec!["2001:db8::5/64".into()])
        );
        let routes = "default via 192.168.1.1 dev eth0 metric 100
default via 10.0.0.1 dev wlan0 proto dhcp metric 600";
        assert_eq!(parse_gateway(routes, "wlan0"), Some("10.0.0.1".into()));
        assert_eq!(parse_gateway(routes, "wlan1"), None);
        assert_eq!(
            parse_nameservers("# hi\nnameserver 1.1.1.1\nnameserver  9.9.9.9\nsearch lan"),
            vec!["1.1.1.1".to_string(), "9.9.9.9".to_string()]
        );
    }

    #[test]
    fn bands_and_channels() {
        assert_eq!(band(2412), "2.4 GHz · channel 1");
        assert_eq!(band(5180), "5 GHz · channel 36");
        assert_eq!(band(5955), "6 GHz · channel 1");
    }
}
