//! cnde-compositor: the display server, window manager and renderer.
//! See ARCHITECTURE.md §4.

mod animation;
mod backend;
mod grabs;
mod handlers;
mod input;
mod keybinds;
mod render;
mod spawn;
mod state;
mod window;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::{error, info};

use crate::state::State;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        error!(
            "no WAYLAND_DISPLAY or DISPLAY: the udev (real hardware) backend arrives in Milestone 2"
        );
        return Err("udev backend not implemented yet".into());
    }

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;

    let (backend, winit_events) = backend::winit::init(&display.handle())?;
    let mut state = State::new(&mut event_loop, display, backend)?;
    backend::winit::insert_event_source(&event_loop.handle(), winit_events)?;

    info!(
        socket = ?state.socket_name,
        modifier = ?state.mod_key,
        "compositor running nested; press {:?}+Q for a terminal, Ctrl+Alt+Backspace to quit",
        state.mod_key,
    );

    // `-c <command>` launches a client at startup (handy for testing).
    let mut args = std::env::args().skip(1);
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

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
