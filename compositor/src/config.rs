//! Loading `config.toml`, watching it for changes, and applying it live
//! (ARCHITECTURE.md §8). Bad edits keep the last good config.

use std::{os::fd::OwnedFd, path::PathBuf};

use cnde_config::{AccelProfile, Config};
use rustix::{
    fs::inotify::{self, CreateFlags, WatchFlags},
    io::Errno,
};
use smithay::{
    input::keyboard::XkbConfig,
    reexports::{
        calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic},
        input::{AccelProfile as LibinputAccel, Device as InputDevice},
    },
};
use tracing::{error, info, warn};

use crate::{animation, state::State};

/// Load the config at startup. Any problem falls back to defaults: the
/// compositor must start even with a broken config.
pub fn load_initial() -> (Config, Option<PathBuf>) {
    let Some(path) = cnde_config::default_path() else {
        warn!("no HOME or XDG_CONFIG_HOME; using the default config");
        return (Config::default(), None);
    };
    match Config::load_or_create(&path) {
        Ok(config) => {
            info!(path = %path.display(), "loaded config");
            (config, Some(path))
        }
        Err(err) => {
            error!(%err, "config error; using defaults until it's fixed");
            (Config::default(), Some(path))
        }
    }
}

/// Watch the config's directory: editors often save by writing a new file
/// and renaming it over the old one, which a watch on the file itself misses.
pub fn watch(handle: &LoopHandle<'static, State>, path: PathBuf) {
    let Some(dir) = path.parent().map(PathBuf::from) else {
        return;
    };
    let watcher = || -> rustix::io::Result<OwnedFd> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        inotify::add_watch(
            &fd,
            &dir,
            WatchFlags::CLOSE_WRITE | WatchFlags::MOVED_TO | WatchFlags::CREATE,
        )?;
        Ok(fd)
    };
    let fd = match watcher() {
        Ok(fd) => fd,
        Err(err) => {
            warn!(
                ?err,
                "can't watch the config for changes; restart to apply edits"
            );
            return;
        }
    };
    let result = handle.insert_source(
        Generic::new(fd, Interest::READ, Mode::Level),
        move |_, fd, state| {
            // Drain the events; which file changed doesn't matter, the reload
            // ignores no-op changes.
            let mut buf = [0u8; 4096];
            loop {
                match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) | Err(Errno::AGAIN) => break,
                    Ok(_) | Err(Errno::INTR) => continue,
                    Err(err) => {
                        warn!(?err, "config watcher failed");
                        break;
                    }
                }
            }
            state.reload_config();
            Ok(PostAction::Continue)
        },
    );
    if let Err(err) = result {
        warn!(?err, "can't watch the config for changes");
    }
}

impl State {
    pub fn reload_config(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        if !path.exists() {
            return;
        }
        let new = match Config::load_or_create(path) {
            Ok(config) => config,
            Err(err) => {
                // TODO: show as a notification once the shell exists.
                error!(%err, "config not applied; keeping the previous one");
                return;
            }
        };
        if new == self.config {
            return;
        }
        info!("config reloaded");
        let old = std::mem::replace(&mut self.config, new);
        self.apply_config(Some(&old));
    }

    /// Push config values into the running compositor. `old` is `None` at
    /// startup, when everything is applied.
    pub fn apply_config(&mut self, old: Option<&Config>) {
        animation::set_speed(self.config.animations.speed);
        self.decorations.dark =
            self.config.appearance.color_scheme == cnde_config::ColorScheme::Dark;
        if old.is_some_and(|old| old.appearance.titlebars != self.config.appearance.titlebars) {
            self.apply_titlebars_config();
        }
        // A new style or wallpaper fades in over the old one.
        if old.is_some_and(|old| {
            old.appearance != self.config.appearance || old.wallpaper != self.config.wallpaper
        }) {
            let outputs: Vec<_> = self.space.outputs().cloned().collect();
            self.crossfade.start(outputs.iter());
        }
        self.wallpapers.update_config(&self.config);
        self.queue_redraw();

        let keyboard_changed =
            old.is_none_or(|old| old.input.keyboard != self.config.input.keyboard);
        if keyboard_changed && let Some(keyboard) = self.seat.get_keyboard() {
            let kb = self.config.input.keyboard.clone();
            let xkb = XkbConfig {
                layout: &kb.layout,
                variant: &kb.variant,
                options: (!kb.options.is_empty()).then(|| kb.options.clone()),
                ..XkbConfig::default()
            };
            if let Err(err) = keyboard.set_xkb_config(self, xkb) {
                error!(?err, layout = %kb.layout, "invalid keyboard layout; keeping the old one");
            }
            keyboard.change_repeat_info(kb.repeat_rate, kb.repeat_delay);
        }

        if old.is_none_or(|old| {
            old.input.mouse != self.config.input.mouse
                || old.input.touchpad != self.config.input.touchpad
        }) {
            self.reconfigure_input_devices();
        }
        if old.is_none_or(|old| old.outputs != self.config.outputs) {
            self.apply_output_layout();
        }
        // Nested in another desktop, gsettings belongs to that desktop.
        let nested = matches!(self.backend, crate::backend::Backend::Winit(_));
        if !nested && old.is_none_or(|old| old.appearance != self.config.appearance) {
            publish_color_scheme(self.config.appearance.color_scheme);
        }
    }

    /// Apply mouse/touchpad settings to one libinput device.
    pub fn configure_input_device(&self, device: &mut InputDevice) {
        let input = &self.config.input;
        let is_touchpad = device.config_tap_finger_count() > 0;
        let (speed, profile, natural) = if is_touchpad {
            let tp = &input.touchpad;
            let _ = device.config_tap_set_enabled(tp.tap_to_click);
            let _ = device.config_tap_set_drag_enabled(tp.tap_and_drag);
            let _ = device.config_dwt_set_enabled(tp.disable_while_typing);
            (tp.speed, tp.accel_profile, tp.natural_scroll)
        } else {
            let mouse = &input.mouse;
            (mouse.speed, mouse.accel_profile, mouse.natural_scroll)
        };
        if device.config_accel_is_available() {
            let _ = device.config_accel_set_speed(speed);
            let _ = device.config_accel_set_profile(match profile {
                AccelProfile::Adaptive => LibinputAccel::Adaptive,
                AccelProfile::Flat => LibinputAccel::Flat,
            });
        }
        if device.config_scroll_has_natural_scroll() {
            let _ = device.config_scroll_set_natural_scroll_enabled(natural);
        }
    }
}

/// Tell apps about light/dark. The GTK portal backend (our default for the
/// Settings portal) serves `org.freedesktop.appearance.color-scheme` from
/// this GNOME setting, and libadwaita, Qt 6 and browsers follow it. It's
/// shared with a GNOME session on the same account, like any preference.
fn publish_color_scheme(scheme: cnde_config::ColorScheme) {
    let value = match scheme {
        cnde_config::ColorScheme::Dark => "prefer-dark",
        cnde_config::ColorScheme::Light => "default",
    };
    std::thread::spawn(move || {
        let result = std::process::Command::new("gsettings")
            .args(["set", "org.gnome.desktop.interface", "color-scheme", value])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Err(err) = result {
            tracing::debug!(%err, "no gsettings; apps won't follow the style");
        }
    });
}
