//! Small animations for immediate-mode painting: a switch's knob sliding,
//! a hover highlight fading, a slider gliding to where you clicked.
//!
//! Each paint asks [`Animator::get`] for a value by key, passing where it
//! should end up. When the target changes, the value eases there over
//! `duration`; while anything is still moving, [`Animator::running`] says
//! to paint again next frame. The first time a key is seen it starts at its
//! target, so nothing animates when a window first appears.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

/// `animations.speed` from the config: durations are multiplied by it, and
/// 0 turns animations off. Stored as f32 bits.
static SPEED: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

pub fn set_speed(speed: f32) {
    SPEED.store(speed.max(0.0).to_bits(), Ordering::Relaxed);
}

/// `duration` after the speed setting.
pub fn scaled(duration: Duration) -> Duration {
    duration.mul_f32(f32::from_bits(SPEED.load(Ordering::Relaxed)))
}

/// How long most control animations take (libadwaita uses 150-250 ms).
pub const SHORT: Duration = Duration::from_millis(150);
pub const MEDIUM: Duration = Duration::from_millis(220);

#[derive(Debug, Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Tween {
    fn value(&self, now: Instant) -> f32 {
        let t = progress(self.start, self.duration, now);
        self.from + (self.to - self.from) * ease_out(t)
    }

    fn done(&self, now: Instant) -> bool {
        now >= self.start + self.duration
    }
}

pub struct Animator<K> {
    tweens: HashMap<K, Tween>,
    /// When this paint is happening; one clock for the whole frame.
    now: Instant,
    /// Keys asked for since the last `begin`, so stale ones can be dropped.
    seen: Vec<K>,
}

impl<K: Hash + Eq + Clone> Default for Animator<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Clone> Animator<K> {
    pub fn new() -> Self {
        Self {
            tweens: HashMap::new(),
            now: Instant::now(),
            seen: Vec::new(),
        }
    }

    /// Call at the start of each paint.
    pub fn begin(&mut self) {
        self.now = Instant::now();
        self.seen.clear();
    }

    /// Call at the end of each paint: forgets keys that weren't painted (a
    /// row scrolled away, a page left), so they start fresh next time.
    pub fn end(&mut self) {
        let seen = &self.seen;
        self.tweens.retain(|k, _| seen.contains(k));
    }

    /// The value for `key`, easing towards `target`.
    pub fn get(&mut self, key: K, target: f32, duration: Duration) -> f32 {
        let now = self.now;
        self.seen.push(key.clone());
        match self.tweens.get_mut(&key) {
            None => {
                self.tweens.insert(
                    key,
                    Tween {
                        from: target,
                        to: target,
                        start: now,
                        duration: Duration::ZERO,
                    },
                );
                target
            }
            Some(tween) => {
                if tween.to != target {
                    *tween = Tween {
                        from: tween.value(now),
                        to: target,
                        start: now,
                        duration: scaled(duration),
                    };
                }
                tween.value(now)
            }
        }
    }

    /// Jump straight to `value` (a slider being dragged follows the pointer).
    pub fn set(&mut self, key: K, value: f32) -> f32 {
        self.seen.push(key.clone());
        self.tweens.insert(
            key,
            Tween {
                from: value,
                to: value,
                start: self.now,
                duration: Duration::ZERO,
            },
        );
        value
    }

    /// Start `key` from `from` towards wherever it's next asked to go (a
    /// page fading in from 0).
    pub fn restart(&mut self, key: K, from: f32, to: f32, duration: Duration) {
        self.tweens.insert(
            key,
            Tween {
                from,
                to,
                start: Instant::now(),
                duration: scaled(duration),
            },
        );
    }

    /// Whether anything is still moving: paint again next frame.
    pub fn running(&self) -> bool {
        let now = Instant::now();
        self.tweens.values().any(|t| !t.done(now))
    }
}

/// 0-1 along a `duration` that began at `start`.
pub fn progress(start: Instant, duration: Duration, now: Instant) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    (now.saturating_duration_since(start).as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
}

/// Cubic ease-out: fast start, gentle landing.
pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sight_is_at_target() {
        let mut a = Animator::new();
        a.begin();
        assert_eq!(a.get("k", 1.0, SHORT), 1.0);
        assert!(!a.running());
    }

    #[test]
    fn eases_to_a_new_target() {
        let mut a = Animator::new();
        a.begin();
        a.get("k", 0.0, SHORT);
        a.begin();
        let v = a.get("k", 1.0, SHORT);
        assert!(v < 0.1, "starts near the old value, got {v}");
        assert!(a.running());
        std::thread::sleep(SHORT + Duration::from_millis(5));
        a.begin();
        assert_eq!(a.get("k", 1.0, SHORT), 1.0);
        assert!(!a.running());
    }

    #[test]
    fn unpainted_keys_are_forgotten() {
        let mut a = Animator::new();
        a.begin();
        a.get("gone", 0.0, SHORT);
        a.end();
        a.begin();
        a.get("gone", 1.0, SHORT);
        a.begin();
        a.end();
        a.begin();
        // Seen fresh: no animation.
        assert_eq!(a.get("gone", 1.0, SHORT), 1.0);
    }
}
