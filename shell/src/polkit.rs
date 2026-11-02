//! The polkit authentication agent: when something asks to do what needs
//! a password (pkexec, Settings changing system state, mounting a disk),
//! polkit asks us, and we show a dialog for it. The password goes straight
//! to polkit's own helper, which checks it with PAM and tells polkit; all
//! we learn is whether it worked.
//!
//! The D-Bus side runs on its own thread and waits there for the dialog's
//! answer; the dialog lives on the main loop like the other surfaces.

use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    process::{Command, Stdio},
};

use cnde_ui::{Canvas, Color, Painter, Rect, Weight};
use smithay_client_toolkit::{
    reexports::{
        calloop::{
            LoopHandle,
            channel::{self, Sender},
        },
        client::protocol::{wl_shm, wl_surface::WlSurface},
    },
    seat::keyboard::{KeyEvent, Keysym, Modifiers},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
    },
};
use tracing::{info, warn};
use zbus::zvariant::{OwnedValue, Value};

use crate::App;

const AGENT_PATH: &str = "/org/cnde/PolicyKit1/AuthenticationAgent";
/// polkit 126+: the helper is started by systemd for each connection.
const HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";
/// Older polkit (and systems without systemd): a setuid helper.
/// (Arch, Alpine, Fedora, Debian.)
const HELPER_PATHS: [&str; 4] = [
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    "/usr/libexec/polkit-agent-helper-1",
    "/usr/libexec/polkit-1/polkit-agent-helper-1",
    "/usr/lib/policykit-1/polkit-agent-helper-1",
];

const WIDTH: f32 = 400.0;
const PADDING: f32 = 24.0;
const LINE: f32 = 20.0;
/// Longer messages are cut off.
const MAX_LINES: usize = 4;

pub enum Event {
    Begin(Request),
    /// polkit gave up on this one (the app went away).
    Cancel(String),
    /// The helper's verdict on a password: an error to show, or none.
    Checked {
        cookie: String,
        result: Result<(), String>,
    },
}

/// One request for a password.
pub struct Request {
    cookie: String,
    message: String,
    user: String,
    /// true: authenticated; false: dismissed.
    reply: async_channel::Sender<bool>,
}

/// The dialog showing, and requests waiting behind it.
#[derive(Default)]
pub struct Polkit {
    pub dialog: Option<Dialog>,
    queue: VecDeque<Request>,
    tx: Option<Sender<Event>>,
}

pub struct Dialog {
    pub layer: LayerSurface,
    request: Request,
    lines: Vec<String>,
    password: String,
    error: Option<String>,
    /// Waiting for the helper.
    checking: bool,
    height: f32,
    scale: i32,
    configured: bool,
    cancel: Rect,
    ok: Rect,
    hover: Option<bool>,
}

/// Register as this session's agent (on a thread: it's a few round trips
/// on the system bus). Another agent already registered wins; then we
/// stay out of the way.
pub fn start(handle: &LoopHandle<'static, App>) -> Polkit {
    let (tx, rx) = channel::channel::<Event>();
    let inserted = handle.insert_source(rx, |event, _, app: &mut App| {
        if let channel::Event::Msg(event) = event {
            app.polkit_event(event);
        }
    });
    if inserted.is_err() {
        return Polkit::default();
    }
    let agent_tx = tx.clone();
    let _ = std::thread::Builder::new()
        .name("polkit".into())
        .spawn(move || match register(agent_tx) {
            // The connection serves the agent for as long as it's open.
            Ok(_conn) => loop {
                std::thread::park();
            },
            Err(err) => warn!(%err, "not the polkit agent"),
        });
    Polkit {
        tx: Some(tx),
        ..Polkit::default()
    }
}

fn register(tx: Sender<Event>) -> zbus::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;
    conn.object_server().at(AGENT_PATH, Agent { tx })?;
    let subject = subject();
    let locale = std::env::var("LANG").unwrap_or_else(|_| "C".into());
    conn.call_method(
        Some("org.freedesktop.PolicyKit1"),
        "/org/freedesktop/PolicyKit1/Authority",
        Some("org.freedesktop.PolicyKit1.Authority"),
        "RegisterAuthenticationAgent",
        &(subject, locale, AGENT_PATH),
    )?;
    info!("polkit agent registered");
    Ok(conn)
}

/// Who we're the agent for: this login session, or failing that, us.
fn subject() -> (&'static str, HashMap<&'static str, Value<'static>>) {
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        return (
            "unix-session",
            HashMap::from([("session-id", Value::from(id))]),
        );
    }
    // The start time is the 22nd field of /proc/self/stat, counting from
    // after the command name (which may hold spaces).
    let start_time = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let rest = stat.rsplit_once(')')?.1;
            rest.split_whitespace().nth(19)?.parse::<u64>().ok()
        })
        .unwrap_or(0);
    (
        "unix-process",
        HashMap::from([
            ("pid", Value::from(std::process::id())),
            ("start-time", Value::from(start_time)),
        ]),
    )
}

struct Agent {
    tx: Sender<Event>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    async fn begin_authentication(
        &self,
        _action_id: String,
        message: String,
        _icon_name: String,
        _details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> zbus::fdo::Result<()> {
        let user = pick_user(&identities)
            .ok_or_else(|| zbus::fdo::Error::Failed("no user to authenticate as".into()))?;
        let (reply, answer) = async_channel::bounded(1);
        self.tx
            .send(Event::Begin(Request {
                cookie,
                message,
                user,
                reply,
            }))
            .map_err(|_| zbus::fdo::Error::Failed("the shell is gone".into()))?;
        match answer.recv().await {
            Ok(true) => Ok(()),
            _ => Err(zbus::fdo::Error::Failed("dismissed".into())),
        }
    }

    async fn cancel_authentication(&self, cookie: String) {
        let _ = self.tx.send(Event::Cancel(cookie));
    }
}

/// Who types the password: us if we may, else the first user polkit
/// would take (e.g. root on systems without an admin group).
fn pick_user(identities: &[(String, HashMap<String, OwnedValue>)]) -> Option<String> {
    let me = rustix::process::getuid().as_raw();
    let uids: Vec<u32> = identities
        .iter()
        .filter(|(kind, _)| kind == "unix-user")
        .filter_map(|(_, details)| u32::try_from(details.get("uid")?.try_clone().ok()?).ok())
        .collect();
    // A group (wheel) means any member: we're probably one.
    let group = identities.iter().any(|(kind, _)| kind == "unix-group");
    let uid = if uids.contains(&me) || (group && uids.is_empty()) {
        me
    } else {
        *uids.first()?
    };
    user_name(uid)
}

fn user_name(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_string())
    })
}

/// Have polkit's helper check `password` (blocks: run it on a thread).
fn check(user: &str, cookie: &str, password: &str) -> Result<(), String> {
    let (mut input, output): (Box<dyn Write>, Box<dyn std::io::Read>) =
        match UnixStream::connect(HELPER_SOCKET) {
            Ok(stream) => {
                let reader = stream.try_clone().map_err(|err| err.to_string())?;
                let mut writer = stream;
                writeln!(writer, "{user}\n{cookie}").map_err(|err| err.to_string())?;
                (Box::new(writer), Box::new(reader))
            }
            Err(_) => {
                let helper = HELPER_PATHS
                    .iter()
                    .find(|path| std::path::Path::new(path).exists())
                    .ok_or("polkit's helper isn't installed")?;
                let mut child = Command::new(helper)
                    .arg(user)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|err| err.to_string())?;
                let mut stdin = child.stdin.take().ok_or("no helper input")?;
                let stdout = child.stdout.take().ok_or("no helper output")?;
                writeln!(stdin, "{cookie}").map_err(|err| err.to_string())?;
                // Reaped once it's said its piece.
                std::thread::spawn(move || child.wait());
                (Box::new(stdin), Box::new(stdout))
            }
        };
    let mut answered = false;
    let mut said = None;
    for line in BufReader::new(output).lines() {
        let line = line.map_err(|err| err.to_string())?;
        let (tag, text) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match tag {
            "PAM_PROMPT_ECHO_OFF" | "PAM_PROMPT_ECHO_ON" => {
                // Only the password is known; a second question (a one-time
                // code) gets an empty answer and fails.
                let answer = if answered { "" } else { password };
                answered = true;
                writeln!(input, "{answer}").map_err(|err| err.to_string())?;
                input.flush().map_err(|err| err.to_string())?;
            }
            "PAM_ERROR_MSG" => said = Some(unescape(text)),
            "SUCCESS" => return Ok(()),
            "FAILURE" => {
                return Err(said.unwrap_or_else(|| "Wrong password, try again".into()));
            }
            _ => {}
        }
    }
    Err("polkit's helper stopped".into())
}

/// The helper escapes its messages C-style.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push(' '),
            Some('t') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out.trim().to_string()
}

/// `text` in lines no wider than `width`, at most `max_lines` (the last
/// one ends in an ellipsis if there was more). Line breaks are kept.
pub fn wrap(p: &mut Painter, text: &str, width: f32, size: f32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let tried = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && p.measure(&tried, size, Weight::Normal) > width {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            } else {
                line = tried;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines
}

impl Dialog {
    pub fn owns(&self, surface: &WlSurface) -> bool {
        self.layer.wl_surface() == surface
    }
}

impl App {
    pub fn polkit_event(&mut self, event: Event) {
        match event {
            Event::Begin(request) => {
                self.polkit.queue.push_back(request);
                if self.polkit.dialog.is_none() {
                    self.polkit_next();
                }
            }
            Event::Cancel(cookie) => {
                self.polkit.queue.retain(|r| r.cookie != cookie);
                if self
                    .polkit
                    .dialog
                    .as_ref()
                    .is_some_and(|d| d.request.cookie == cookie)
                {
                    self.polkit_close(false);
                }
            }
            Event::Checked { cookie, result } => {
                let Some(dialog) = &mut self.polkit.dialog else {
                    return;
                };
                if dialog.request.cookie != cookie {
                    return;
                }
                match result {
                    Ok(()) => self.polkit_close(true),
                    Err(err) => {
                        dialog.checking = false;
                        dialog.error = Some(err);
                        self.draw_polkit();
                    }
                }
            }
        }
    }

    /// Show the next request waiting, if any.
    fn polkit_next(&mut self) {
        let Some(request) = self.polkit.queue.pop_front() else {
            return;
        };
        let Some(panel) = self.panels.first() else {
            let _ = request.reply.try_send(false);
            return;
        };
        let (output, scale) = (panel.output.clone(), panel.scale());
        // Measured once: the message doesn't change.
        let lines = {
            let mut pixels = [0u8; 4];
            let Some(mut canvas) = Canvas::new(&mut pixels, 1, 1, 1.0) else {
                return;
            };
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: &self.theme,
            };
            wrap(
                &mut p,
                &request.message,
                WIDTH - PADDING * 2.0,
                self.theme.font_size,
                MAX_LINES,
            )
        };
        let t = &self.theme;
        // Title, message, label, entry, error line, buttons.
        let height = PADDING
            + 32.0
            + 12.0
            + lines.len() as f32 * LINE
            + 16.0
            + LINE
            + 6.0
            + t.button_height
            + LINE
            + 8.0
            + t.button_height
            + PADDING;
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("cnde-polkit"),
            Some(&output),
        );
        layer.set_anchor(Anchor::empty());
        layer.set_size(WIDTH as u32, height.ceil() as u32);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();
        self.polkit.dialog = Some(Dialog {
            layer,
            request,
            lines,
            password: String::new(),
            error: None,
            checking: false,
            height: height.ceil(),
            scale,
            configured: false,
            cancel: Rect::new(0.0, 0.0, 0.0, 0.0),
            ok: Rect::new(0.0, 0.0, 0.0, 0.0),
            hover: None,
        });
    }

    /// Answer polkit and take the dialog down.
    fn polkit_close(&mut self, authenticated: bool) {
        if let Some(dialog) = self.polkit.dialog.take() {
            let _ = dialog.request.reply.try_send(authenticated);
            let mut password = dialog.password.into_bytes();
            password.fill(0);
        }
        self.polkit_next();
    }

    /// The compositor closed it: same as Cancel.
    pub fn polkit_closed(&mut self, surface: &WlSurface) {
        if self.polkit.dialog.as_ref().is_some_and(|d| d.owns(surface)) {
            self.polkit_close(false);
        }
    }

    pub fn polkit_configured(&mut self, surface: &WlSurface) {
        if let Some(dialog) = &mut self.polkit.dialog
            && dialog.owns(surface)
        {
            dialog.configured = true;
            self.draw_polkit();
        }
    }

    fn polkit_submit(&mut self) {
        let Some(dialog) = &mut self.polkit.dialog else {
            return;
        };
        if dialog.checking || dialog.password.is_empty() {
            return;
        }
        let Some(tx) = self.polkit.tx.clone() else {
            return;
        };
        let Some(dialog) = &mut self.polkit.dialog else {
            return;
        };
        dialog.checking = true;
        dialog.error = None;
        let user = dialog.request.user.clone();
        let cookie = dialog.request.cookie.clone();
        let mut password = std::mem::take(&mut dialog.password);
        let _ = std::thread::Builder::new()
            .name("polkit-check".into())
            .spawn(move || {
                let result = check(&user, &cookie, &password);
                let mut bytes = std::mem::take(&mut password).into_bytes();
                bytes.fill(0);
                let _ = tx.send(Event::Checked { cookie, result });
            });
        self.draw_polkit();
    }

    /// Typing goes to the dialog while it's up. Returns false if it isn't.
    pub fn polkit_key(&mut self, event: &KeyEvent, modifiers: &Modifiers) -> bool {
        let Some(dialog) = &mut self.polkit.dialog else {
            return false;
        };
        match event.keysym {
            Keysym::Escape => {
                self.polkit_close(false);
                return true;
            }
            Keysym::Return | Keysym::KP_Enter => {
                self.polkit_submit();
                return true;
            }
            _ if dialog.checking => return true,
            Keysym::BackSpace if modifiers.ctrl => dialog.password.clear(),
            Keysym::BackSpace => {
                dialog.password.pop();
            }
            Keysym::u | Keysym::U if modifiers.ctrl => dialog.password.clear(),
            _ if modifiers.ctrl || modifiers.alt || modifiers.logo => return true,
            _ => match &event.utf8 {
                Some(text) => dialog
                    .password
                    .extend(text.chars().filter(|c| !c.is_control())),
                None => return true,
            },
        }
        dialog.error = None;
        self.draw_polkit();
        true
    }

    /// Pointer motion (`press` false) or a click on the dialog.
    pub fn polkit_pointer(&mut self, position: (f64, f64), press: bool) {
        let Some(dialog) = &mut self.polkit.dialog else {
            return;
        };
        let hover = if dialog.ok.contains(position) {
            Some(true)
        } else if dialog.cancel.contains(position) {
            Some(false)
        } else {
            None
        };
        if press {
            match hover {
                Some(true) => self.polkit_submit(),
                Some(false) => self.polkit_close(false),
                None => {}
            }
        } else if hover != dialog.hover {
            dialog.hover = hover;
            self.draw_polkit();
        }
    }

    fn draw_polkit(&mut self) {
        let Some(dialog) = &mut self.polkit.dialog else {
            return;
        };
        if !dialog.configured {
            return;
        }
        let scale = dialog.scale;
        let (lw, lh) = (WIDTH, dialog.height);
        let (w, h) = (lw as i32 * scale, lh as i32 * scale);
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate the password dialog");
                return;
            }
        };
        if let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) {
            let t = &self.theme;
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            canvas.rounded_rect(0.0, 0.0, lw, lh, t.qs_radius, t.qs_background);
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: t,
            };
            let inner = lw - PADDING * 2.0;
            let mut y = PADDING;
            p.icon(
                "dialog-password-symbolic",
                PADDING + 14.0,
                y + 16.0,
                28.0,
                t.popup_text,
            );
            p.label_v(
                "Authentication Required",
                PADDING + 40.0,
                y + 16.0,
                inner - 40.0,
                t.font_size + 2.0,
                Weight::Bold,
                t.popup_text,
            );
            y += 32.0 + 12.0;
            for line in &dialog.lines {
                p.label_v(
                    line,
                    PADDING,
                    y + LINE / 2.0,
                    inner,
                    t.font_size,
                    Weight::Normal,
                    t.popup_text,
                );
                y += LINE;
            }
            y += 16.0;
            p.label_v(
                &format!("Password for {}", dialog.request.user),
                PADDING,
                y + LINE / 2.0,
                inner,
                t.font_size - 1.0,
                Weight::Normal,
                t.qs_dim,
            );
            y += LINE + 6.0;
            let dots: String = "•".repeat(dialog.password.chars().count());
            let cursor = (!dialog.checking).then_some(dots.chars().count());
            p.entry(
                Rect::new(PADDING, y, inner, t.button_height),
                &dots,
                "",
                cursor,
                dialog.error.is_some(),
            );
            y += t.button_height;
            let note = if dialog.checking {
                Some(("Checking…", t.qs_dim))
            } else {
                dialog.error.as_deref().map(|e| (e, t.error))
            };
            if let Some((text, color)) = note {
                p.label_v(
                    text,
                    PADDING,
                    y + LINE / 2.0 + 2.0,
                    inner,
                    t.font_size - 1.0,
                    Weight::Normal,
                    color,
                );
            }
            y += LINE + 8.0;
            // Buttons on the right: Cancel, then Authenticate.
            let bh = t.button_height;
            let ok_w = p.measure("Authenticate", t.font_size, Weight::Normal) + 32.0;
            let cancel_w = p.measure("Cancel", t.font_size, Weight::Normal) + 32.0;
            let ok = Rect::new(lw - PADDING - ok_w, y, ok_w, bh);
            let cancel = Rect::new(ok.x - 8.0 - cancel_w, y, cancel_w, bh);
            p.button(
                cancel,
                if dialog.hover == Some(false) {
                    1.0
                } else {
                    0.0
                },
            );
            let fill = if dialog.hover == Some(true) {
                t.accent.mix(Color::rgb(0xff, 0xff, 0xff), 0.12)
            } else {
                t.accent
            };
            p.canvas
                .rounded_rect(ok.x, ok.y, ok.w, ok.h, t.button_radius, fill);
            for (rect, label, color) in [
                (cancel, "Cancel", t.popup_text),
                (ok, "Authenticate", t.on_accent),
            ] {
                let tw = p.measure(label, t.font_size, Weight::Normal);
                p.label_v(
                    label,
                    rect.x + (rect.w - tw) / 2.0,
                    rect.y + rect.h / 2.0,
                    tw + 1.0,
                    t.font_size,
                    Weight::Normal,
                    color,
                );
            }
            dialog.ok = ok;
            dialog.cancel = cancel;
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = dialog.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_messages_are_unescaped() {
        assert_eq!(
            unescape("Authentication failure\\n"),
            "Authentication failure"
        );
        assert_eq!(unescape("say \\\"hi\\\""), "say \"hi\"");
    }

    #[test]
    fn users_come_from_passwd() {
        assert_eq!(user_name(0).as_deref(), Some("root"));
    }

    #[test]
    fn we_answer_for_ourselves_when_allowed() {
        let me = rustix::process::getuid().as_raw();
        let user = |uid: u32| {
            (
                "unix-user".to_string(),
                HashMap::from([("uid".to_string(), OwnedValue::from(uid))]),
            )
        };
        assert_eq!(pick_user(&[user(0), user(me)]), user_name(me));
        // Not among them: the first one polkit would take.
        if me != 0 {
            assert_eq!(pick_user(&[user(0)]).as_deref(), Some("root"));
        }
        let wheel = (
            "unix-group".to_string(),
            HashMap::from([("gid".to_string(), OwnedValue::from(998u32))]),
        );
        assert_eq!(pick_user(&[wheel]), user_name(me));
    }
}
