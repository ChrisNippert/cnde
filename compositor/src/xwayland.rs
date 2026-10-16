// Adapted from niri (GPL-3.0-or-later): src/utils/xwayland/mod.rs,
// src/utils/xwayland/satellite.rs (display/lock/socket setup, on-demand
// activation via -listenfd).

//! X11 apps via xwayland-satellite (ARCHITECTURE.md §7), started on demand:
//! we own the X11 sockets and set DISPLAY for clients, and only when the
//! first X11 client connects do we start satellite (and with it Xwayland),
//! handing it the listening sockets. No X11 app, no X server in memory.

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::{
        fd::{AsRawFd, BorrowedFd},
        linux::net::SocketAddrExt,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::{SocketAddr, UnixListener},
            process::CommandExt,
        },
    },
    path::PathBuf,
    process::{Command, Stdio},
    thread,
};

use rustix::io::{FdFlags, fcntl_setfd};
use smithay::reexports::calloop::{
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
    channel::{self, Event, Sender},
    generic::Generic,
};
use tracing::{debug, info, warn};

use crate::state::State;

const BINARY: &str = "xwayland-satellite";
const X11_UNIX_DIR: &str = "/tmp/.X11-unix";
/// How many display numbers to try before giving up.
const MAX_DISPLAYS: u32 = 50;

pub struct Xwayland {
    display: u32,
    unix: UnixListener,
    /// Linux abstract socket; most X11 clients try this first.
    abstract_socket: UnixListener,
    /// (socket index, watch) for each socket being watched.
    watches: Vec<(usize, RegistrationToken)>,
    /// The spawner thread tells the main loop to re-arm when satellite exits.
    rearm: Sender<()>,
    _lock: Unlink,
    _socket: Unlink,
}

impl Xwayland {
    pub fn display_name(&self) -> String {
        format!(":{}", self.display)
    }
}

/// Removes a file when dropped (lock file, socket path).
struct Unlink(PathBuf);

impl Drop for Unlink {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Claim a display and start listening. A missing or too-old
/// xwayland-satellite just means no X11 support; the session runs regardless.
pub fn setup(state: &mut State, handle: &LoopHandle<'static, State>) {
    if !satellite_supports_on_demand() {
        info!("{BINARY} not found (or too old for -listenfd); X11 apps won't run");
        return;
    }
    let (rearm, rearm_events) = channel::channel();
    if let Err(err) = handle.insert_source(rearm_events, |event, _, state| {
        if let Event::Msg(()) = event {
            state.arm_xwayland_watch();
        }
    }) {
        warn!(?err, "can't set up XWayland");
        return;
    }
    match open_display(rearm) {
        Ok(xwayland) => {
            info!(display = %xwayland.display_name(), "XWayland ready (starts on first X11 client)");
            state.xwayland = Some(xwayland);
            state.arm_xwayland_watch();
        }
        Err(err) => warn!(%err, "can't set up XWayland"),
    }
}

fn satellite_supports_on_demand() -> bool {
    Command::new(BINARY)
        .args([":0", "--test-listenfd-support"])
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn open_display(rearm: Sender<()>) -> Result<Xwayland, String> {
    ensure_x11_unix_dir()?;
    for display in 0..MAX_DISPLAYS {
        // The lock file is how X servers claim a display number; create_new
        // makes the claim atomic. Taken (by the host desktop's Xwayland,
        // say): try the next one.
        let lock_path = PathBuf::from(format!("/tmp/.X{display}-lock"));
        let Ok(mut lock) = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&lock_path)
        else {
            continue;
        };
        let lock_guard = Unlink(lock_path);
        // X11 convention: the PID, right-aligned in 10 columns.
        if writeln!(lock, "{:>10}", std::process::id()).is_err() {
            continue;
        }

        let socket_path = PathBuf::from(format!("{X11_UNIX_DIR}/X{display}"));
        let _ = fs::remove_file(&socket_path); // stale, from a crashed server
        let Ok(unix) = UnixListener::bind(&socket_path) else {
            continue;
        };
        let socket_guard = Unlink(socket_path.clone());
        let abstract_socket =
            SocketAddr::from_abstract_name(socket_path.as_os_str().as_encoded_bytes())
                .and_then(|addr| UnixListener::bind_addr(&addr));
        let Ok(abstract_socket) = abstract_socket else {
            // Someone else's X server owns this number without a lock file.
            continue;
        };

        return Ok(Xwayland {
            display,
            unix,
            abstract_socket,
            watches: Vec::new(),
            rearm,
            _lock: lock_guard,
            _socket: socket_guard,
        });
    }
    Err(format!(
        "no free X11 display among :0..:{}",
        MAX_DISPLAYS - 1
    ))
}

/// /tmp/.X11-unix must be world-writable and sticky, like /tmp.
fn ensure_x11_unix_dir() -> Result<(), String> {
    match fs::create_dir(X11_UNIX_DIR) {
        Ok(()) => fs::set_permissions(X11_UNIX_DIR, fs::Permissions::from_mode(0o1777))
            .map_err(|err| format!("can't set permissions on {X11_UNIX_DIR}: {err}")),
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            let mode = fs::metadata(X11_UNIX_DIR)
                .map_err(|err| format!("can't stat {X11_UNIX_DIR}: {err}"))?
                .permissions()
                .mode();
            if mode & 0o1022 == 0o1022 {
                Ok(())
            } else {
                Err(format!("{X11_UNIX_DIR} has unsafe permissions {mode:o}"))
            }
        }
        Err(err) => Err(format!("can't create {X11_UNIX_DIR}: {err}")),
    }
}

/// Accept and drop anything queued, so a socket left readable by a failed
/// satellite doesn't immediately fire the watch again in a loop.
fn drain_pending(listener: &UnixListener) {
    if listener.set_nonblocking(true).is_ok() {
        while listener.accept().is_ok() {}
        let _ = listener.set_nonblocking(false);
    }
}

impl State {
    /// Wait for the first X11 connection on either socket.
    fn arm_xwayland_watch(&mut self) {
        let handle = self.loop_handle.clone();
        let Some(xwayland) = &mut self.xwayland else {
            return;
        };
        for (_, token) in xwayland.watches.drain(..) {
            handle.remove(token);
        }
        for (index, listener) in [&xwayland.unix, &xwayland.abstract_socket]
            .into_iter()
            .enumerate()
        {
            drain_pending(listener);
            let Ok(clone) = listener.try_clone() else {
                warn!("can't watch the X11 socket");
                continue;
            };
            let source = Generic::new(clone, Interest::READ, Mode::Level);
            match handle.insert_source(source, move |_, _, state| {
                state.start_satellite(index);
                // The other watch is removed in start_satellite; a source
                // can't remove itself from inside its own callback.
                Ok(PostAction::Remove)
            }) {
                Ok(token) => xwayland.watches.push((index, token)),
                Err(err) => warn!(?err, "can't watch the X11 socket"),
            }
        }
    }

    /// An X11 client is knocking: start satellite with our sockets. It
    /// accepts that client and every later one until it exits.
    fn start_satellite(&mut self, fired: usize) {
        let wayland_display = self.socket_name.clone();
        let handle = self.loop_handle.clone();
        let Some(xwayland) = &mut self.xwayland else {
            return;
        };
        // Stop watching: satellite owns the sockets now. Removing the source
        // that's currently firing is left to its own PostAction::Remove.
        let watches = std::mem::take(&mut xwayland.watches);
        let fds = (
            xwayland.unix.try_clone(),
            xwayland.abstract_socket.try_clone(),
        );
        let (Ok(unix), Ok(abstract_socket)) = fds else {
            warn!("can't hand the X11 sockets to {BINARY}");
            let _ = xwayland.rearm.send(());
            return;
        };
        for (index, token) in watches {
            if index != fired {
                handle.remove(token);
            }
        }

        let x11_display = xwayland.display_name();
        let rearm = xwayland.rearm.clone();
        info!(display = %x11_display, "X11 client connected; starting {BINARY}");
        let spawned = thread::Builder::new()
            .name("xwayland-satellite".into())
            .spawn(move || {
                let (unix_fd, abstract_fd) = (unix.as_raw_fd(), abstract_socket.as_raw_fd());
                let mut command = Command::new(BINARY);
                command
                    .arg(&x11_display)
                    .args(["-listenfd", &unix_fd.to_string()])
                    .args(["-listenfd", &abstract_fd.to_string()])
                    .env("WAYLAND_DISPLAY", &wayland_display)
                    .env_remove("DISPLAY")
                    .env_remove("RUST_BACKTRACE")
                    .stdin(Stdio::null());
                // Safety: only async-signal-safe fcntl calls between fork and exec.
                unsafe {
                    command.pre_exec(move || {
                        // Our clones are close-on-exec; let these two through.
                        for fd in [unix_fd, abstract_fd] {
                            fcntl_setfd(BorrowedFd::borrow_raw(fd), FdFlags::empty())?;
                        }
                        Ok(())
                    });
                }
                match command.spawn() {
                    Ok(mut child) => {
                        drop((unix, abstract_socket));
                        let status = child.wait();
                        debug!(?status, "{BINARY} exited");
                    }
                    Err(err) => warn!(?err, "failed to start {BINARY}"),
                }
                // Ready for the next X11 client.
                let _ = rearm.send(());
            });
        if let Err(err) = spawned {
            warn!(?err, "can't start {BINARY}");
            let _ = xwayland.rearm.send(());
        }
    }
}
