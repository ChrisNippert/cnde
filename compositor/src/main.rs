//! cnde-compositor: the display server, window manager and renderer.
//! See ARCHITECTURE.md §4.

mod animation;
mod backend;
mod config;
mod cursor;
mod effects;
mod grabs;
mod handlers;
mod input;
mod keybinds;
mod layers;
mod render;
mod snap;
mod snapshot;
mod spawn;
mod state;
mod window;

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

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
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
    for command in &state.config.autostart.commands {
        spawn::spawn_command_line(command, &state.socket_name);
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
            spawn::spawn_command_line(&command, &state.socket_name);
        }
    }

    event_loop.run(None, &mut state, State::on_loop_iteration)?;
    Ok(())
}

/// Environment for a real session, inherited by every client we launch.
/// Only the defaults: anything the user already set wins.
fn set_session_environment() {
    let defaults = [
        ("XDG_SESSION_TYPE", "wayland"),
        ("XDG_CURRENT_DESKTOP", "cnde"),
        ("XCURSOR_THEME", cursor::DEFAULT_THEME),
        ("XCURSOR_SIZE", "24"),
    ];
    for (key, value) in defaults {
        if std::env::var_os(key).is_none() {
            // Safety: called first thing in main, before any other thread exists.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
