//! Sound needs PipeWire running in the session. With systemd, its user
//! units start it (socket activation) as soon as anything connects; without
//! systemd (Alpine with elogind or seatd) nothing does, so apps stay silent
//! even with pipewire and pipewire-pulse installed. Then we start it.
//!
//! Only for real sessions (a nested cnde uses its host's sound), and only
//! if no PipeWire socket exists yet (`autostart.audio` turns it off).

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use tracing::{info, warn};

/// What Alpine ships to start pipewire, wireplumber and pipewire-pulse
/// together.
const LAUNCHER: &str = "/usr/libexec/pipewire-launcher";

pub fn ensure_running() {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return;
    };
    // Something answering there: running, or systemd starts it on first
    // use. (A runtime dir from an earlier login can hold a dead socket.)
    if std::os::unix::net::UnixStream::connect(runtime.join("pipewire-0")).is_ok() {
        return;
    }
    let _ = thread::Builder::new().name("audio".into()).spawn(move || {
        if Path::new(LAUNCHER).is_file() {
            info!("starting PipeWire ({LAUNCHER})");
            start(&[LAUNCHER]);
            return;
        }
        // Elsewhere: the daemons one by one, whichever are installed.
        let Some(pipewire) = find("pipewire") else {
            info!("PipeWire isn't installed; no sound");
            return;
        };
        info!("starting PipeWire");
        start(&[&pipewire]);
        // The session manager routes streams to devices; without it
        // there's still no sound.
        if let Some(manager) = find("wireplumber").or_else(|| find("pipewire-media-session")) {
            start(&[&manager]);
        } else {
            warn!("no wireplumber; PipeWire won't route sound anywhere");
        }
        if let Some(pulse) = find("pipewire-pulse") {
            start(&[&pulse]);
        }
    });
}

/// Run `argv` in the background for the rest of the session.
fn start(argv: &[&str]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reaped on its own thread so it never lingers as a zombie.
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(err) => warn!(program, %err, "can't start"),
    }
}

/// `name` on PATH.
fn find(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|exe| exe.is_file())
        .map(|exe| exe.to_string_lossy().into_owned())
}
