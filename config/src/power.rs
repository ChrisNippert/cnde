//! `[power]`: dimming the screen, turning it off and suspending when the
//! computer sits idle, with separate timeouts on battery and plugged in.
//!
//! Functions of the time idle, so the compositor (which acts on them) and
//! its tests agree without real time passing.

use std::time::Duration;

use serde::Deserialize;

use crate::{Config, Error};

/// The longest timeout allowed: a day.
const MAX_SECONDS: u32 = 24 * 60 * 60;

/// `[power]`. Desktops without a battery use `ac`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Power {
    /// Lock the session when the monitors turn off.
    pub lock_on_screen_off: bool,
    /// Lock before suspending.
    pub lock_before_suspend: bool,
    /// Show the charge as a number next to the battery icon in the top bar.
    pub battery_percent: bool,
    /// Warn when the charge drops below this percent while discharging; 0
    /// turns the warning off.
    pub low_battery: u8,
    /// Command that switches the power mode when no power-profiles daemon
    /// is running; it gets `power-saver`, `balanced` or `performance` as
    /// its last argument. Empty: no command.
    pub profile_command: String,
    /// While running on the battery.
    pub battery: PowerProfile,
    /// While plugged in.
    pub ac: PowerProfile,
}

/// How long to be idle before each step, in seconds; 0 skips the step.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PowerProfile {
    /// Fade the screen to half brightness.
    pub dim_after: u32,
    /// Turn the monitors off.
    pub screen_off_after: u32,
    /// Lock (if a lock command is set) and suspend.
    pub suspend_after: u32,
}

impl Default for PowerProfile {
    /// The battery's; [`Power::default`] has the plugged-in values.
    fn default() -> Self {
        Self {
            dim_after: 120,
            screen_off_after: 300,
            suspend_after: 900,
        }
    }
}

impl Default for Power {
    fn default() -> Self {
        Self {
            lock_on_screen_off: true,
            lock_before_suspend: true,
            battery_percent: true,
            low_battery: 15,
            profile_command: String::new(),
            battery: PowerProfile::default(),
            ac: PowerProfile {
                dim_after: 300,
                screen_off_after: 600,
                suspend_after: 0,
            },
        }
    }
}

/// How far along the idle steps a computer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdleStage {
    Active,
    Dimmed,
    ScreenOff,
    Suspend,
}

impl Power {
    pub fn profile(&self, on_battery: bool) -> &PowerProfile {
        if on_battery { &self.battery } else { &self.ac }
    }
}

impl PowerProfile {
    fn steps(&self) -> [(u32, IdleStage); 3] {
        [
            (self.dim_after, IdleStage::Dimmed),
            (self.screen_off_after, IdleStage::ScreenOff),
            (self.suspend_after, IdleStage::Suspend),
        ]
    }

    /// The furthest step reached after being idle for `idle`. The steps are
    /// independent: a screen-off time shorter than the dim time just skips
    /// the dimming.
    pub fn stage_at(&self, idle: Duration) -> IdleStage {
        self.steps()
            .into_iter()
            .filter(|(secs, _)| *secs > 0 && idle >= Duration::from_secs(*secs as u64))
            .map(|(_, stage)| stage)
            .max()
            .unwrap_or(IdleStage::Active)
    }

    /// How much longer until the next step, or `None` when none is left.
    pub fn next_step_in(&self, idle: Duration) -> Option<Duration> {
        self.steps()
            .into_iter()
            .filter(|(secs, _)| *secs > 0)
            .map(|(secs, _)| Duration::from_secs(secs as u64))
            .filter(|at| *at > idle)
            .min()
            .map(|at| at - idle)
    }
}

pub(crate) fn validate(config: &Config) -> Result<(), Error> {
    if config.power.low_battery > 100 {
        return Err(Error::Invalid(format!(
            "power.low_battery must be 0 (off) to 100 percent, got {}",
            config.power.low_battery
        )));
    }
    for (name, profile) in [("battery", &config.power.battery), ("ac", &config.power.ac)] {
        for (key, secs) in [
            ("dim_after", profile.dim_after),
            ("screen_off_after", profile.screen_off_after),
            ("suspend_after", profile.suspend_after),
        ] {
            if secs > MAX_SECONDS {
                return Err(Error::Invalid(format!(
                    "power.{name}.{key} must be 0 (never) to {MAX_SECONDS} seconds, got {secs}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn defaults() {
        let power = Power::default();
        assert!(power.lock_on_screen_off);
        assert!(power.lock_before_suspend);
        assert!(power.battery_percent);
        assert_eq!(power.low_battery, 15);
        assert_eq!(power.profile_command, "");
        assert_eq!(power.battery.dim_after, 120);
        assert_eq!(power.battery.screen_off_after, 300);
        assert_eq!(power.battery.suspend_after, 900);
        assert_eq!(power.ac.dim_after, 300);
        assert_eq!(power.ac.screen_off_after, 600);
        assert_eq!(power.ac.suspend_after, 0, "plugged in never suspends");
        assert!(std::ptr::eq(power.profile(true), &power.battery));
        assert!(std::ptr::eq(power.profile(false), &power.ac));
    }

    #[test]
    fn parses_and_overrides_one_key() {
        let config = Config::parse("[power.battery]\ndim_after = 30\n").unwrap();
        assert_eq!(config.power.battery.dim_after, 30);
        assert_eq!(config.power.battery.screen_off_after, 300);
        assert_eq!(config.power.ac, Power::default().ac);
    }

    #[test]
    fn lock_options_parse() {
        let config =
            Config::parse("[power]\nlock_on_screen_off = false\nlock_before_suspend = false\n")
                .unwrap();
        assert!(!config.power.lock_on_screen_off);
        assert!(!config.power.lock_before_suspend);
        assert_eq!(config.power.battery, PowerProfile::default());
    }

    #[test]
    fn battery_display_options_parse() {
        let config = Config::parse("[power]\nbattery_percent = false\nlow_battery = 0\n").unwrap();
        assert!(!config.power.battery_percent);
        assert_eq!(config.power.low_battery, 0);
        assert!(Config::parse("[power]\nlow_battery = 101\n").is_err());
        assert!(Config::parse("[power]\nlow_battery = -1\n").is_err());
        assert!(Config::parse("[power]\nlow_battery = 100\n").is_ok());
    }

    #[test]
    fn profile_command_parses() {
        let config = Config::parse("[power]\nprofile_command = \"my-tool set\"\n").unwrap();
        assert_eq!(config.power.profile_command, "my-tool set");
        assert!(Config::parse("[power]\nprofile_command = 3\n").is_err());
    }

    #[test]
    fn rejects_typos_and_huge_values() {
        assert!(Config::parse("[power.battery]\ndim = 30\n").is_err());
        assert!(Config::parse("[power.ac]\nsuspend_after = 100000\n").is_err());
        assert!(Config::parse("[power.ac]\nsuspend_after = -1\n").is_err());
        assert!(Config::parse("[power.ac]\nsuspend_after = 86400\n").is_ok());
    }

    #[test]
    fn stages_follow_the_idle_time() {
        let p = Power::default().battery;
        assert_eq!(p.stage_at(secs(0)), IdleStage::Active);
        assert_eq!(p.stage_at(secs(119)), IdleStage::Active);
        assert_eq!(p.stage_at(secs(120)), IdleStage::Dimmed);
        assert_eq!(p.stage_at(secs(300)), IdleStage::ScreenOff);
        assert_eq!(p.stage_at(secs(900)), IdleStage::Suspend);
    }

    #[test]
    fn zero_means_never() {
        let p = Power::default().ac;
        assert_eq!(p.stage_at(secs(100_000)), IdleStage::ScreenOff);
        assert_eq!(p.next_step_in(secs(600)), None);
        let off = PowerProfile {
            dim_after: 0,
            screen_off_after: 0,
            suspend_after: 0,
        };
        assert_eq!(off.stage_at(secs(100_000)), IdleStage::Active);
        assert_eq!(off.next_step_in(secs(0)), None);
    }

    #[test]
    fn next_step_counts_down() {
        let p = Power::default().battery;
        assert_eq!(p.next_step_in(secs(0)), Some(secs(120)));
        assert_eq!(p.next_step_in(secs(100)), Some(secs(20)));
        assert_eq!(p.next_step_in(secs(120)), Some(secs(180)));
        assert_eq!(p.next_step_in(secs(900)), None);
    }

    #[test]
    fn a_short_screen_off_skips_the_dim() {
        let p = PowerProfile {
            dim_after: 600,
            screen_off_after: 60,
            suspend_after: 0,
        };
        assert_eq!(p.stage_at(secs(30)), IdleStage::Active);
        assert_eq!(p.stage_at(secs(60)), IdleStage::ScreenOff);
        // The later dim time doesn't bring the screen back.
        assert_eq!(p.stage_at(secs(700)), IdleStage::ScreenOff);
    }
}
