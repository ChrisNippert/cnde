//! Compositor-driven animations (ARCHITECTURE.md §4.3).
//!
//! Milestone 1 only needs eased timelines. Springs (interruptible, velocity
//! preserving) arrive with touchpad gestures and the overview.

pub mod presets;

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Global duration multiplier from `animations.speed` in the config;
/// 0.0 disables motion ("reduce motion"). Stored as f64 bits.
static SPEED: AtomicU64 = AtomicU64::new(1.0f64.to_bits());

pub fn set_speed(speed: f64) {
    SPEED.store(speed.max(0.0).to_bits(), Ordering::Relaxed);
}

fn speed() -> f64 {
    f64::from_bits(SPEED.load(Ordering::Relaxed))
}

#[allow(dead_code)] // the full palette for presets, not all in use yet
#[derive(Debug, Clone, Copy)]
pub enum Curve {
    Linear,
    EaseOutCubic,
    EaseOutExpo,
}

impl Curve {
    /// Maps linear progress `t` in [0, 1] to eased progress.
    pub fn apply(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Curve::Linear => t,
            Curve::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            Curve::EaseOutExpo => {
                if t >= 1.0 {
                    1.0
                } else {
                    1.0 - 2f64.powf(-10.0 * t)
                }
            }
        }
    }
}

/// Timing for one kind of animation. All of these live in `presets`.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub duration: Duration,
    pub curve: Curve,
}

#[derive(Debug, Clone)]
pub struct Animation {
    start: Instant,
    duration: Duration,
    from: f64,
    to: f64,
    curve: Curve,
}

impl Animation {
    pub fn new(from: f64, to: f64, params: Params) -> Self {
        Self {
            start: Instant::now(),
            duration: params.duration.mul_f64(speed()),
            from,
            to,
            curve: params.curve,
        }
    }

    pub fn value_at(&self, now: Instant) -> f64 {
        if self.is_done(now) {
            return self.to;
        }
        let t =
            now.saturating_duration_since(self.start).as_secs_f64() / self.duration.as_secs_f64();
        self.from + (self.to - self.from) * self.curve.apply(t)
    }

    pub fn is_done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_hit_endpoints() {
        for curve in [Curve::Linear, Curve::EaseOutCubic, Curve::EaseOutExpo] {
            assert_eq!(curve.apply(0.0), 0.0);
            assert!((curve.apply(1.0) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn zero_duration_is_instant() {
        let anim = Animation::new(
            0.0,
            1.0,
            Params {
                duration: Duration::ZERO,
                curve: Curve::Linear,
            },
        );
        assert!(anim.is_done(Instant::now()));
        assert_eq!(anim.value_at(Instant::now()), 1.0);
    }
}
