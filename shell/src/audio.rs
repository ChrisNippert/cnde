//! Audio devices for quick settings, through `pactl` (PipeWire's
//! pipewire-pulse or PulseAudio; the shell's volume uses the same tool).
//! `read` lists the outputs (sinks) and inputs (sources) with their ports
//! and volumes; `select` makes one the default and moves the playing or
//! recording streams over to it, so audio follows like it does in GNOME.

use std::process::{Command, Stdio};

/// Output (sink) or input (source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// The word `pactl` uses for the streams on it.
    fn stream(self) -> &'static str {
        match self {
            Kind::Output => "sink-input",
            Kind::Input => "source-output",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub description: String,
}

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
    /// What it can be plugged into right now (e.g. headphones and
    /// speakers of one card); only worth listing when there are several.
    pub ports: Vec<Port>,
    pub active_port: Option<String>,
    /// A source that records an output (not a microphone).
    pub monitor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Audio {
    pub outputs: Vec<Device>,
    /// Real inputs: monitors of outputs are left out.
    pub inputs: Vec<Device>,
    /// Some app is recording from an input.
    pub recording: bool,
}

impl Audio {
    pub fn devices(&self, kind: Kind) -> &[Device] {
        match kind {
            Kind::Output => &self.outputs,
            Kind::Input => &self.inputs,
        }
    }

    pub fn default_device(&self, kind: Kind) -> Option<&Device> {
        self.devices(kind).iter().find(|d| d.default)
    }
}

fn pactl(args: &[&str]) -> Option<String> {
    let output = Command::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Everything the card shows; `None` without `pactl` or an audio server.
pub fn read() -> Option<Audio> {
    let info = parse_json(&pactl(&["-f", "json", "info"])?)?;
    let sinks = pactl(&["-f", "json", "list", "sinks"])?;
    let sources = pactl(&["-f", "json", "list", "sources"])?;
    let outputs = parse_devices(
        &sinks,
        Kind::Output,
        info.str("default_sink_name").unwrap_or(""),
    );
    let all_inputs = parse_devices(
        &sources,
        Kind::Input,
        info.str("default_source_name").unwrap_or(""),
    );
    let recording = pactl(&["-f", "json", "list", "source-outputs"])
        .is_some_and(|text| recording(&text, &all_inputs));
    Some(Audio {
        outputs,
        inputs: all_inputs.into_iter().filter(|d| !d.monitor).collect(),
        recording,
    })
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
            let active_port = item.str("active_port").map(String::from);
            let mut port_types = Vec::new();
            let mut ports = Vec::new();
            if let Some(Json::Arr(list)) = item.get("ports") {
                for port in list {
                    let Some(port_name) = port.str("name") else {
                        continue;
                    };
                    port_types.push((port_name, port.str("type").unwrap_or("")));
                    // An unplugged jack isn't a choice (unless it's the
                    // one in use).
                    if port.str("availability") == Some("not available")
                        && active_port.as_deref() != Some(port_name)
                    {
                        continue;
                    }
                    ports.push(Port {
                        name: port_name.into(),
                        description: port.str("description").unwrap_or(port_name).into(),
                    });
                }
            }
            let active_type = port_types
                .iter()
                .find(|(n, _)| Some(*n) == active_port.as_deref())
                .map_or("", |(_, t)| t);
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
                    active_type,
                    prop("device.form_factor"),
                    prop("device.bus"),
                ),
                percent: (volume * 100.0 / 65536.0).round() as u32,
                muted: matches!(item.get("mute"), Some(Json::Bool(true))),
                default: name == default,
                ports,
                active_port,
                monitor,
            })
        })
        .collect()
}

/// The icon for a device: what's plugged into it, else what kind of
/// device it is, else how it connects.
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

/// Whether any stream in `pactl -f json list source-outputs` records a real
/// input: not a monitor of an output (screen-sharing audio, volume
/// meters) and not a peak detector.
pub fn recording(text: &str, inputs: &[Device]) -> bool {
    let Some(Json::Arr(items)) = parse_json(text) else {
        return false;
    };
    items.iter().any(|item| is_mic_stream(item, inputs))
}

fn is_mic_stream(item: &Json, inputs: &[Device]) -> bool {
    let props = item.get("properties");
    let prop = |k: &str| props.and_then(|p| p.str(k)).unwrap_or("");
    if prop("stream.monitor") == "true" || prop("media.name").eq_ignore_ascii_case("peak detect") {
        return false;
    }
    let source = item.num("source").unwrap_or(-1.0);
    inputs
        .iter()
        .any(|d| d.index as f64 == source && !d.monitor)
}

/// The `pactl` calls that make `name` the default and move `streams` to it.
pub fn select_plan(kind: Kind, name: &str, streams: &[u32]) -> Vec<Vec<String>> {
    let mut plan = vec![vec![format!("set-default-{}", kind.noun()), name.into()]];
    plan.extend(streams.iter().map(|id| {
        vec![
            format!("move-{}", kind.stream()),
            id.to_string(),
            name.into(),
        ]
    }));
    plan
}

/// The streams to move along with a new default: all playing streams, but
/// for recording only those on real inputs (a monitor stream would end up
/// recording the microphone).
pub fn movable_streams(kind: Kind, text: &str, inputs: &[Device]) -> Vec<u32> {
    let Some(Json::Arr(items)) = parse_json(text) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| kind == Kind::Output || is_mic_stream(item, inputs))
        .filter_map(|item| item.num("index").map(|n| n as u32))
        .collect()
}

/// Make `name` the default and bring the streams along. Streams that
/// refuse to move (some can't) are skipped.
pub fn select(kind: Kind, name: &str) -> Result<(), String> {
    let inputs = match kind {
        Kind::Input => pactl(&["-f", "json", "list", "sources"])
            .map(|t| parse_devices(&t, Kind::Input, ""))
            .unwrap_or_default(),
        Kind::Output => Vec::new(),
    };
    let list = format!("{}s", kind.stream());
    let streams = pactl(&["-f", "json", "list", &list])
        .map(|t| movable_streams(kind, &t, &inputs))
        .unwrap_or_default();
    for (i, args) in select_plan(kind, name, &streams).iter().enumerate() {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if pactl(&args).is_none() && i == 0 {
            return Err(format!("pactl {} failed", args[0]));
        }
    }
    Ok(())
}

/// Switch a device's port (headphones to speakers) and make it the default.
pub fn select_port(kind: Kind, device: &str, port: &str) -> Result<(), String> {
    pactl(&[&format!("set-{}-port", kind.noun()), device, port])
        .ok_or_else(|| "pactl set port failed".to_string())?;
    select(kind, device)
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

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from `pactl -f json list sinks`: a USB interface and an HDMI
    // output (one port each), and a laptop card with two ports.
    const SINKS: &str = r#"[
      {"index":73,"name":"alsa_output.usb-Scarlett.HiFi__Line1__sink","description":"Scarlett 2i2 Headphones / Line 1-2",
       "mute":false,"volume":{"front-left":{"value":39976,"value_percent":"61%"},"front-right":{"value":39976,"value_percent":"61%"}},
       "active_port":"[Out] Line1",
       "ports":[{"name":"[Out] Line1","description":"Headphones / Line 1-2","type":"Line","availability":"availability unknown"}],
       "properties":{"device.bus":"usb","device.icon_name":"audio-speakers"}},
      {"index":61,"name":"alsa_output.pci-0000_0b_00.1.hdmi-stereo-extra2","description":"HDMI 3","mute":true,
       "volume":{"front-left":{"value":65536,"value_percent":"100%"}},
       "active_port":"hdmi-output-2",
       "ports":[{"name":"hdmi-output-2","description":"HDMI / DisplayPort 3","type":"HDMI","availability":"available"}],
       "properties":{"device.bus":"pci"}},
      {"index":50,"name":"alsa_output.pci-0000_00_1f.3.analog-stereo","description":"Built-in Audio Analog Stereo","mute":false,
       "volume":{"mono":{"value":32768,"value_percent":"50%"}},
       "active_port":"analog-output-speaker",
       "ports":[{"name":"analog-output-speaker","description":"Speakers","type":"Speaker","availability":"availability unknown"},
                {"name":"analog-output-headphones","description":"Headphones","type":"Headphones","availability":"not available"}],
       "properties":{"device.bus":"pci","device.form_factor":"internal"}}
    ]"#;

    const SOURCES: &str = r#"[
      {"index":71,"name":"alsa_input.usb-Scarlett.HiFi__Mic1__source","description":"Scarlett Input 1","mute":false,
       "volume":{"mono":{"value":65536,"value_percent":"100%"}},"monitor_of_sink":null,
       "active_port":"[In] Mic1","ports":[{"name":"[In] Mic1","description":"Input 1","type":"Mic","availability":"availability unknown"}],
       "properties":{"device.bus":"usb"}},
      {"index":60,"name":"alsa_output.usb-Scarlett.HiFi__Line1__sink.monitor","description":"Monitor of Scarlett","mute":false,
       "volume":{"front-left":{"value":65536,"value_percent":"100%"}},"monitor_of_sink":73,
       "active_port":null,"ports":[],"properties":{}},
      {"index":62,"name":"alsa_output.pci.hdmi.monitor2","description":"Monitor of HDMI","mute":false,
       "volume":{"front-left":{"value":65536,"value_percent":"100%"}},"monitor_of_sink":null,
       "active_port":null,"ports":[],"properties":{"device.class":"monitor"}},
      {"index":63,"name":"alsa_input.pci.analog-stereo","description":"Built-in Audio","mute":true,
       "volume":{"front-left":{"value":32768,"value_percent":"50%"}},"monitor_of_sink":null,
       "active_port":"analog-input-front-mic",
       "ports":[{"name":"analog-input-front-mic","description":"Front Microphone","type":"Mic","availability":"not available"},
                {"name":"analog-input-linein","description":"Line In","type":"Line","availability":"not available"}],
       "properties":{"device.bus":"pci"}}
    ]"#;

    #[test]
    fn sinks_with_ports_and_default() {
        let default = "alsa_output.usb-Scarlett.HiFi__Line1__sink";
        let sinks = parse_devices(SINKS, Kind::Output, default);
        assert_eq!(sinks.len(), 3);
        assert!(sinks[0].default && !sinks[1].default);
        assert_eq!((sinks[0].percent, sinks[0].muted), (61, false));
        assert_eq!((sinks[1].percent, sinks[1].muted), (100, true));
        assert_eq!(sinks[0].icon, "audio-card-symbolic");
        assert_eq!(sinks[1].icon, "video-display-symbolic");
        assert_eq!(sinks[2].icon, "audio-speakers-symbolic");
        // The unplugged headphone jack isn't offered; the speakers are active.
        assert_eq!(sinks[2].ports.len(), 1);
        assert_eq!(
            sinks[2].active_port.as_deref(),
            Some("analog-output-speaker")
        );
        assert_eq!(sinks[2].percent, 50);
    }

    #[test]
    fn available_ports_are_listed() {
        let text = SINKS.replace("\"not available\"", "\"available\"");
        let sinks = parse_devices(&text, Kind::Output, "");
        let names: Vec<_> = sinks[2]
            .ports
            .iter()
            .map(|p| p.description.as_str())
            .collect();
        assert_eq!(names, ["Speakers", "Headphones"]);
    }

    #[test]
    fn monitors_are_not_inputs() {
        let default = "alsa_input.usb-Scarlett.HiFi__Mic1__source";
        let sources = parse_devices(SOURCES, Kind::Input, default);
        let monitors: Vec<_> = sources.iter().map(|d| d.monitor).collect();
        // By name, by monitor_of_sink and by device.class.
        assert_eq!(monitors, [false, true, true, false]);
        assert!(sources[0].default);
        assert_eq!(sources[0].icon, "audio-input-microphone-symbolic");
        assert!(sources[3].muted);
        // The only (unplugged) port stays, being the active one.
        assert_eq!(sources[3].ports.len(), 1);
    }

    #[test]
    fn recording_ignores_monitors_and_peak_detect() {
        let inputs = parse_devices(SOURCES, Kind::Input, "");
        let stream = |source: u32, props: &str| {
            format!(r#"[{{"index":9,"source":{source},"properties":{{{props}}}}}]"#)
        };
        assert!(recording(
            &stream(71, r#""media.name":"recStream""#),
            &inputs
        ));
        assert!(!recording(&stream(60, r#""media.name":"x""#), &inputs));
        assert!(!recording(
            &stream(4294967295, r#""media.name":"x""#),
            &inputs
        ));
        assert!(!recording(
            &stream(71, r#""media.name":"Peak detect""#),
            &inputs
        ));
        assert!(!recording(
            &stream(71, r#""stream.monitor":"true""#),
            &inputs
        ));
        assert!(!recording("[]", &inputs));
    }

    #[test]
    fn selecting_moves_the_streams_along() {
        let plan = select_plan(Kind::Output, "spk", &[4, 7]);
        assert_eq!(
            plan,
            [
                vec!["set-default-sink", "spk"],
                vec!["move-sink-input", "4", "spk"],
                vec!["move-sink-input", "7", "spk"],
            ]
        );
        let plan = select_plan(Kind::Input, "mic", &[3]);
        assert_eq!(plan[0], ["set-default-source", "mic"]);
        assert_eq!(plan[1], ["move-source-output", "3", "mic"]);
        assert_eq!(select_plan(Kind::Input, "mic", &[]).len(), 1);
    }

    #[test]
    fn only_mic_streams_move_to_a_new_input() {
        let inputs = parse_devices(SOURCES, Kind::Input, "");
        let text = r#"[{"index":1,"source":71,"properties":{}},
                        {"index":2,"source":60,"properties":{}},
                        {"index":3,"source":63,"properties":{}}]"#;
        assert_eq!(movable_streams(Kind::Input, text, &inputs), [1, 3]);
        // Every playing stream follows a new output.
        assert_eq!(movable_streams(Kind::Output, text, &[]), [1, 2, 3]);
    }
}
