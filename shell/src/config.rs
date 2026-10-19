//! The shell's slice of config.toml: turning it into a theme, and reloading
//! it live when the file changes (same rules as the compositor: a broken
//! edit keeps the last good config).

use std::{os::fd::OwnedFd, path::PathBuf};

use cnde_config::{Config, parse_color};
use cnde_ui::{Color, Theme};
use rustix::{
    fs::inotify::{self, CreateFlags, WatchFlags},
    io::Errno,
};
use smithay_client_toolkit::reexports::calloop::{
    Interest, LoopHandle, Mode, PostAction, generic::Generic,
};
use tracing::{error, info, warn};

use crate::App;

pub fn load() -> Config {
    cnde_config::default_path()
        .and_then(|path| {
            Config::load_or_create(&path)
                .map_err(|err| warn!(%err, "config error; using defaults"))
                .ok()
        })
        .unwrap_or_default()
}

pub fn theme_from(config: &Config) -> Theme {
    let panel = &config.panel;
    let color = |text: &str, fallback: Color| {
        parse_color(text).map_or(fallback, |(r, g, b, a)| {
            Color::rgba(r, g, b, a as f32 / 255.0)
        })
    };
    let mut theme = Theme::for_appearance(&config.appearance);
    theme.panel_height = panel.height;
    if !panel.rounded {
        theme.panel_radius = 0.0;
        theme.panel_shadow = 0;
    }
    theme.panel_background = color(&panel.background, theme.panel_background);
    // Frosted: the compositor blurs the wallpaper behind; tint it lightly
    // (a colour set in the config keeps its own transparency).
    if panel.frosted && panel.background.is_empty() {
        theme.panel_background = theme.panel_background.with_alpha(0.45);
    }
    // Its popups (quick settings, the calendar, menus) get frost behind
    // them too; tinted more, since text sits on all of them.
    if panel.frosted {
        theme.qs_background = theme.qs_background.with_alpha(0.74);
    }
    theme.panel_foreground = color(&panel.foreground, theme.panel_foreground);
    theme.font_size = panel.font_size;
    theme.panel_padding = panel.padding;
    theme.panel_button_padding = panel.button_padding;
    theme.workspace_dot_size = panel.workspace_dot_size;
    theme.workspace_active_width = panel.workspace_active_width;
    // The dots follow the text colour so a light panel stays legible.
    theme.workspace_active = theme.panel_foreground;
    theme.workspace_dot = theme
        .panel_foreground
        .with_alpha(theme.panel_foreground.a * 0.45);
    theme
}

/// Switch the light/dark style by editing config.toml; the reload that
/// follows applies it, here and everywhere else.
pub fn set_dark(dark: bool) {
    let Some(path) = cnde_config::default_path() else {
        return;
    };
    let result = cnde_config::ConfigFile::open(&path).and_then(|mut file| {
        file.set(
            "appearance.color_scheme",
            if dark { "dark" } else { "light" },
        );
        file.save()
    });
    if let Err(err) = result {
        warn!(%err, "can't switch the style");
    }
}

/// Watch the config's directory (editors often save by renaming over the
/// file) and reload on change.
pub fn watch(handle: &LoopHandle<'static, App>) {
    let Some(dir) = cnde_config::default_path().and_then(|p| p.parent().map(PathBuf::from)) else {
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
                "can't watch the config; restart the shell to apply edits"
            );
            return;
        }
    };
    let result = handle.insert_source(
        Generic::new(fd, Interest::READ, Mode::Level),
        |_, fd, app| {
            let mut buf = [0u8; 4096];
            loop {
                match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) | Err(Errno::AGAIN) => break,
                    Ok(_) | Err(Errno::INTR) => continue,
                    Err(_) => break,
                }
            }
            app.reload_config();
            Ok(PostAction::Continue)
        },
    );
    if let Err(err) = result {
        warn!(?err, "can't watch the config");
    }
}

impl App {
    pub fn reload_config(&mut self) {
        let Some(path) = cnde_config::default_path() else {
            return;
        };
        let config = match Config::load_or_create(&path) {
            Ok(config) => config,
            Err(err) => {
                error!(%err, "config not applied; keeping the previous one");
                return;
            }
        };
        if config == self.config {
            return;
        }
        info!("config reloaded");
        self.theme = theme_from(&config);
        cnde_ui::anim::set_speed(config.animations.speed as f32);
        self.icons.set_theme(&config.appearance.icon_theme);
        let dock_moved = config.dock.position != self.config.dock.position;
        self.config = config;
        self.status
            .set_profile_command(&self.config.power.profile_command);
        if self.config.dock.locked && self.dock_editing {
            self.finish_dock_edit();
        }
        for panel in &mut self.panels {
            panel.apply_theme(&self.theme);
        }
        if dock_moved {
            self.dock = None;
        }
        self.sync_panels();
        self.redraw_all();
    }
}
