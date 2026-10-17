//! Bluetooth for the Settings page, through BlueZ on the system bus. Like
//! Wi-Fi's, a worker thread makes the D-Bus calls: the page sends it
//! commands, and it sends back the whole picture after each one and every
//! few seconds while the window is open. Pairing and connecting can take
//! many seconds, so those run on a thread of their own and the list keeps
//! updating meanwhile.
//!
//! Pairing registers an agent that can show a code but not type or confirm
//! one ("DisplayOnly"): headphones, mice and speakers pair without asking
//! ("just works"), and keyboards show the code to type on them. Devices
//! that want a code typed in here aren't supported.

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
pub enum Availability {
    /// Still looking.
    #[default]
    Unknown,
    /// BlueZ isn't running (or there's no system bus).
    NoService,
    NoAdapter,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// What commands name it by.
    pub address: String,
    pub name: String,
    /// BlueZ's freedesktop icon name ("audio-headphones"), or empty.
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    /// Percent, if it reports one.
    pub battery: Option<u8>,
    /// Signal strength in dBm, while it's being heard.
    pub rssi: Option<i16>,
}

impl Device {
    /// What kind of thing it is, in words ("" if BlueZ doesn't know).
    pub fn kind(&self) -> &'static str {
        kind(&self.icon)
    }
}

/// What's being done to a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Pairing,
    Connecting,
    Disconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub availability: Availability,
    pub powered: bool,
    /// Looking for nearby devices.
    pub discovering: bool,
    /// Paired ones (connected first, then by name), then nearby ones
    /// (strongest first).
    pub devices: Vec<Device>,
    /// The device being paired or (dis)connected: its address.
    pub busy: Option<(String, Busy)>,
    /// A code to type on the device being paired.
    pub code: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Command {
    SetPowered(bool),
    /// Look for nearby devices (while the page shows), or stop.
    Discover(bool),
    Pair(String),
    Connect(String),
    Disconnect(String),
    /// Unpair and remove.
    Forget(String),
}

/// How often the list is re-read while nothing happens.
const POLL: Duration = Duration::from_secs(2);
/// How often while pairing or connecting (for the code and the outcome).
const POLL_BUSY: Duration = Duration::from_millis(300);

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";

/// Everything BlueZ manages: path -> interface -> properties.
type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// Start the worker; state arrives on `tx`. Dropping the returned sender
/// stops it (BlueZ ends our discovery when we leave the bus).
pub fn start(tx: Sender<State>) -> mpsc::Sender<Command> {
    let (commands, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("bluetooth".into())
        .spawn(move || {
            let conn = match Connection::system() {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(%err, "no system bus; no Bluetooth settings");
                    let _ = tx.send(State {
                        availability: Availability::NoService,
                        ..State::default()
                    });
                    return;
                }
            };
            Worker::new(conn, tx).run(rx);
        });
    commands
}

/// What the agent knows about the pairing in progress.
#[derive(Default)]
struct Pairing {
    device: Option<OwnedObjectPath>,
    code: Option<String>,
}

struct Worker {
    conn: Connection,
    tx: Sender<State>,
    /// Discovery is wanted (the page is showing).
    discover: bool,
    /// We started discovery and haven't stopped it.
    discovering: bool,
    busy: Option<(String, Busy)>,
    /// What the pairing or connecting thread came back with.
    done: Arc<Mutex<Option<Result<(), String>>>>,
    pairing: Arc<Mutex<Pairing>>,
    agent_registered: bool,
    error: Option<String>,
}

impl Worker {
    fn new(conn: Connection, tx: Sender<State>) -> Self {
        Self {
            conn,
            tx,
            discover: false,
            discovering: false,
            busy: None,
            done: Arc::new(Mutex::new(None)),
            pairing: Arc::new(Mutex::new(Pairing::default())),
            agent_registered: false,
            error: None,
        }
    }

    fn run(&mut self, rx: mpsc::Receiver<Command>) {
        self.send();
        loop {
            let wait = if self.busy.is_some() { POLL_BUSY } else { POLL };
            match rx.recv_timeout(wait) {
                Ok(command) => self.handle(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.finish();
                    self.keep_discovering();
                    self.send();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if self.discovering {
                        let _: zbus::Result<()> = self.adapter_call("StopDiscovery");
                    }
                    return;
                }
            }
        }
    }

    fn handle(&mut self, command: Command) {
        debug!(?command, "bluetooth");
        self.error = None;
        if let Err(err) = self.command(&command) {
            let named = match &command {
                Command::Pair(a) => Command::Pair(self.name_of(a)),
                Command::Connect(a) => Command::Connect(self.name_of(a)),
                Command::Disconnect(a) => Command::Disconnect(self.name_of(a)),
                Command::Forget(a) => Command::Forget(self.name_of(a)),
                other => other.clone(),
            };
            self.error = Some(describe(&named, &err.to_string()));
        }
        self.keep_discovering();
        self.send();
    }

    fn command(&mut self, command: &Command) -> zbus::Result<()> {
        match command {
            Command::SetPowered(on) => {
                let adapter = self.adapter().ok_or_else(no_adapter)?;
                if !on {
                    // Powering off ends it; start afresh when it's back.
                    self.discovering = false;
                }
                self.props(adapter.as_str())?.set(
                    InterfaceName::try_from(ADAPTER).map_err(zbus::Error::from)?,
                    "Powered",
                    Value::from(*on),
                )?;
            }
            Command::Discover(on) => {
                self.discover = *on;
                if !on && self.discovering {
                    self.discovering = false;
                    // Fails if BlueZ already stopped it; that's fine.
                    let _: zbus::Result<()> = self.adapter_call("StopDiscovery");
                }
            }
            Command::Forget(address) => {
                let adapter = self.adapter().ok_or_else(no_adapter)?;
                let device = self.device(address).ok_or_else(gone)?;
                self.call::<_, ()>(adapter.as_str(), ADAPTER, "RemoveDevice", &(&device,))?;
            }
            Command::Pair(address) | Command::Connect(address) | Command::Disconnect(address) => {
                // One at a time.
                if self.busy.is_some() {
                    return Ok(());
                }
                let device = self.device(address).ok_or_else(gone)?;
                let busy = match command {
                    Command::Pair(_) => {
                        self.register_agent()?;
                        if let Ok(mut pairing) = self.pairing.lock() {
                            *pairing = Pairing {
                                device: Some(device.clone()),
                                code: None,
                            };
                        }
                        Busy::Pairing
                    }
                    Command::Connect(_) => Busy::Connecting,
                    _ => Busy::Disconnecting,
                };
                self.busy = Some((address.clone(), busy));
                let conn = self.conn.clone();
                let done = self.done.clone();
                let spawned = std::thread::Builder::new()
                    .name("bluetooth-device".into())
                    .spawn(move || {
                        let result = work(&conn, &device, busy).map_err(|err| err.to_string());
                        if let Ok(mut done) = done.lock() {
                            *done = Some(result);
                        }
                    });
                if let Err(err) = spawned {
                    self.busy = None;
                    return Err(zbus::Error::Failure(err.to_string()));
                }
            }
        }
        Ok(())
    }

    /// The pairing or connecting thread is done: say how it went.
    fn finish(&mut self) {
        let Some(result) = self.done.lock().ok().and_then(|mut done| done.take()) else {
            return;
        };
        let Some((address, busy)) = self.busy.take() else {
            return;
        };
        if let Ok(mut pairing) = self.pairing.lock() {
            *pairing = Pairing::default();
        }
        if let Err(err) = result {
            let name = self.name_of(&address);
            let command = match busy {
                Busy::Pairing => Command::Pair(name),
                Busy::Connecting => Command::Connect(name),
                Busy::Disconnecting => Command::Disconnect(name),
            };
            self.error = Some(describe(&command, &err));
        }
    }

    /// What the device with `address` is called, for messages.
    fn name_of(&self, address: &str) -> String {
        self.read()
            .devices
            .into_iter()
            .find(|d| d.address == address)
            .map_or_else(|| address.to_string(), |d| d.name)
    }

    /// Discovery runs while it's wanted and the adapter is on (and starts
    /// again once it's switched back on).
    fn keep_discovering(&mut self) {
        let powered = self
            .adapter()
            .map(|a| self.get_all(a.as_str(), ADAPTER))
            .and_then(|all| value::<bool>(&all, "Powered"))
            .unwrap_or(false);
        if !powered {
            self.discovering = false;
            return;
        }
        if self.discover && !self.discovering {
            match self.adapter_call::<()>("StartDiscovery") {
                Ok(()) => self.discovering = true,
                // Ours is already running.
                Err(err) if err.to_string().contains("InProgress") => self.discovering = true,
                Err(err) => debug!(%err, "can't look for Bluetooth devices"),
            }
        }
    }

    fn send(&mut self) {
        self.finish();
        let _ = self.tx.send(self.read());
    }

    fn read(&self) -> State {
        let objects: zbus::Result<Objects> = self.call(
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "GetManagedObjects",
            &(),
        );
        let mut state = match objects {
            Ok(objects) => parse(&objects),
            Err(err) => {
                debug!(%err, "no BlueZ");
                State {
                    availability: Availability::NoService,
                    ..State::default()
                }
            }
        };
        state.busy = self.busy.clone();
        state.code = self.pairing.lock().ok().and_then(|p| p.code.clone());
        state.error = self.error.clone();
        state
    }

    /// The first adapter.
    fn adapter(&self) -> Option<OwnedObjectPath> {
        let objects: Objects = self
            .call(
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "GetManagedObjects",
                &(),
            )
            .ok()?;
        first_adapter(&objects).map(|(path, _)| path.clone())
    }

    /// The adapter's device with this address.
    fn device(&self, address: &str) -> Option<OwnedObjectPath> {
        let objects: Objects = self
            .call(
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "GetManagedObjects",
                &(),
            )
            .ok()?;
        let (adapter, _) = first_adapter(&objects)?;
        objects.iter().find_map(|(path, ifaces)| {
            let props = ifaces.get(DEVICE)?;
            (value::<String>(props, "Address").as_deref() == Some(address)
                && value::<OwnedObjectPath>(props, "Adapter").as_ref() == Some(adapter))
            .then(|| path.clone())
        })
    }

    fn props(&self, path: &str) -> zbus::Result<PropertiesProxy<'_>> {
        PropertiesProxy::builder(&self.conn)
            .destination(BLUEZ)?
            .path(path.to_string())?
            .build()
    }

    fn get_all(&self, path: &str, interface: &str) -> HashMap<String, OwnedValue> {
        self.props(path)
            .ok()
            .and_then(|p| p.get_all(InterfaceName::try_from(interface).ok()?).ok())
            .unwrap_or_default()
    }

    fn call<B, R>(&self, path: &str, interface: &str, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = self
            .conn
            .call_method(Some(BLUEZ), path, Some(interface), method, body)?;
        reply.body().deserialize::<R>()
    }

    fn adapter_call<R>(&self, method: &str) -> zbus::Result<R>
    where
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let adapter = self.adapter().ok_or_else(no_adapter)?;
        self.call(adapter.as_str(), ADAPTER, method, &())
    }

    /// BlueZ asks the agent of whoever called Pair about codes: ours.
    fn register_agent(&mut self) -> zbus::Result<()> {
        if self.agent_registered {
            return Ok(());
        }
        let agent = Agent {
            pairing: self.pairing.clone(),
        };
        self.conn.object_server().at(AGENT_PATH, agent)?;
        let path = ObjectPath::try_from(AGENT_PATH).map_err(zbus::Error::from)?;
        self.call::<_, ()>(
            "/org/bluez",
            "org.bluez.AgentManager1",
            "RegisterAgent",
            &(path, "DisplayOnly"),
        )?;
        self.agent_registered = true;
        Ok(())
    }
}

fn no_adapter() -> zbus::Error {
    zbus::Error::Failure("no Bluetooth adapter".into())
}

fn gone() -> zbus::Error {
    zbus::Error::Failure("it's gone out of range".into())
}

/// Pair (then trust and connect, as other desktops do), connect or
/// disconnect. Blocks until BlueZ says how it went.
fn work(conn: &Connection, device: &OwnedObjectPath, busy: Busy) -> zbus::Result<()> {
    let call = |method: &str| -> zbus::Result<()> {
        conn.call_method(Some(BLUEZ), device.as_str(), Some(DEVICE), method, &())
            .map(|_| ())
    };
    match busy {
        Busy::Pairing => {
            call("Pair")?;
            // Trusted: it may reconnect by itself later without asking.
            let trusted = PropertiesProxy::builder(conn)
                .destination(BLUEZ)?
                .path(device.as_str())?
                .build()?
                .set(
                    InterfaceName::try_from(DEVICE).map_err(zbus::Error::from)?,
                    "Trusted",
                    Value::from(true),
                );
            if let Err(err) = trusted {
                debug!(%err, "can't trust the paired device");
            }
            // Keyboards and mice connect by themselves once paired.
            match call("Connect") {
                Err(err) if err.to_string().contains("AlreadyConnected") => Ok(()),
                other => other,
            }
        }
        Busy::Connecting => call("Connect"),
        Busy::Disconnecting => call("Disconnect"),
    }
}

/// The first adapter, by path (hci0 before hci1).
fn first_adapter(objects: &Objects) -> Option<(&OwnedObjectPath, &HashMap<String, OwnedValue>)> {
    objects
        .iter()
        .filter_map(|(path, ifaces)| Some((path, ifaces.get(ADAPTER)?)))
        .min_by(|a, b| a.0.as_str().cmp(b.0.as_str()))
}

/// The state from BlueZ's objects (without the worker's own parts).
fn parse(objects: &Objects) -> State {
    let Some((adapter, props)) = first_adapter(objects) else {
        return State {
            availability: Availability::NoAdapter,
            ..State::default()
        };
    };
    let mut devices: Vec<Device> = objects
        .values()
        .filter_map(|ifaces| {
            let props = ifaces.get(DEVICE)?;
            if value::<OwnedObjectPath>(props, "Adapter").as_ref() != Some(adapter) {
                return None;
            }
            let paired = value::<bool>(props, "Paired").unwrap_or(false)
                || value::<bool>(props, "Bonded").unwrap_or(false);
            // Nearby things that don't say what they're called are mostly
            // beacons and other people's gadgets.
            if !paired && value::<String>(props, "Name").is_none_or(|n| n.trim().is_empty()) {
                return None;
            }
            let address: String = value(props, "Address")?;
            Some(Device {
                name: value::<String>(props, "Alias")
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| address.clone()),
                address,
                icon: value(props, "Icon").unwrap_or_default(),
                paired,
                connected: value::<bool>(props, "Connected").unwrap_or(false),
                battery: ifaces
                    .get("org.bluez.Battery1")
                    .and_then(|b| value::<u8>(b, "Percentage")),
                rssi: value(props, "RSSI"),
            })
        })
        .collect();
    devices.sort_by(|a, b| {
        (b.paired, b.connected)
            .cmp(&(a.paired, a.connected))
            .then_with(|| {
                if a.paired {
                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                } else {
                    b.rssi.cmp(&a.rssi)
                }
            })
    });
    State {
        availability: Availability::Ready,
        powered: value(props, "Powered").unwrap_or(false),
        discovering: value(props, "Discovering").unwrap_or(false),
        devices,
        ..State::default()
    }
}

/// "Headphones" from BlueZ's icon name.
fn kind(icon: &str) -> &'static str {
    match icon {
        "audio-headset" => "Headset",
        "audio-headphones" => "Headphones",
        "audio-card" => "Speaker",
        "input-keyboard" => "Keyboard",
        "input-mouse" => "Mouse",
        "input-gaming" => "Game controller",
        "input-tablet" => "Graphics tablet",
        "phone" => "Phone",
        "computer" => "Computer",
        "video-display" => "Display",
        "printer" => "Printer",
        "multimedia-player" => "Media player",
        "network-wireless" => "Network device",
        icon if icon.starts_with("camera") => "Camera",
        _ => "",
    }
}

const AGENT_PATH: &str = "/org/cnde/settings/bluetooth_agent";

/// Answers BlueZ's questions while we pair: shows codes, agrees to the
/// pairing we asked for, refuses anything else.
struct Agent {
    pairing: Arc<Mutex<Pairing>>,
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum Refusal {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

impl Agent {
    /// Whether `device` is the one we're pairing.
    fn ours(&self, device: &ObjectPath<'_>) -> bool {
        self.pairing
            .lock()
            .ok()
            .and_then(|p| p.device.clone())
            .is_some_and(|d| d.as_str() == device.as_str())
    }

    fn show(&self, device: &ObjectPath<'_>, code: String) -> Result<(), Refusal> {
        if !self.ours(device) {
            return Err(Refusal::Rejected("not pairing it".into()));
        }
        if let Ok(mut pairing) = self.pairing.lock() {
            pairing.code = Some(code);
        }
        Ok(())
    }

    fn agree(&self, device: &ObjectPath<'_>) -> Result<(), Refusal> {
        if self.ours(device) {
            Ok(())
        } else {
            Err(Refusal::Rejected("not pairing it".into()))
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    /// Old (pre-2.1) devices: most headsets take 0000, and a keyboard is
    /// asked to type it.
    fn request_pin_code(&self, device: ObjectPath<'_>) -> Result<String, Refusal> {
        self.show(&device, "0000".into())?;
        Ok("0000".into())
    }

    fn display_pin_code(&self, device: ObjectPath<'_>, pincode: String) -> Result<(), Refusal> {
        self.show(&device, pincode)
    }

    /// Typing a code shown on the device: not supported.
    fn request_passkey(&self, _device: ObjectPath<'_>) -> Result<u32, Refusal> {
        Err(Refusal::Rejected("can't type a passkey".into()))
    }

    fn display_passkey(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
        _entered: u16,
    ) -> Result<(), Refusal> {
        self.show(&device, format!("{passkey:06}"))
    }

    /// Only asked of agents that can confirm; agree to the pairing we
    /// started (the user just clicked Pair).
    fn request_confirmation(&self, device: ObjectPath<'_>, _passkey: u32) -> Result<(), Refusal> {
        self.agree(&device)
    }

    fn request_authorization(&self, device: ObjectPath<'_>) -> Result<(), Refusal> {
        self.agree(&device)
    }

    fn authorize_service(&self, device: ObjectPath<'_>, _uuid: String) -> Result<(), Refusal> {
        self.agree(&device)
    }

    fn cancel(&self) {
        if let Ok(mut pairing) = self.pairing.lock() {
            pairing.code = None;
        }
    }
}

fn value<T: TryFrom<OwnedValue>>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    T::try_from(map.get(key)?.try_clone().ok()?).ok()
}

/// What went wrong, in words for the page. `command` names the device.
fn describe(command: &Command, err: &str) -> String {
    let what = match command {
        Command::SetPowered(true) => "Couldn't turn Bluetooth on".to_string(),
        Command::SetPowered(false) => "Couldn't turn Bluetooth off".to_string(),
        Command::Discover(_) => "Couldn't look for devices".to_string(),
        Command::Pair(name) => format!("Couldn't pair with {name}"),
        Command::Connect(name) => format!("Couldn't connect to {name}"),
        Command::Disconnect(name) => format!("Couldn't disconnect {name}"),
        Command::Forget(name) => format!("Couldn't forget {name}"),
    };
    let has = |names: &[&str]| names.iter().any(|n| err.contains(n));
    let why = if has(&["AccessDenied", "NotAuthorized", "PermissionDenied"]) {
        "not allowed"
    } else if has(&["AuthenticationFailed", "AuthenticationRejected"]) {
        "the device refused (or the code didn't match)"
    } else if has(&["AuthenticationCanceled", "AuthenticationTimeout"]) {
        "it took too long"
    } else if has(&[
        "Page Timeout",
        "page-timeout",
        "Host is down",
        "ConnectionAttemptFailed",
    ]) {
        "it's off or out of range"
    } else if has(&["profile-unavailable"]) {
        "nothing on this computer can use it (is the audio server running?)"
    } else if has(&["NotReady", "Resource Not Ready"]) {
        "Bluetooth is off"
    } else if has(&["rfkill", "Blocked"]) {
        "it's switched off (airplane mode or a hardware switch)"
    } else if !err.contains("org.") {
        // Our own messages are already in words.
        return format!("{what}: {err}");
    } else {
        warn!(%err, "bluetooth");
        return what;
    };
    format!("{what}: {why}")
}

/// Diagnostics, read-only (lists, never powers, scans, pairs or connects):
/// `cargo test -p cnde-settings bluetooth_reads_this_machine -- --ignored --nocapture`
#[cfg(test)]
#[test]
#[ignore]
fn bluetooth_reads_this_machine() {
    let conn = Connection::system().unwrap();
    let (tx, _rx) = smithay_client_toolkit::reexports::calloop::channel::channel();
    let state = Worker::new(conn, tx).read();
    eprintln!(
        "{:?} powered={} discovering={}",
        state.availability, state.powered, state.discovering
    );
    for d in &state.devices {
        eprintln!(
            "  {:?} paired={} connected={} battery={:?} rssi={:?}",
            d.kind(),
            d.paired,
            d.connected,
            d.battery,
            d.rssi
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(value: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn path(p: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(p).unwrap()
    }

    fn device(
        address: &str,
        name: Option<&str>,
        paired: bool,
        connected: bool,
        rssi: Option<i16>,
    ) -> HashMap<String, HashMap<String, OwnedValue>> {
        let mut props = HashMap::from([
            ("Address".to_string(), v(Value::from(address))),
            (
                "Adapter".to_string(),
                v(Value::from(
                    ObjectPath::try_from("/org/bluez/hci0").unwrap(),
                )),
            ),
            ("Paired".to_string(), v(Value::from(paired))),
            ("Connected".to_string(), v(Value::from(connected))),
            ("Icon".to_string(), v(Value::from("audio-headphones"))),
        ]);
        props.insert(
            "Alias".into(),
            v(Value::from(name.unwrap_or(&address.replace(':', "-")))),
        );
        if let Some(name) = name {
            props.insert("Name".into(), v(Value::from(name)));
        }
        if let Some(rssi) = rssi {
            props.insert("RSSI".into(), v(Value::from(rssi)));
        }
        HashMap::from([(DEVICE.to_string(), props)])
    }

    #[test]
    fn reads_bluez_objects() {
        let mut objects: Objects = HashMap::new();
        assert_eq!(parse(&objects).availability, Availability::NoAdapter);

        objects.insert(
            path("/org/bluez/hci0"),
            HashMap::from([(
                ADAPTER.to_string(),
                HashMap::from([
                    ("Powered".to_string(), v(Value::from(true))),
                    ("Discovering".to_string(), v(Value::from(false))),
                ]),
            )]),
        );
        let dev = |a: &str| path(&format!("/org/bluez/hci0/dev_{}", a.replace(':', "_")));
        let entries = [
            ("00:00:00:00:00:01", Some("Zed Mouse"), true, false, None),
            ("00:00:00:00:00:02", Some("Buds"), true, true, None),
            (
                "00:00:00:00:00:03",
                Some("Far Speaker"),
                false,
                false,
                Some(-80),
            ),
            (
                "00:00:00:00:00:04",
                Some("Near Speaker"),
                false,
                false,
                Some(-40),
            ),
            // No name: a beacon, left out.
            ("00:00:00:00:00:05", None, false, false, Some(-30)),
        ];
        for (address, name, paired, connected, rssi) in entries {
            objects.insert(dev(address), device(address, name, paired, connected, rssi));
        }
        objects.get_mut(&dev("00:00:00:00:00:02")).unwrap().insert(
            "org.bluez.Battery1".into(),
            HashMap::from([("Percentage".to_string(), v(Value::from(80u8)))]),
        );

        let state = parse(&objects);
        assert_eq!(state.availability, Availability::Ready);
        assert!(state.powered);
        let names: Vec<&str> = state.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Buds", "Zed Mouse", "Near Speaker", "Far Speaker"]);
        assert_eq!(state.devices[0].battery, Some(80));
        assert_eq!(state.devices[0].kind(), "Headphones");
    }

    #[test]
    fn errors_in_words() {
        assert_eq!(
            describe(
                &Command::Connect("Buds".into()),
                "org.bluez.Error.Failed: Page Timeout"
            ),
            "Couldn't connect to Buds: it's off or out of range"
        );
        assert_eq!(
            describe(
                &Command::Pair("Keys".into()),
                "org.bluez.Error.AuthenticationFailed: Authentication Failed"
            ),
            "Couldn't pair with Keys: the device refused (or the code didn't match)"
        );
        assert_eq!(
            describe(&Command::SetPowered(true), "no Bluetooth adapter"),
            "Couldn't turn Bluetooth on: no Bluetooth adapter"
        );
    }
}
