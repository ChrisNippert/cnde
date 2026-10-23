//! Making screen sharing work for apps (ARCHITECTURE.md §6): apps ask
//! xdg-desktop-portal, which hands screen capture to xdg-desktop-portal-wlr,
//! which captures through our `ext-image-copy-capture` (see `capture`).
//!
//! Three things have to be true for that chain to find us:
//! - The portal must know to use the wlr backend for a "cnde" desktop: a
//!   `cnde-portals.conf`, and xdg-desktop-portal-wlr's own settings (which
//!   picker to show: `cnde-share-picker`). Both are installed for the user
//!   if missing; a wlr config the user wrote themselves always wins.
//! - D-Bus starts the portals on demand, with the environment the bus
//!   knows, not ours. So the bus has to be told our WAYLAND_DISPLAY etc.
//!   (dbus-update-activation-environment), or the portal can't connect.
//! - Portals left over from the previous session still point at its dead
//!   WAYLAND_DISPLAY (and report no screens to share), so once the bus
//!   knows ours, the portal services are restarted to bind to this one.
//!
//! Only for real sessions: a nested cnde must not touch the bus of the
//! desktop it's running inside.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use cnde_config::SharePicker;
use tracing::{info, warn};

use crate::spawn::ClientEnv;

const PORTALS_CONF: &str = include_str!("../../data/cnde-portals.conf");
const WLR_PORTAL_CONF: &str = include_str!("../../data/xdg-desktop-portal-wlr.ini");
/// What older versions installed (fuzzel as the picker), so an unedited
/// copy can be upgraded.
const LEGACY_WLR_PORTAL_CONF: &str = "# xdg-desktop-portal-wlr settings for cnde (installed to
# $XDG_CONFIG_HOME/xdg-desktop-portal-wlr/cnde on first run if missing).
#
# When an app asks to share the screen, a menu lists every monitor and
# window to pick from. fuzzel stands in until cnde-shell has its own picker.
[screencast]
chooser_type=dmenu
chooser_cmd=fuzzel --dmenu --prompt \"Share: \" --width 50
max_fps=60
";
const PICKER: &str = "cnde-share-picker";
/// The portal services to start afresh at login, backends first.
const PORTAL_UNITS: [&str; 3] = [
    "xdg-desktop-portal-wlr.service",
    "xdg-desktop-portal-gtk.service",
    "xdg-desktop-portal.service",
];

pub fn setup(env: &ClientEnv, share_picker: SharePicker) {
    let env = env.clone();
    // Writes files and runs helpers: off the compositor thread.
    let _ = thread::Builder::new()
        .name("portals".into())
        .spawn(move || {
            if let Some(config) = config_home() {
                install_if_missing(
                    &config.join("xdg-desktop-portal").join("cnde-portals.conf"),
                    PORTALS_CONF,
                );
                install_wlr_config(&config.join("xdg-desktop-portal-wlr"), share_picker);
            }
            register_file_manager();
            if update_activation_environment(&env) {
                restart_portals();
            }
        });
}

const FILE_MANAGER_SERVICE: &str = "org.freedesktop.FileManager1.service";

/// Where the session bus looks first for services to start on demand
/// (before ~/.local/share and /usr/share); gone when the user's last
/// session ends.
fn runtime_service_file() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    Some(Path::new(&runtime).join("dbus-1/services").join(FILE_MANAGER_SERVICE))
}

/// "Show in folder" from apps calls org.freedesktop.FileManager1, which the
/// bus starts from whichever file manager installed a service file (often
/// Nautilus). Point it at cnde-files for this session instead, so it starts
/// only when asked. A service file the user made in ~/.local/share wins.
fn register_file_manager() {
    let Some(path) = runtime_service_file() else {
        return;
    };
    let users_own = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .is_some_and(|data| data.join("dbus-1/services").join(FILE_MANAGER_SERVICE).exists());
    let Some(files) = find_binary("cnde-files").filter(|_| !users_own) else {
        // Not ours to serve (anymore): don't leave an old registration.
        let _ = std::fs::remove_file(&path);
        return;
    };
    let contents = file_manager_service(&files);
    if std::fs::read_to_string(&path).is_ok_and(|old| old == contents) {
        return;
    }
    write_config(&path, &contents);
    // The bus only rereads its service directories when asked.
    let reloaded = Command::new("dbus-send")
        .args([
            "--session",
            "--type=method_call",
            "--dest=org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.ReloadConfig",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !reloaded {
        warn!("can't ask the session bus to reload; \"show in folder\" may open another file manager");
    }
}

/// Undo `register_file_manager` when the session ends.
pub fn unregister_file_manager() {
    if let Some(path) = runtime_service_file() {
        let _ = std::fs::remove_file(path);
    }
}

fn file_manager_service(files: &Path) -> String {
    format!(
        "# Written by cnde for this session: \"show in folder\" opens cnde-files.\n\
         [D-BUS Service]\nName=org.freedesktop.FileManager1\nExec={}\n",
        shell_quote(&files.to_string_lossy())
    )
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
    write_config(path, contents);
}

fn write_config(path: &Path, contents: &str) {
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, contents));
    match result {
        Ok(()) => info!(path = %path.display(), "installed portal config"),
        Err(err) => warn!(path = %path.display(), %err, "can't install portal config"),
    }
}

/// What to do about xdg-desktop-portal-wlr's settings.
#[derive(Debug, PartialEq, Eq)]
enum WlrPlan {
    /// `share_picker = "keep"`.
    Keep,
    /// The user has a config (or their own `cnde` file): theirs wins.
    UserOwns(PathBuf),
    /// Ours is there and current.
    UpToDate,
    Write(PathBuf, String),
}

/// xdg-desktop-portal-wlr reads `<dir>/$XDG_CURRENT_DESKTOP` (ours is
/// `cnde`) before `<dir>/config`, so writing `cnde` would hide the user's
/// `config`: only do it when there's no `config`, and only ever replace a
/// `cnde` file that is exactly what cnde wrote.
fn plan_wlr_config(dir: &Path, share_picker: SharePicker, picker_command: &str) -> WlrPlan {
    if share_picker == SharePicker::Keep {
        return WlrPlan::Keep;
    }
    let generic = dir.join("config");
    if generic.exists() {
        return WlrPlan::UserOwns(generic);
    }
    let ours = dir.join("cnde");
    let wanted = render_wlr_config(picker_command);
    match std::fs::read_to_string(&ours) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => WlrPlan::Write(ours, wanted),
        Ok(existing) if existing == wanted => WlrPlan::UpToDate,
        Ok(existing) if was_written_by_cnde(&existing) => WlrPlan::Write(ours, wanted),
        _ => WlrPlan::UserOwns(ours),
    }
}

fn render_wlr_config(picker_command: &str) -> String {
    WLR_PORTAL_CONF.replace("@CHOOSER@", picker_command)
}

/// An unedited file from this or an older cnde (maybe for another path of
/// the picker, as when running from target/).
fn was_written_by_cnde(contents: &str) -> bool {
    contents == LEGACY_WLR_PORTAL_CONF
        || contents
            .lines()
            .find_map(|l| l.strip_prefix("chooser_cmd="))
            .is_some_and(|command| contents == render_wlr_config(command))
}

fn install_wlr_config(dir: &Path, share_picker: SharePicker) {
    let Some(command) = picker_command() else {
        if share_picker == SharePicker::Cnde {
            warn!(
                "{PICKER} not found next to the compositor or in PATH; leaving portal-wlr's picker alone"
            );
        }
        return;
    };
    match plan_wlr_config(dir, share_picker, &command) {
        WlrPlan::Keep => {
            info!("share_picker = \"keep\": not touching xdg-desktop-portal-wlr's config")
        }
        WlrPlan::UpToDate => {}
        WlrPlan::UserOwns(path) => info!(
            path = %path.display(),
            "using your own xdg-desktop-portal-wlr config; for cnde's screen picker set \
             chooser_type=simple and chooser_cmd={command} in its [screencast] section"
        ),
        WlrPlan::Write(path, contents) => write_config(&path, &contents),
    }
}

/// `cnde-share-picker` as portal-wlr should run it (it hands the string to
/// a shell): next to our own binary first (a dev build run from target/),
/// else wherever PATH has it.
fn picker_command() -> Option<String> {
    let path = find_binary(PICKER)?;
    Some(shell_quote(&path.to_string_lossy()))
}

/// One of cnde's programs: next to our own binary first (a dev build run
/// from target/), else wherever PATH has it.
fn find_binary(name: &str) -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(name)))
        .filter(|path| path.is_file());
    sibling.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    })
}

fn shell_quote(text: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+@".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Tell the session bus (and systemd, if the bus is systemd's per-user
/// one) the environment apps and portals need to reach us.
fn update_activation_environment(env: &ClientEnv) -> bool {
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
        Ok(status) if status.success() => {
            info!("exported the session environment to D-Bus");
            return true;
        }
        Ok(status) => {
            warn!(%status, "dbus-update-activation-environment failed; screen sharing may not work")
        }
        Err(err) => {
            warn!(%err, "can't run dbus-update-activation-environment; screen sharing may not work")
        }
    }
    false
}

/// Portals started by an earlier session outlive it, still pointing at its
/// WAYLAND_DISPLAY: they'd offer no screens to share. Restart the ones that
/// are systemd user services so they pick up the environment just exported.
/// Asks systemd by unit name; never touches processes directly.
fn restart_portals() {
    if !on_systemd_user_bus() {
        info!("not on systemd's user bus: leaving the portals as they are");
        return;
    }
    let listing = systemctl(&[
        "list-unit-files",
        "--no-legend",
        "--no-pager",
        "xdg-desktop-portal*.service",
    ]);
    let Some(listing) = listing else {
        info!("no systemd user manager to restart the portals with");
        return;
    };
    let units = portal_units_to_restart(&listing);
    if units.is_empty() {
        info!("no portal services installed to restart");
        return;
    }
    // A backend that crash-looped while the last session ended has hit
    // systemd's start limit, and try-restart skips failed units: clear that
    // first so D-Bus activation can start it again.
    let mut reset = vec!["reset-failed"];
    reset.extend(&units);
    let _ = systemctl(&reset);
    let mut args = vec!["try-restart"];
    args.extend(&units);
    match systemctl(&args) {
        Some(_) => info!(?units, "restarted the portals to bind to this session"),
        None => warn!(
            ?units,
            "can't restart the portals; screen sharing may need a re-login"
        ),
    }
}

/// Runs `systemctl --user`; its output if it succeeded.
fn systemctl(args: &[&str]) -> Option<String> {
    let output = Command::new("systemctl")
        .arg("--user")
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

/// Of `systemctl list-unit-files` output, the portal units we restart, in
/// our order (backends before the main portal).
fn portal_units_to_restart(listing: &str) -> Vec<&'static str> {
    PORTAL_UNITS
        .into_iter()
        .filter(|unit| {
            listing
                .lines()
                .any(|line| line.split_whitespace().next() == Some(unit))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test, under the temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cnde-portals-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CMD: &str = "/usr/bin/cnde-share-picker";

    #[test]
    fn writes_when_there_is_no_config() {
        let dir = scratch("none");
        let WlrPlan::Write(path, text) = plan_wlr_config(&dir, SharePicker::Cnde, CMD) else {
            panic!("expected a write");
        };
        assert_eq!(path, dir.join("cnde"));
        assert!(text.contains("chooser_type=simple"));
        assert!(text.contains("chooser_cmd=/usr/bin/cnde-share-picker\n"));
    }

    #[test]
    fn keep_and_users_own_config_win() {
        let dir = scratch("user");
        assert_eq!(plan_wlr_config(&dir, SharePicker::Keep, CMD), WlrPlan::Keep);
        std::fs::write(dir.join("config"), "[screencast]\nchooser_type=none\n").unwrap();
        assert_eq!(
            plan_wlr_config(&dir, SharePicker::Cnde, CMD),
            WlrPlan::UserOwns(dir.join("config"))
        );
    }

    #[test]
    fn only_our_own_unedited_file_is_replaced() {
        let dir = scratch("cnde-file");
        let ours = dir.join("cnde");
        let is_write = |dir: &Path| {
            matches!(
                plan_wlr_config(dir, SharePicker::Cnde, CMD),
                WlrPlan::Write(..)
            )
        };
        // Written by an older cnde, or for another place of the picker.
        std::fs::write(&ours, LEGACY_WLR_PORTAL_CONF).unwrap();
        assert!(is_write(&dir));
        std::fs::write(
            &ours,
            render_wlr_config("/old/target/debug/cnde-share-picker"),
        )
        .unwrap();
        assert!(is_write(&dir));
        std::fs::write(&ours, render_wlr_config(CMD)).unwrap();
        assert_eq!(
            plan_wlr_config(&dir, SharePicker::Cnde, CMD),
            WlrPlan::UpToDate
        );
        // Edited by the user: hands off.
        let edited = format!("{}# mine\n", render_wlr_config(CMD));
        std::fs::write(&ours, &edited).unwrap();
        assert_eq!(
            plan_wlr_config(&dir, SharePicker::Cnde, CMD),
            WlrPlan::UserOwns(ours.clone())
        );
        assert_eq!(std::fs::read_to_string(&ours).unwrap(), edited);
    }

    #[test]
    fn picker_paths_are_quoted_for_the_shell() {
        assert_eq!(
            shell_quote("/usr/bin/cnde-share-picker"),
            "/usr/bin/cnde-share-picker"
        );
        assert_eq!(shell_quote("/home/a b/it's"), r"'/home/a b/it'\''s'");
    }

    #[test]
    fn restarts_only_installed_portal_units() {
        let listing = "xdg-desktop-portal.service static -\n\
                       xdg-desktop-portal-wlr.service static -\n\
                       xdg-desktop-portal-rewrite.service static -\n";
        assert_eq!(
            portal_units_to_restart(listing),
            [
                "xdg-desktop-portal-wlr.service",
                "xdg-desktop-portal.service"
            ]
        );
        assert!(portal_units_to_restart("").is_empty());
    }
}
