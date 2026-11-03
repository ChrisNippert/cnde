//! Acting on the `[dark_mode]` schedule (the config crate's `schedule`
//! module says what it wants at a given time).
//!
//! The compositor owns the timer because it's always running (the shell is
//! optional). A scheduled style switch
//! is written to config.toml, exactly like the quick settings toggle, so
//! every process follows it the usual way: reload, crossfade, gsettings.

use std::time::Duration;

use cnde_config::schedule::Clock;
use smithay::reexports::calloop::{
    LoopHandle,
    timer::{TimeoutAction, Timer},
};
use tracing::{info, warn};

use crate::state::State;

/// How often the time is checked: a switch lands within this of its
/// minute, and a missed one (suspend, clock change) is caught up on.
const TICK: Duration = Duration::from_secs(20);

/// Follows a schedule, but lets a change made by hand stand until the
/// schedule next changes its mind (like GNOME and iOS): it only acts when
/// what the schedule wants is different from the last time it looked.
#[derive(Debug, Default)]
pub struct Follower {
    last: Option<bool>,
}

impl Follower {
    /// What to switch to now, if anything. `wanted` is the schedule's
    /// answer, `None` when it's off.
    pub fn update(&mut self, wanted: Option<bool>) -> Option<bool> {
        if wanted == self.last {
            return None;
        }
        self.last = wanted;
        wanted
    }

    /// The schedule changed: apply what it wants right away.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[derive(Debug, Default)]
pub struct Schedules {
    pub dark: Follower,
    /// Check again at the next loop iteration (the config changed).
    pub dirty: bool,
}

/// Check the schedules every `TICK`.
pub fn start(handle: &LoopHandle<'static, State>) {
    let result = handle.insert_source(Timer::from_duration(TICK), |_, _, state| {
        state.update_schedules();
        TimeoutAction::ToDuration(TICK)
    });
    if let Err(err) = result {
        warn!(
            ?err,
            "can't start the schedule timer; dark mode won't follow the time"
        );
    }
}

impl State {
    /// The config changed: re-check the schedules soon, and if the dark
    /// mode schedule itself changed, apply it even if that undoes a switch
    /// made by hand.
    pub fn schedules_config_changed(&mut self, old: Option<&cnde_config::Config>) {
        if old.is_none_or(|old| {
            old.dark_mode != self.config.dark_mode || old.location != self.config.location
        }) {
            self.schedules.dark.reset();
        }
        self.schedules.dirty = true;
    }

    /// Switch the style to what the schedule wants now.
    pub fn update_schedules(&mut self) {
        self.schedules.dirty = false;
        let clock = Clock::now();
        let config = &self.config;
        let night = config.night_light.target(&config.location, &clock);
        if self.night_light.set(night, config.night_light.intensity) {
            info!(?night, "night light: fading");
            self.queue_redraw();
        }
        let config = &self.config;
        let wanted = config.dark_mode.wants_dark(&config.location, &clock);
        let Some(dark) = self.schedules.dark.update(wanted) else {
            return;
        };
        let is_dark = config.appearance.color_scheme == cnde_config::ColorScheme::Dark;
        if dark != is_dark {
            info!(dark, "dark mode schedule: switching the style");
            self.write_color_scheme(dark);
        }
    }

    /// Write `appearance.color_scheme` to config.toml (keeping everything
    /// else in it), and apply it now rather than waiting for the file
    /// watcher, which isn't running yet at startup.
    fn write_color_scheme(&mut self, dark: bool) {
        let Some(path) = &self.config_path else {
            return;
        };
        let result = cnde_config::ConfigFile::open(path).and_then(|mut file| {
            file.set(
                "appearance.color_scheme",
                if dark { "dark" } else { "light" },
            );
            file.save()
        });
        match result {
            Ok(new) if new != self.config => {
                let old = std::mem::replace(&mut self.config, new);
                self.apply_config(Some(&old));
            }
            Ok(_) => {}
            Err(err) => warn!(%err, "dark mode schedule: can't switch the style"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_schedule() {
        let mut f = Follower::default();
        // Starting up: apply what the schedule wants.
        assert_eq!(f.update(Some(true)), Some(true));
        assert_eq!(f.update(Some(true)), None);
        // Its next switch.
        assert_eq!(f.update(Some(false)), Some(false));
        // Off: nothing to do, and turning it on again applies at once.
        assert_eq!(f.update(None), None);
        assert_eq!(f.update(Some(false)), Some(false));
    }

    #[test]
    fn a_switch_by_hand_lasts_until_the_next_transition() {
        let mut f = Follower::default();
        assert_eq!(f.update(Some(true)), Some(true));
        // Switched to light by hand: the schedule still says dark, which
        // it already said, so it leaves it be...
        assert_eq!(f.update(Some(true)), None);
        assert_eq!(f.update(Some(true)), None);
        // ...until sunrise, when it wants light (already light: harmless)...
        assert_eq!(f.update(Some(false)), Some(false));
        // ...and dark again at sunset.
        assert_eq!(f.update(Some(true)), Some(true));
    }

    #[test]
    fn a_new_schedule_applies_at_once() {
        let mut f = Follower::default();
        assert_eq!(f.update(Some(false)), Some(false));
        f.reset();
        assert_eq!(f.update(Some(false)), Some(false));
    }
}
