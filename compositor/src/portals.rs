//! Making screen sharing work for apps (ARCHITECTURE.md §6): apps ask
//! xdg-desktop-portal, which hands screen capture to xdg-desktop-portal-wlr,
//! which captures through our `ext-image-copy-capture` (see `capture`).
//!
//! Two things have to be true for that chain to find us:
//! - The portal must know to use the wlr backend for a "cnde" desktop: a
//!   `cnde-portals.conf`, and xdg-desktop-portal-wlr's own settings (which
//!   picker to show). Both are installed for the user if missing.
//! - D-Bus starts the portals on demand, with the environment the bus
//!   knows, not ours. So the bus has to be told our WAYLAND_DISPLAY etc.
//!   (dbus-update-activation-environment), or the portal can't connect.
//!
//! Only for real sessions: a nested cnde must not touch the bus of the
//! desktop it's running inside.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use tracing::{info, warn};

use crate::spawn::ClientEnv;

const PORTALS_CONF: &str = include_str!("../../data/cnde-portals.conf");
const WLR_PORTAL_CONF: &str = include_str!("../../data/xdg-desktop-portal-wlr.ini");

pub fn setup(env: &ClientEnv) {
    let env = env.clone();
    // Writes files and runs a helper: off the compositor thread.
    let _ = thread::Builder::new()
        .name("portals".into())
        .spawn(move || {
            if let Some(config) = config_home() {
                install_if_missing(
                    &config.join("xdg-desktop-portal").join("cnde-portals.conf"),
                    PORTALS_CONF,
                );
                install_if_missing(
                    &config.join("xdg-desktop-portal-wlr").join("cnde"),
                    WLR_PORTAL_CONF,
                );
            }
            update_activation_environment(&env);
        });
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

fn install_if_missing(path: &Path, contents: &str) {
    if path.exists() {
        return;
    }
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, contents));
    match result {
        Ok(()) => info!(path = %path.display(), "installed portal config"),
        Err(err) => warn!(path = %path.display(), %err, "can't install portal config"),
    }
}

/// Tell the session bus (and systemd, if the bus is systemd's per-user
/// one) the environment apps and portals need to reach us.
fn update_activation_environment(env: &ClientEnv) {
    let mut command = Command::new("dbus-update-activation-environment");
    if on_systemd_user_bus() {
        command.arg("--systemd");
    }
    command
        .args([
            "WAYLAND_DISPLAY",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_TYPE",
            cnde_ipc::SOCKET_ENV,
        ])
        .env("WAYLAND_DISPLAY", &env.wayland_display)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(display) = &env.x11_display {
        command.arg("DISPLAY").env("DISPLAY", display);
    }
    match command.status() {
        Ok(status) if status.success() => info!("exported the session environment to D-Bus"),
        Ok(status) => {
            warn!(%status, "dbus-update-activation-environment failed; screen sharing may not work")
        }
        Err(err) => {
            warn!(%err, "can't run dbus-update-activation-environment; screen sharing may not work")
        }
    }
}

/// Whether we're on systemd's shared per-user bus rather than a private one
/// (`dbus-run-session`). On the shared bus, the portals are systemd services
/// shared with any other session of this user.
fn on_systemd_user_bus() -> bool {
    let (Some(address), Some(runtime)) = (
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    ) else {
        // No address: clients use the default, which is the systemd one.
        return std::env::var_os("XDG_RUNTIME_DIR").is_some();
    };
    let default = format!("unix:path={}/bus", Path::new(&runtime).display());
    address.to_string_lossy() == default
}
