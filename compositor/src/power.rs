//! Idle handling (`[power]`): dim the screen, turn it off and suspend after
//! no input, with separate timeouts on battery and plugged in.
//!
//! Input only records the time it happened (and wakes things up if they
//! were dimmed or off). One calloop timer is armed for the next step; when
//! it fires it works out where the idle time has got to (an input since it
//! was armed just pushes it further out), so nothing polls while the
//! computer is in use. Idle inhibitors (video players) and ext-idle-notify
//! (swayidle and friends) are served here too.
//!
//! Dimming is the night light's quad with a brightness factor (see
//! night_light.rs). Screen off is DPMS off on the DRM outputs (udev.rs);
//! nested there's no monitor to turn off, so it stays dimmed and the
//! computer is never suspended.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use cnde_config::IdleStage;
use smithay::{
    delegate_idle_notify,
    reexports::{
        calloop::{
            LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{DisplayHandle, Resource, protocol::wl_surface::WlSurface},
    },
    wayland::{
        compositor::get_parent,
        idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState},
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
    },
};
use tracing::{info, warn};

use crate::{backend::Backend, night_light::DIMMED, state::State, window::with_state};

/// How long the dimming takes to fade in.
const DIM_FADE: Duration = Duration::from_secs(1);
/// How often the power source is looked at.
const POWER_SOURCE_TICK: Duration = Duration::from_secs(30);
/// How long to wait for the locker to take hold before suspending anyway.
const LOCK_WAIT: Duration = Duration::from_secs(5);
const POWER_SUPPLY: &str = "/sys/class/power_supply";

pub struct Idle {
    /// The last keyboard, pointer, touch or gesture event.
    last_input: Instant,
    /// What's applied to the screen now (never `Suspend`).
    stage: IdleStage,
    /// Suspend was asked for in this idle period.
    suspended: bool,
    /// Monitors are off (DPMS); udev.rs stops drawing to them.
    pub screens_off: bool,
    on_battery: bool,
    timer: Option<RegistrationToken>,
    /// Surfaces that asked (zwp_idle_inhibitor_v1) for the screen to stay on.
    inhibitors: Vec<WlSurface>,
    inhibited: bool,
    notifier: IdleNotifierState<State>,
    #[allow(dead_code)] // owns the global; dropping it removes the protocol
    inhibit_manager: IdleInhibitManagerState,
}

impl Idle {
    pub fn new(display: &DisplayHandle, handle: LoopHandle<'static, State>) -> Self {
        Self {
            last_input: Instant::now(),
            stage: IdleStage::Active,
            suspended: false,
            screens_off: false,
            on_battery: on_battery(Path::new(POWER_SUPPLY)),
            timer: None,
            inhibitors: Vec::new(),
            inhibited: false,
            notifier: IdleNotifierState::new(display, handle),
            inhibit_manager: IdleInhibitManagerState::new::<State>(display),
        }
    }
}

/// Look at the power source every `POWER_SOURCE_TICK`.
pub fn start(handle: &LoopHandle<'static, State>) {
    let result = handle.insert_source(Timer::from_duration(POWER_SOURCE_TICK), |_, _, state| {
        let on_battery = on_battery(Path::new(POWER_SUPPLY));
        if on_battery != state.idle.on_battery {
            info!(on_battery, "power source changed");
            state.idle.on_battery = on_battery;
            state.idle_reevaluate();
        }
        TimeoutAction::ToDuration(POWER_SOURCE_TICK)
    });
    if let Err(err) = result {
        warn!(?err, "can't start the power source timer");
    }
}

/// Whether the computer runs on a battery right now: one is present and
/// no mains supply is online. Desktops (no battery) are never on battery.
/// Reads `root` (/sys/class/power_supply).
fn on_battery(root: &Path) -> bool {
    let read = |dir: &Path, name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map(|text| text.trim().to_string())
            .unwrap_or_default()
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    let (mut battery, mut discharging) = (false, false);
    let (mut mains, mut mains_online) = (false, false);
    for dir in entries.flatten().map(|e| e.path()) {
        match read(&dir, "type").as_str() {
            "Battery" => {
                // Wireless mice and the like report as batteries too.
                if read(&dir, "scope") == "Device" {
                    continue;
                }
                battery = true;
                discharging |= read(&dir, "status") == "Discharging";
            }
            // The UPS of a desktop isn't the computer's own battery.
            "UPS" | "" => {}
            // Mains, USB, USB_PD...: all have an `online` flag.
            _ => {
                mains = true;
                mains_online |= read(&dir, "online") == "1";
            }
        }
    }
    if !battery || mains_online {
        return false;
    }
    // Without a mains entry the battery's own status is all there is.
    mains || discharging
}

/// CLOCK_BOOTTIME minus CLOCK_MONOTONIC in nanoseconds: the time spent
/// suspended so far, which grows across a suspend.
#[allow(clippy::unnecessary_cast)] // the fields are i32 on 32-bit targets
fn suspended_nanos() -> i64 {
    let now = |clock| {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // Safety: `ts` is a valid timespec for clock_gettime to fill in.
        unsafe { libc::clock_gettime(clock, &mut ts) };
        ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
    };
    now(libc::CLOCK_BOOTTIME) - now(libc::CLOCK_MONOTONIC)
}

impl State {
    /// Called for every input event. Returns whether the screen was off or
    /// dimmed, so the event that woke it can be dropped.
    pub fn idle_activity(&mut self) -> bool {
        self.idle.last_input = Instant::now();
        self.idle.notifier.notify_activity(&self.seat);
        let was_off = self.idle.screens_off;
        if self.idle.stage != IdleStage::Active || self.idle.suspended {
            self.idle.suspended = false;
            self.idle_set_stage(IdleStage::Active);
            self.idle_rearm();
        } else if self.idle.timer.is_none() {
            self.idle_rearm();
        }
        was_off
    }

    /// The config or the power source changed: check the idle time against
    /// the new timeouts now.
    pub fn idle_reevaluate(&mut self) {
        self.idle_tick();
    }

    /// Step to where the idle time says (the timer ran out, or the timeouts
    /// changed), then arm the timer for the step after.
    fn idle_tick(&mut self) {
        if let Some(token) = self.idle.timer.take() {
            self.loop_handle.remove(token);
        }
        self.update_idle_inhibit();
        // Playing video counts as activity.
        if self.idle.inhibited {
            self.idle.last_input = Instant::now();
        }
        let nested = matches!(self.backend, Backend::Winit(_));
        let profile = self.config.power.profile(self.idle.on_battery).clone();
        let idle = self.idle.last_input.elapsed();
        let mut wanted = profile.stage_at(idle);
        if nested {
            // No monitor to turn off or computer to suspend.
            wanted = wanted.min(IdleStage::Dimmed);
        }
        match wanted {
            IdleStage::Suspend => {
                // The screen is as the other steps left it.
                let screen = cnde_config::PowerProfile {
                    suspend_after: 0,
                    ..profile
                };
                self.idle_set_stage(screen.stage_at(idle));
                if !self.idle.suspended {
                    self.idle.suspended = true;
                    self.idle_suspend();
                }
            }
            stage => {
                self.idle.suspended = false;
                self.idle_set_stage(stage);
            }
        }
        self.idle_rearm();
    }

    /// Arm the timer for the next step. While an inhibitor is held the
    /// timer still runs: it's what notices it going away.
    fn idle_rearm(&mut self) {
        if let Some(token) = self.idle.timer.take() {
            self.loop_handle.remove(token);
        }
        let profile = self.config.power.profile(self.idle.on_battery);
        let Some(next) = profile.next_step_in(self.idle.last_input.elapsed()) else {
            return;
        };
        let result = self
            .loop_handle
            .insert_source(Timer::from_duration(next), |_, _, state| {
                // The token is spent; `idle_tick` arms a new one.
                state.idle.timer = None;
                state.idle_tick();
                TimeoutAction::Drop
            });
        match result {
            Ok(token) => self.idle.timer = Some(token),
            Err(err) => warn!(?err, "can't start the idle timer"),
        }
    }

    /// Show `stage`: dimmed, or the monitors off, or neither.
    fn idle_set_stage(&mut self, stage: IdleStage) {
        let before = self.idle.stage;
        if stage == before {
            return;
        }
        info!(?stage, "idle");
        self.idle.stage = stage;
        match stage {
            IdleStage::Active => {
                // Back at once: waking shouldn't take a second.
                self.night_light.set_dim(1.0, Duration::ZERO);
            }
            IdleStage::Dimmed => {
                let fade = if before == IdleStage::ScreenOff {
                    Duration::ZERO
                } else {
                    DIM_FADE
                };
                self.night_light.set_dim(DIMMED, fade);
            }
            // Dimmed already or not, nobody sees it.
            IdleStage::ScreenOff | IdleStage::Suspend => {}
        }
        // Stay dark while locking and suspending; waking goes to Active.
        let off = matches!(stage, IdleStage::ScreenOff | IdleStage::Suspend);
        if off != self.idle.screens_off {
            self.idle.screens_off = off;
            if off {
                self.udev_screens_off();
            }
        }
        // Waking, dimming, and the first frame after DPMS off.
        self.queue_redraw();
    }

    /// Lock (if there's a locker to start), then suspend.
    fn idle_suspend(&mut self) {
        info!("idle: suspending");
        let command = self.config.lock.command.trim();
        if self.lock.is_locked() || command.is_empty() {
            self.run_suspend();
            return;
        }
        self.lock_screen();
        let started = Instant::now();
        let result = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(200)),
            move |_, _, state| {
                // Woken in the meantime: never mind.
                if !state.idle.suspended {
                    return TimeoutAction::Drop;
                }
                if state.lock.is_locked() {
                    state.run_suspend();
                    return TimeoutAction::Drop;
                }
                if started.elapsed() > LOCK_WAIT {
                    warn!("the screen didn't lock in time; suspending anyway");
                    state.run_suspend();
                    return TimeoutAction::Drop;
                }
                TimeoutAction::ToDuration(Duration::from_millis(200))
            },
        );
        if let Err(err) = result {
            warn!(?err, "can't wait for the lock; suspending now");
            self.run_suspend();
        }
    }

    /// Ask logind (or elogind) to suspend; works with both, with no D-Bus
    /// client of our own. Then watch for the computer waking up.
    fn run_suspend(&mut self) {
        let baseline = suspended_nanos();
        std::thread::spawn(|| {
            let result = std::process::Command::new("loginctl")
                .arg("suspend")
                .stdin(std::process::Stdio::null())
                .status();
            match result {
                Ok(status) if !status.success() => warn!(%status, "loginctl suspend failed"),
                Err(err) => warn!(%err, "can't run loginctl to suspend"),
                Ok(_) => {}
            }
        });
        // Waking from suspend sends us nothing (and the screen may be
        // asleep without anyone touching a key, e.g. a lid opening), so
        // notice the jump in the time spent suspended.
        let result = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            move |_, _, state| {
                if !state.idle.suspended {
                    return TimeoutAction::Drop;
                }
                if suspended_nanos() > baseline + 1_000_000_000 {
                    info!("resumed from suspend");
                    state.idle_activity();
                    return TimeoutAction::Drop;
                }
                TimeoutAction::ToDuration(Duration::from_secs(1))
            },
        );
        if let Err(err) = result {
            warn!(?err, "can't watch for resuming; the screen wakes on input");
        }
    }

    /// An inhibitor counts while its surface is on screen: mapped, on the
    /// current workspace and not minimized.
    fn inhibitor_visible(&self, surface: &WlSurface) -> bool {
        if !surface.is_alive() {
            return false;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.space.elements().any(|window| {
            window.toplevel().is_some_and(|t| *t.wl_surface() == root)
                && !with_state(window, |state| state.minimized)
        })
    }

    /// Re-check whether any inhibitor counts, and tell ext-idle-notify
    /// clients when that changes. Cheap with none, the usual case.
    pub fn update_idle_inhibit(&mut self) {
        if self.idle.inhibitors.is_empty() && !self.idle.inhibited {
            return;
        }
        self.idle.inhibitors.retain(|s| s.is_alive());
        let inhibited = self
            .idle
            .inhibitors
            .iter()
            .any(|s| self.inhibitor_visible(s));
        if inhibited != self.idle.inhibited {
            self.idle.inhibited = inhibited;
            self.idle.notifier.set_is_inhibited(inhibited);
        }
    }
}

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle.inhibitors.push(surface);
        self.update_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle.inhibitors.retain(|s| *s != surface);
        self.update_idle_inhibit();
    }
}
smithay::delegate_idle_inhibit!(State);

impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle.notifier
    }
}
delegate_idle_notify!(State);

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake /sys/class/power_supply with the given (name, files).
    fn supply(entries: &[(&str, &[(&str, &str)])]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "cnde-power-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for (name, files) in entries {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for (file, text) in *files {
                std::fs::write(dir.join(file), format!("{text}\n")).unwrap();
            }
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn check(entries: &[(&str, &[(&str, &str)])]) -> bool {
        let root = supply(entries);
        let result = on_battery(&root);
        let _ = std::fs::remove_dir_all(root);
        result
    }

    #[test]
    fn a_desktop_is_never_on_battery() {
        assert!(!check(&[]));
        assert!(!check(&[("AC", &[("type", "Mains"), ("online", "0")])]));
    }

    #[test]
    fn laptop_on_battery_and_plugged_in() {
        let battery: &[(&str, &str)] = &[("type", "Battery"), ("status", "Discharging")];
        assert!(check(&[
            ("BAT0", battery),
            ("AC", &[("type", "Mains"), ("online", "0")]),
        ]));
        assert!(!check(&[
            ("BAT0", &[("type", "Battery"), ("status", "Charging")]),
            ("AC", &[("type", "Mains"), ("online", "1")]),
        ]));
        // No mains entry: the battery says.
        assert!(check(&[("BAT0", battery)]));
        assert!(!check(&[(
            "BAT0",
            &[("type", "Battery"), ("status", "Full")]
        )]));
    }

    #[test]
    fn peripheral_batteries_dont_count() {
        assert!(!check(&[(
            "hidpp_battery_0",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("status", "Discharging")
            ],
        )]));
    }
}
