//! Audio for the Sound page, through `pactl` (PipeWire's pipewire-pulse or
//! PulseAudio; the shell's volume uses the same tool). A worker thread
//! lists the output and input devices, the playing streams (`pactl -f json
//! list sink-inputs`) and who records, runs the volume, mute, move and
//! default commands, and re-lists shortly after `pactl subscribe` reports a
//! change, so streams appear and vanish live and external changes show up. `pactl` is an optional
//! dependency: without it the page says so.

use std::os::unix::process::CommandExt as _;

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command as Process, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tracing::{debug, warn};

/// Wait this long after a change before listing again, so a burst of
/// events (a stream starting touches several objects) costs one listing.
const DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Availability {
    #[default]
    Unknown,
    /// No `pactl`, or no audio server answering it.
    Unavailable,
    Ready,
}

/// Output (sink) or input (source).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Output,
    Input,
}

impl Kind {
    /// The word `pactl` uses for it.
    fn noun(self) -> &'static str {
        match self {
            Kind::Output => "sink",
            Kind::Input => "source",
        }
    }
}

/// An output or input device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub index: u32,
    pub name: String,
    pub description: String,
    /// A symbolic icon name.
    pub icon: &'static str,
    pub percent: u32,
    pub muted: bool,
    pub default: bool,
    /// Not an unplugged jack or cable.
    pub available: bool,
    /// A source that records an output (not a microphone).
    pub monitor: bool,
}

/// One playing stream, as `pactl` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub id: u32,
    /// Name of the sink (output device) it plays to.
    pub sink: String,
    pub volume: u32,
    pub muted: bool,
    pub app: String,
    /// What the app calls this stream ("Spotify", a tab's title).
    pub media: String,
    /// Which program, to tell apart apps (the binary, else the name).
    pub key: String,
    /// `application.icon_name`, when the app says.
    pub icon: String,
    /// `application.process.binary`, to find its desktop entry.
    pub binary: String,
}

/// An app's streams to one output: one row on the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub key: String,
    pub name: String,
    pub ids: Vec<u32>,
    /// The loudest stream's volume, in percent.
    pub volume: u32,
    /// Every stream is muted.
    pub muted: bool,
    /// The output (by name) they play to.
    pub sink: String,
    pub icon: String,
    pub binary: String,
}

impl App {
    /// Tells this row from the others: the same program can play to two
    /// outputs.
    pub fn id(&self) -> String {
        format!("{}\u{1}{}", self.sink, self.key)
    }
}

/// An app recording from an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorder {
    /// The input's name.
    pub source: String,
    pub app: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub availability: Availability,
    pub outputs: Vec<Device>,
    /// Real inputs: monitors of outputs are left out.
    pub inputs: Vec<Device>,
    pub apps: Vec<App>,
    pub recorders: Vec<Recorder>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Percent, for all of an app's streams.
    Volume(Vec<u32>, u32),
    Mute(Vec<u32>, bool),
    /// Play to the sink with this name.
    Output(Vec<u32>, String),
    /// A device's own volume (by name).
    DeviceVolume(Kind, String, u32),
    /// Make the input the default, and bring the recording apps along.
    DefaultInput(String),
}

enum Msg {
    /// `pactl subscribe` saw something.
    Changed,
    Command(Command),
    Stop,
}

/// The running worker; dropping it stops the worker and kills `pactl
/// subscribe` by its own PID.
pub struct Handle {
    tx: mpsc::Sender<Msg>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Handle {
    pub fn command(&self, command: Command) {
        let _ = self.tx.send(Msg::Command(command));
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Stop);
        kill(&self.child);
    }
}

fn kill(slot: &Mutex<Option<Child>>) {
    if let Some(mut child) = slot.lock().ok().and_then(|mut c| c.take()) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub fn start(tx: Sender<State>) -> Handle {
    let (commands, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(None));
    {
        let (commands, stop, child) = (commands.clone(), stop.clone(), child.clone());
        let _ = std::thread::Builder::new()
            .name("sound-events".into())
            .spawn(move || subscribe(&commands, &stop, &child));
    }
    let _ = std::thread::Builder::new()
        .name("sound".into())
        .spawn(move || run(&tx, &rx));
    Handle {
        tx: commands,
        stop,
        child,
    }
}

/// Run `pactl subscribe`, turning sink-input events into [`Msg::Changed`];
/// start it again if the audio server restarts.
fn subscribe(tx: &mpsc::Sender<Msg>, stop: &AtomicBool, slot: &Mutex<Option<Child>>) {
    while !stop.load(Ordering::SeqCst) {
        let mut cmd = Process::new("pactl");
        cmd.arg("subscribe");
        // Dies with Settings, even when Settings is killed outright.
        // SAFETY: prctl is async-signal-safe and touches no shared state.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ))
                .map_err(std::io::Error::from)
            });
        }
        let spawned = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            let stdout = child.stdout.take();
            if let Ok(mut held) = slot.lock() {
                *held = Some(child);
            }
            // Dropped while spawning: the handle's kill came too early.
            if stop.load(Ordering::SeqCst) {
                kill(slot);
                return;
            }
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    // "Event 'new' on sink-input #12", "... on server", ...
                    if is_stream_event(&line) && tx.send(Msg::Changed).is_err() {
                        return;
                    }
                }
            }
        }
        // No audio server (yet): try again shortly.
        for _ in 0..30 {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = tx.send(Msg::Changed);
    }
}

/// Whether a `pactl subscribe` line can change the list: streams coming,
/// going or changing, and the devices.
fn is_stream_event(line: &str) -> bool {
    [
        "sink-input",
        " sink #",
        "source-output",
        " source #",
        "server",
    ]
    .iter()
    .any(|word| line.contains(word))
}

fn run(tx: &Sender<State>, rx: &mpsc::Receiver<Msg>) {
    let mut last: Option<State> = None;
    let refresh = |last: &mut Option<State>| {
        let state = read();
        if last.as_ref() != Some(&state) {
            let _ = tx.send(state.clone());
            *last = Some(state);
        }
    };
    refresh(&mut last);
    while let Ok(msg) = rx.recv() {
        let mut commands = Vec::new();
        let mut stopping = false;
        let mut take = |msg: Msg| match msg {
            Msg::Command(c) => commands.push(c),
            Msg::Changed => {}
            Msg::Stop => stopping = true,
        };
        take(msg);
        // Let a burst settle (and a dragged slider's updates pile up).
        while let Ok(msg) = rx.recv_timeout(DEBOUNCE) {
            take(msg);
        }
        if stopping {
            return;
        }
        for command in coalesce(commands) {
            debug!(?command, "sound");
            if let Err(err) = apply(&command) {
                warn!(%err, "pactl failed");
            }
        }
        refresh(&mut last);
    }
}

/// Only the last volume for a set of streams matters.
fn coalesce(commands: Vec<Command>) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::new();
    for command in commands {
        match &command {
            Command::Volume(ids, _) => {
                out.retain(|c| !matches!(c, Command::Volume(other, _) if other == ids));
            }
            Command::DeviceVolume(kind, name, _) => out
                .retain(|c| !matches!(c, Command::DeviceVolume(k, n, _) if k == kind && n == name)),
            _ => {}
        }
        out.push(command);
    }
    out
}

fn pactl(args: &[&str]) -> Result<String, String> {
    let output = Process::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(format!("exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn apply(command: &Command) -> Result<(), String> {
    match command {
        Command::Volume(ids, percent) => {
            for id in ids {
                let id = id.to_string();
                pactl(&["set-sink-input-volume", &id, &format!("{percent}%")])?;
            }
        }
        Command::Mute(ids, mute) => {
            for id in ids {
                let id = id.to_string();
                pactl(&["set-sink-input-mute", &id, if *mute { "1" } else { "0" }])?;
            }
        }
        Command::Output(ids, sink) => {
            for id in ids {
                pactl(&["move-sink-input", &id.to_string(), sink])?;
            }
        }
        Command::DeviceVolume(kind, name, percent) => {
            let verb = format!("set-{}-volume", kind.noun());
            pactl(&[&verb, name, &format!("{percent}%")])?;
        }
        Command::DefaultInput(name) => {
            let inputs = pactl(&["-f", "json", "list", "sources"])
                .map(|t| parse_devices(&t, Kind::Input, ""))
                .unwrap_or_default();
            let outputs = pactl(&["-f", "json", "list", "source-outputs"]).unwrap_or_default();
            pactl(&["set-default-source", name])?;
            // Streams that refuse to move (some can't) are skipped.
            for id in movable_recorders(&outputs, &inputs) {
                let _ = pactl(&["move-source-output", &id.to_string(), name]);
            }
        }
    }
    Ok(())
}

fn read() -> State {
    let (Ok(inputs), Ok(sinks), Ok(sources), Ok(info)) = (
        pactl(&["-f", "json", "list", "sink-inputs"]),
        pactl(&["-f", "json", "list", "sinks"]),
        pactl(&["-f", "json", "list", "sources"]),
        pactl(&["-f", "json", "info"]),
    ) else {
        return State {
            availability: Availability::Unavailable,
            ..State::default()
        };
    };
    let defaults = parse_json(&info);
    let default = |key: &str| defaults.as_ref().and_then(|d| d.str(key)).unwrap_or("");
    let outputs = parse_devices(&sinks, Kind::Output, default("default_sink_name"));
    let all_inputs = parse_devices(&sources, Kind::Input, default("default_source_name"));
    let recorders = pactl(&["-f", "json", "list", "source-outputs"])
        .map(|t| parse_recorders(&t, &all_inputs))
        .unwrap_or_default();
    let streams = parse_streams(&inputs, &outputs);
    State {
        availability: Availability::Ready,
        apps: group(&streams),
        outputs,
        inputs: all_inputs.into_iter().filter(|d| !d.monitor).collect(),
        recorders,
    }
}

/// Streams of the same program to the same output become one app, in order
/// of first appearance: the slider sets them all, the mute switch is on
/// when every one is muted.
pub fn group(streams: &[Stream]) -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    for s in streams {
        match apps.iter_mut().find(|a| a.key == s.key && a.sink == s.sink) {
            Some(app) => {
                app.ids.push(s.id);
                app.volume = app.volume.max(s.volume);
                app.muted &= s.muted;
            }
            None => apps.push(App {
                key: s.key.clone(),
                name: s.app.clone(),
                ids: vec![s.id],
                volume: s.volume,
                muted: s.muted,
                sink: s.sink.clone(),
                icon: s.icon.clone(),
                binary: s.binary.clone(),
            }),
        }
    }
    apps
}

// --- Parsing. `pactl -f json` is plain JSON, and this crate has no JSON
// dependency, so this is the small slice of it that's needed.

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn num(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        self.ws();
        (self.s.get(self.i) == Some(&c)).then(|| self.i += 1)
    }

    fn lit(&mut self, word: &str, value: Json) -> Option<Json> {
        self.s[self.i..].starts_with(word.as_bytes()).then(|| {
            self.i += word.len();
            value
        })
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut fields = Vec::new();
                if self.eat(b'}').is_some() {
                    return Some(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.eat(b':')?;
                    fields.push((key, self.value()?));
                    if self.eat(b',').is_none() {
                        self.eat(b'}')?;
                        return Some(Json::Obj(fields));
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                if self.eat(b']').is_some() {
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value()?);
                    if self.eat(b',').is_none() {
                        self.eat(b']')?;
                        return Some(Json::Arr(items));
                    }
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'n' => self.lit("null", Json::Null),
            _ => {
                let start = self.i;
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|c| c.is_ascii_digit() || b"+-.eE".contains(c))
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Json::Num)
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(digits, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xd800..0xdc00).contains(&code)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                self.i += 2;
                                let low = self.hex4()?;
                                code = 0x10000 + ((code - 0xd800) << 10) + low.wrapping_sub(0xdc00);
                            }
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        other => other as char,
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c => out.push(c),
            }
        }
    }
}

fn parse_json(text: &str) -> Option<Json> {
    Parser {
        s: text.as_bytes(),
        i: 0,
    }
    .value()
}

/// The devices `pactl -f json list sinks` (or sources) describes.
pub fn parse_devices(text: &str, kind: Kind, default: &str) -> Vec<Device> {
    let Some(Json::Arr(items)) = parse_json(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.str("name")?;
            let props = item.get("properties");
            let prop = |k: &str| props.and_then(|p| p.str(k)).unwrap_or("");
            let monitor = kind == Kind::Input
                && (name.ends_with(".monitor")
                    || matches!(item.get("monitor_of_sink"), Some(Json::Num(_)))
                    || prop("device.class") == "monitor");
            let active_port = item.str("active_port");
            let (mut port_type, mut available) = ("", true);
            if let Some(Json::Arr(list)) = item.get("ports") {
                for port in list.iter().filter(|p| p.str("name") == active_port) {
                    port_type = port.str("type").unwrap_or("");
                    available = port.str("availability") != Some("not available");
                }
            }
            let volume = match item.get("volume") {
                Some(Json::Obj(channels)) => channels
                    .iter()
                    .filter_map(|(_, c)| c.num("value"))
                    .fold(0.0, f64::max),
                _ => 0.0,
            };
            Some(Device {
                index: item.num("index")? as u32,
                name: name.into(),
                description: item.str("description").unwrap_or(name).into(),
                icon: icon(
                    kind,
                    port_type,
                    prop("device.form_factor"),
                    prop("device.bus"),
                ),
                percent: (volume * 100.0 / 65536.0).round() as u32,
                muted: matches!(item.get("mute"), Some(Json::Bool(true))),
                default: name == default,
                available,
                monitor,
            })
        })
        .collect()
}

/// The icon for a device: what's plugged into it, else what kind of
/// device it is, else how it connects (as the shell's quick settings do).
fn icon(kind: Kind, port_type: &str, form_factor: &str, bus: &str) -> &'static str {
    match port_type.to_lowercase().as_str() {
        "headphones" => return "audio-headphones-symbolic",
        "headset" => return "audio-headset-symbolic",
        "speaker" => return "audio-speakers-symbolic",
        "hdmi" | "displayport" => return "video-display-symbolic",
        "mic" => return "audio-input-microphone-symbolic",
        _ => {}
    }
    match form_factor {
        "headphone" | "hands-free" => "audio-headphones-symbolic",
        "headset" => "audio-headset-symbolic",
        "speaker" | "tv" | "computer" => "audio-speakers-symbolic",
        "hdmi" => "video-display-symbolic",
        "microphone" | "webcam" => "audio-input-microphone-symbolic",
        _ if bus == "usb" && kind == Kind::Output => "audio-card-symbolic",
        _ if kind == Kind::Input => "audio-input-microphone-symbolic",
        _ => "audio-speakers-symbolic",
    }
}

/// A recording stream worth showing: on a real input, not a volume meter's
/// peak detector or a monitor (screen-sharing audio).
fn is_mic_stream(item: &Json, inputs: &[Device]) -> bool {
    let props = item.get("properties");
    let prop = |k: &str| props.and_then(|p| p.str(k)).unwrap_or("");
    if prop("stream.monitor") == "true" || prop("media.name").eq_ignore_ascii_case("peak detect") {
        return false;
    }
    let source = item.num("source").unwrap_or(-1.0);
    inputs
        .iter()
        .any(|d| f64::from(d.index) == source && !d.monitor)
}

/// Who records from which input, from `pactl -f json list source-outputs`.
/// `inputs` is every source, monitors included.
pub fn parse_recorders(text: &str, inputs: &[Device]) -> Vec<Recorder> {
    let Some(Json::Arr(items)) = parse_json(text) else {
        return Vec::new();
    };
    let mut out: Vec<Recorder> = Vec::new();
    for item in items.iter().filter(|i| is_mic_stream(i, inputs)) {
        let props = item.get("properties");
        let prop = |k: &str| props.and_then(|p| p.str(k)).unwrap_or("");
        let app = [prop("application.name"), prop("application.process.binary")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("Unknown app");
        let source = item.num("source").unwrap_or(-1.0);
        let Some(device) = inputs.iter().find(|d| f64::from(d.index) == source) else {
            continue;
        };
        let recorder = Recorder {
            source: device.name.clone(),
            app: app.into(),
        };
        if !out.contains(&recorder) {
            out.push(recorder);
        }
    }
    out
}

/// The recording streams to bring along to a new default input: those on
/// real inputs (a monitor stream would end up recording the microphone).
pub fn movable_recorders(text: &str, inputs: &[Device]) -> Vec<u32> {
    let Some(Json::Arr(items)) = parse_json(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| is_mic_stream(item, inputs))
        .filter_map(|item| item.num("index").map(|n| n as u32))
        .collect()
}

/// The streams worth showing: not event sounds (volume-key pings, which
/// have `media.role = event`) and not volume meters' peak detectors.
pub fn parse_streams(text: &str, sinks: &[Device]) -> Vec<Stream> {
    let Some(Json::Arr(items)) = parse_json(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item.num("index")? as u32;
            let props = item.get("properties")?;
            let prop = |k: &str| props.str(k).unwrap_or("").to_string();
            if prop("media.role") == "event"
                || prop("media.name").eq_ignore_ascii_case("peak detect")
            {
                return None;
            }
            let binary = prop("application.process.binary");
            let app = [prop("application.name"), binary.clone()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| "Unknown app".into());
            let key = if binary.is_empty() { &app } else { &binary }.to_lowercase();
            // The sink is an index here; the commands want its name.
            let sink_index = item.num("sink")? as u32;
            let sink = sinks
                .iter()
                .find(|s| s.index == sink_index)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            let volume = match item.get("volume") {
                Some(Json::Obj(channels)) => channels
                    .iter()
                    .filter_map(|(_, c)| c.num("value"))
                    .fold(0.0, f64::max),
                _ => 0.0,
            };
            Some(Stream {
                id,
                sink,
                volume: (volume * 100.0 / 65536.0).round() as u32,
                muted: matches!(item.get("mute"), Some(Json::Bool(true))),
                app,
                media: prop("media.name"),
                key,
                icon: prop("application.icon_name"),
                binary,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINKS: &str = r#"[
        {"index":73,"name":"alsa_output.usb-Scarlett","description":"Scarlett \"Headphones\"","mute":false},
        {"index":74,"name":"alsa_output.hdmi","description":"HDMI é"}
    ]"#;

    const INPUTS: &str = r#"[
        {"index":10,"sink":73,"mute":false,
         "volume":{"front-left":{"value":32768,"value_percent":"50%"},"front-right":{"value":39322,"value_percent":"60%"}},
         "properties":{"application.name":"Firefox","application.process.binary":"firefox","media.name":"Some \"video\" – tab"}},
        {"index":11,"sink":74,"mute":true,
         "volume":{"mono":{"value":65536,"value_percent":"100%"}},
         "properties":{"application.name":"Firefox","application.process.binary":"firefox","media.name":"Other tab"}},
        {"index":12,"sink":73,"mute":false,"volume":{"mono":{"value":65536}},
         "properties":{"application.name":"libcanberra","media.role":"event","media.name":"bell"}},
        {"index":13,"sink":73,"mute":false,"volume":{"mono":{"value":65536}},
         "properties":{"application.name":"PulseAudio Volume Control","media.name":"Peak detect"}},
        {"index":14,"sink":73,"mute":false,"volume":{"mono":{"value":0}},
         "properties":{"application.name":"mpv Media Player","media.name":"song"}}
    ]"#;

    fn sinks() -> Vec<Device> {
        parse_devices(SINKS, Kind::Output, "alsa_output.hdmi")
    }

    #[test]
    fn parses_sinks() {
        let sinks = sinks();
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].index, 73);
        assert_eq!(sinks[0].description, "Scarlett \"Headphones\"");
        assert_eq!(sinks[1].description, "HDMI é");
        assert!(!sinks[0].default && sinks[1].default);
    }

    const DEVICES: &str = r#"[
        {"index":1,"name":"alsa_output.pci.analog","description":"Built-in","mute":false,
         "volume":{"front-left":{"value":32768},"front-right":{"value":16384}},
         "active_port":"analog-output-headphones",
         "ports":[{"name":"analog-output-speaker","type":"Speaker","availability":"not available"},
                  {"name":"analog-output-headphones","type":"Headphones","availability":"available"}],
         "properties":{"device.form_factor":"internal","device.bus":"pci"}},
        {"index":2,"name":"alsa_output.hdmi","description":"HDMI","mute":true,
         "active_port":"hdmi-output-0",
         "ports":[{"name":"hdmi-output-0","type":"HDMI","availability":"not available"}],
         "properties":{}},
        {"index":3,"name":"alsa_output.usb","description":"Scarlett","mute":false,
         "properties":{"device.bus":"usb"}}
    ]"#;

    #[test]
    fn devices_get_icons_levels_and_availability() {
        let d = parse_devices(DEVICES, Kind::Output, "alsa_output.usb");
        assert_eq!(d[0].icon, "audio-headphones-symbolic");
        assert_eq!(d[0].percent, 50);
        assert!(d[0].available);
        assert_eq!(d[1].icon, "video-display-symbolic");
        assert!(d[1].muted && !d[1].available);
        assert_eq!(d[2].icon, "audio-card-symbolic");
        assert!(d[2].default && d[2].available);
    }

    const SOURCES: &str = r#"[
        {"index":20,"name":"alsa_input.mic","description":"Mic","mute":false,"properties":{"device.form_factor":"microphone"}},
        {"index":21,"name":"alsa_output.usb.monitor","description":"Monitor of Scarlett","monitor_of_sink":3,"mute":false,"properties":{}}
    ]"#;

    const OUTPUTS: &str = r#"[
        {"index":40,"source":20,"properties":{"application.name":"Discord"}},
        {"index":41,"source":20,"properties":{"application.name":"Discord"}},
        {"index":42,"source":20,"properties":{"application.name":"pavucontrol","media.name":"Peak detect"}},
        {"index":43,"source":21,"properties":{"application.name":"OBS"}},
        {"index":44,"source":20,"properties":{"application.name":"Portal","stream.monitor":"true"}},
        {"index":45,"source":20,"properties":{"application.process.binary":"arecord"}}
    ]"#;

    #[test]
    fn recording_ignores_monitors_and_meters() {
        let inputs = parse_devices(SOURCES, Kind::Input, "alsa_input.mic");
        assert!(!inputs[0].monitor && inputs[1].monitor);
        let who = parse_recorders(OUTPUTS, &inputs);
        let apps: Vec<&str> = who.iter().map(|r| r.app.as_str()).collect();
        // Two streams of one app count once; the binary names an unnamed one.
        assert_eq!(apps, ["Discord", "arecord"]);
        assert!(who.iter().all(|r| r.source == "alsa_input.mic"));
        // Only real microphone streams come along to a new default input.
        assert_eq!(movable_recorders(OUTPUTS, &inputs), [40, 41, 45]);
        assert!(parse_recorders("nope", &inputs).is_empty());
    }

    #[test]
    fn parses_streams_and_drops_system_ones() {
        let streams = parse_streams(INPUTS, &sinks());
        let ids: Vec<u32> = streams.iter().map(|s| s.id).collect();
        // The event sound and the peak detector are hidden.
        assert_eq!(ids, [10, 11, 14]);
        // The loudest channel, as a percentage.
        assert_eq!(streams[0].volume, 60);
        assert_eq!(streams[0].sink, "alsa_output.usb-Scarlett");
        assert_eq!(streams[0].media, "Some \"video\" – tab");
        assert_eq!(streams[0].binary, "firefox");
        assert!(streams[1].muted);
        assert_eq!(streams[1].sink, "alsa_output.hdmi");
        // No binary: the key falls back to the app name.
        assert_eq!(streams[2].key, "mpv media player");
        assert_eq!(streams[2].volume, 0);
    }

    #[test]
    fn groups_streams_by_program_and_output() {
        let streams = parse_streams(INPUTS, &sinks());
        // Firefox plays to two outputs: a row on each.
        let apps = group(&streams);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].ids, [10]);
        assert_eq!(apps[0].sink, "alsa_output.usb-Scarlett");
        assert_eq!(apps[1].ids, [11]);
        assert_eq!(apps[1].sink, "alsa_output.hdmi");
        assert!(apps[1].muted);
        assert_ne!(apps[0].id(), apps[1].id());
        assert_eq!(apps[2].name, "mpv Media Player");
        // The same program to the same output is one row, the loudest volume.
        let mut more = streams.clone();
        more.push(Stream {
            id: 15,
            volume: 100,
            muted: true,
            ..streams[0].clone()
        });
        let apps = group(&more);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].ids, [10, 15]);
        assert_eq!(apps[0].volume, 100);
        // Muted only when every stream is.
        assert!(!apps[0].muted);
    }

    #[test]
    fn bad_output_is_nothing_playing() {
        assert!(parse_streams("", &[]).is_empty());
        assert!(parse_streams("{\"not\": \"a list\"}", &[]).is_empty());
        assert!(parse_streams("[]", &[]).is_empty());
        assert!(parse_devices("[{\"index\":", Kind::Output, "").is_empty());
    }

    #[test]
    fn only_the_last_volume_is_sent() {
        let sets = vec![
            Command::DeviceVolume(Kind::Input, "a".into(), 10),
            Command::DeviceVolume(Kind::Output, "a".into(), 20),
            Command::DeviceVolume(Kind::Input, "a".into(), 30),
        ];
        assert_eq!(
            coalesce(sets),
            [
                Command::DeviceVolume(Kind::Output, "a".into(), 20),
                Command::DeviceVolume(Kind::Input, "a".into(), 30),
            ]
        );
        let commands = vec![
            Command::Volume(vec![1], 10),
            Command::Mute(vec![1], true),
            Command::Volume(vec![2], 5),
            Command::Volume(vec![1], 30),
        ];
        assert_eq!(
            coalesce(commands),
            [
                Command::Mute(vec![1], true),
                Command::Volume(vec![2], 5),
                Command::Volume(vec![1], 30),
            ]
        );
    }

    #[test]
    fn subscribe_events_that_matter() {
        assert!(is_stream_event("Event 'new' on sink-input #12"));
        assert!(is_stream_event("Event 'remove' on sink-input #12"));
        assert!(is_stream_event("Event 'change' on sink #5"));
        assert!(is_stream_event("Event 'change' on source #5"));
        assert!(is_stream_event("Event 'new' on source-output #7"));
        assert!(!is_stream_event("Event 'new' on client #9"));
    }
}
