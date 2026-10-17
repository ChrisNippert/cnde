//! cnde-compositor: the display server, window manager and renderer.
//! See ARCHITECTURE.md §4.

mod animation;
mod audio;
mod backend;
mod capture;
mod config;
mod crossfade;
mod cursor;
mod decorations;
mod effects;
mod ext_workspace;
mod frost;
mod grabs;
mod handlers;
mod input;
mod ipc;
mod keybinds;
mod layers;
mod lock;
mod overview;
mod popup_anim;
mod portals;
mod render;
mod schedule;
mod screenshot;
mod shaders;
mod snap;
mod snapshot;
mod spawn;
mod state;
mod wallpaper;
mod window;
mod workspaces;
mod xwayland;

use smithay::backend::renderer::ImportMemWl;
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::info;

use crate::{backend::Backend, keybinds::ModKey, state::State};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Nested when started inside another desktop, real hardware from a TTY.
    // `--nested` / `--udev` override the guess.
    let nested = if args.iter().any(|a| a == "--udev") {
        false
    } else {
        args.iter().any(|a| a == "--nested")
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DISPLAY").is_some()
    };
    if !nested {
        set_session_environment();
    }
    init_logging();

    // Wayland's socket lives here. Without elogind/logind setting it (e.g.
    // Alpine with seatd), it has to come from the login shell.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return Err(
            "XDG_RUNTIME_DIR is not set. Without elogind, set it in your shell \
                    profile, e.g. export XDG_RUNTIME_DIR=/tmp/$(id -u)-runtime-dir \
                    and create it with mode 0700"
                .into(),
        );
    }

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    // Before anything is launched: exports CNDE_SOCKET.
    let ipc_socket = ipc::listen(&event_loop.handle());
    let display: Display<State> = Display::new()?;
    let (config, config_path) = config::load_initial();

    let mut state = if nested {
        let (winit, events) = backend::winit::init(&display.handle())?;
        let output = winit.output.clone();
        let mut state = State::new(
            &mut event_loop,
            display,
            Backend::Winit(winit),
            // The host desktop grabs Super, so nested bindings use Alt.
            ModKey::Alt,
            "seat0",
        )?;
        state.config = config;
        state.config_path = config_path;
        state.space.map_output(&output, (0, 0));
        // Offer every shm format the renderer can import, not just the two
        // mandatory ones (the udev backend does the same per GPU).
        if let Backend::Winit(winit) = &mut state.backend {
            let formats = winit.renderer().shm_formats();
            state.shm_state.update_formats(formats);
        }
        backend::winit::insert_event_source(&event_loop.handle(), events)?;
        state
    } else {
        let (udev, session_events) = backend::udev::create()?;
        let seat_name = udev.seat_name();
        let mut state = State::new(
            &mut event_loop,
            display,
            Backend::Udev(udev),
            ModKey::Super,
            &seat_name,
        )?;
        state.config = config;
        state.config_path = config_path;
        backend::udev::init(&mut state, &mut event_loop, session_events)?;
        state
    };
    state.apply_config(None);
    if let Some(path) = state.config_path.clone() {
        config::watch(&event_loop.handle(), path);
    }
    schedule::start(&event_loop.handle());
    // Before autostart, so launched apps get DISPLAY.
    xwayland::setup(&mut state, &event_loop.handle());
    if !nested {
        portals::setup(&state.client_env());
        if state.config.autostart.audio {
            audio::ensure_running();
        }
    }
    if state.config.panel.enabled {
        spawn::supervise_shell(&state.client_env(), &state.config.autostart.shell);
    }
    for command in &state.config.autostart.commands {
        spawn::spawn_command_line(command, &state.client_env());
    }

    info!(
        socket = ?state.socket_name,
        nested,
        "compositor running; {:?}+Q opens a terminal, Ctrl+Alt+Backspace quits",
        state.mod_key,
    );

    // `-c <command>` launches a client at startup (handy for testing).
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "-c" | "--command")
            && let Some(command) = args.next()
        {
            spawn::spawn_command_line(&command, &state.client_env());
        }
    }

    // Draw the first frames before blocking for events.
    state.on_loop_iteration();
    let result = event_loop.run(None, &mut state, State::on_loop_iteration);
    if let Some(path) = ipc_socket {
        let _ = std::fs::remove_file(path);
    }
    result?;
    Ok(())
}

/// Environment for a real session, inherited by every client we launch.
fn set_session_environment() {
    // We *are* the session, so these always win. A TTY login sets
    // XDG_SESSION_TYPE=tty, and Electron 38+ / Chromium pick X11 vs Wayland
    // from it (Discord failed with "Missing X server" because of that).
    let session = [
        ("XDG_SESSION_TYPE", "wayland"),
        ("XDG_CURRENT_DESKTOP", "cnde"),
    ];
    // Defaults only: anything the user already set wins.
    let defaults = [
        ("XCURSOR_THEME", cursor::DEFAULT_THEME),
        ("XCURSOR_SIZE", "24"),
        // No XWayland yet (Milestone 4): run older Electron apps (< 38, which
        // ignore XDG_SESSION_TYPE) and Qt apps natively on Wayland, with X11
        // as Qt's fallback once it exists.
        ("ELECTRON_OZONE_PLATFORM_HINT", "auto"),
        ("QT_QPA_PLATFORM", "wayland;xcb"),
    ];
    let unset = |key: &str| std::env::var_os(key).is_none();
    for (key, value) in session
        .into_iter()
        .chain(defaults.into_iter().filter(|(key, _)| unset(key)))
    {
        // Safety: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var(key, value) };
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
