//! Adaptive colour: what the bar, dock and popups take on (a window's edge
//! colour or the wallpaper's), and easing between those choices.

use std::time::Instant;

use crate::{
    Animator, Color, Theme,
    anim::{self, MEDIUM},
};

/// A colour for the bar or dock to take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tint {
    pub color: Color,
    /// Opaque (a window's colour; the compositor's backing makes it so) vs
    /// the surface keeping its own translucency (the wallpaper's colour).
    pub solid: bool,
}

/// `color` with its lightness cut to 60%: the dock's "darker" shade.
pub fn darker(color: Color) -> Color {
    let (h, s, l) = color.hsl();
    Color::from_hsl(h, s, l * 0.6).with_alpha(color.a)
}

/// The bar's colour: the window against the top edge, solid, else the
/// wallpaper's colour keeping the bar's translucency.
pub fn bar_choice(window: Option<Color>, wallpaper: Option<Color>) -> Option<Tint> {
    match (window, wallpaper) {
        (Some(color), _) => Some(Tint { color, solid: true }),
        (None, Some(color)) => Some(Tint {
            color,
            solid: false,
        }),
        _ => None,
    }
}

/// The dock's colour under `mode`: the bar's, or a darker shade of the
/// window against the bottom edge (else of the wallpaper).
pub fn dock_choice(
    mode: cnde_config::DockAdaptive,
    top: Option<Color>,
    bottom: Option<Color>,
    wallpaper: Option<Color>,
) -> Option<Tint> {
    match mode {
        cnde_config::DockAdaptive::Bar => bar_choice(top, wallpaper),
        cnde_config::DockAdaptive::Darker => bar_choice(bottom, wallpaper).map(|t| Tint {
            color: darker(t.color),
            ..t
        }),
    }
}

/// What's on screen right now while easing towards a [`Tint`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shown {
    pub color: Color,
    /// 0 (the surface's own colour) to 1 (fully the tint).
    pub amount: f32,
    /// 0 (keeps its translucency) to 1 (opaque).
    pub solid: f32,
}

impl Shown {
    /// `theme` with the bar/dock colours in this tint.
    pub fn apply(&self, theme: &Theme) -> Theme {
        theme.matched(self.color, self.amount, self.solid)
    }

    /// The same for a popup, which keeps its own alpha whatever the bar does.
    pub fn apply_translucent(&self, theme: &Theme) -> Theme {
        theme.matched(self.color, self.amount, 0.0)
    }
}

/// Eases a surface towards its [`Tint`]: in and out when one appears or
/// goes, and from colour to colour when it changes.
pub struct TintFade {
    amount: Animator<()>,
    solid: Animator<()>,
    from: Color,
    to: Color,
    since: Instant,
    /// The amount last shown, 0 meaning a new tint can appear as it is.
    last_amount: f32,
    last_solid: bool,
    held: bool,
}

impl Default for TintFade {
    fn default() -> Self {
        Self::new()
    }
}

impl TintFade {
    pub fn new() -> Self {
        Self {
            amount: Animator::new(),
            solid: Animator::new(),
            from: Color::rgb(0, 0, 0),
            to: Color::rgb(0, 0, 0),
            since: Instant::now(),
            last_amount: 0.0,
            last_solid: false,
            held: false,
        }
    }

    fn colour_progress(&self, now: Instant) -> f32 {
        anim::progress(self.since, anim::scaled(MEDIUM), now)
    }

    /// Call once per paint with the colour wanted; `None` once it has
    /// faded out.
    pub fn update(&mut self, target: Option<Tint>) -> Option<Shown> {
        self.amount.begin();
        self.solid.begin();
        let now = Instant::now();
        if let Some(t) = target {
            if !self.held || self.last_amount <= 0.0 {
                // Nothing showing: appear in place, don't slide from a
                // colour that's gone.
                self.from = t.color;
                self.to = t.color;
                self.since = now;
            } else if self.to != t.color {
                let k = anim::ease_out(self.colour_progress(now));
                self.from = self.from.mix(self.to, k);
                self.to = t.color;
                self.since = now;
            }
            self.held = true;
            self.last_solid = t.solid;
        }
        let amount = self
            .amount
            .get((), if target.is_some() { 1.0 } else { 0.0 }, MEDIUM);
        // Fading out keeps the last look.
        let solid = self
            .solid
            .get((), if self.last_solid { 1.0 } else { 0.0 }, MEDIUM);
        self.last_amount = amount;
        if amount <= 0.0 {
            self.held = false;
            return None;
        }
        let color = self
            .from
            .mix(self.to, anim::ease_out(self.colour_progress(now)));
        Some(Shown {
            color,
            amount,
            solid,
        })
    }

    /// Whether another frame is needed.
    pub fn running(&self) -> bool {
        self.amount.running()
            || self.solid.running()
            || (self.held && self.colour_progress(Instant::now()) < 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cnde_config::DockAdaptive;

    const RED: Color = Color::rgb(200, 30, 30);
    const BLUE: Color = Color::rgb(30, 30, 200);
    const GREEN: Color = Color::rgb(30, 200, 30);

    #[test]
    fn darker_cuts_lightness_and_keeps_hue_and_alpha() {
        let c = Color::rgba(200, 100, 100, 0.5);
        let d = darker(c);
        let (h0, _, l0) = c.hsl();
        let (h1, _, l1) = d.hsl();
        assert!((l1 - l0 * 0.6).abs() < 0.01);
        assert!((h1 - h0).abs() < 1.0);
        assert_eq!(d.a, 0.5);
    }

    #[test]
    fn bar_prefers_the_window_then_the_wallpaper() {
        assert_eq!(
            bar_choice(Some(RED), Some(BLUE)),
            Some(Tint {
                color: RED,
                solid: true
            })
        );
        assert_eq!(
            bar_choice(None, Some(BLUE)),
            Some(Tint {
                color: BLUE,
                solid: false
            })
        );
        assert_eq!(bar_choice(None, None), None);
    }

    #[test]
    fn dock_follows_the_bar_or_goes_darker() {
        let bar = dock_choice(DockAdaptive::Bar, Some(RED), Some(GREEN), Some(BLUE));
        assert_eq!(bar, bar_choice(Some(RED), Some(BLUE)));
        let dark = dock_choice(DockAdaptive::Darker, Some(RED), Some(GREEN), Some(BLUE)).unwrap();
        assert_eq!(dark.color, darker(GREEN));
        assert!(dark.solid);
        let dark = dock_choice(DockAdaptive::Darker, Some(RED), None, Some(BLUE)).unwrap();
        assert_eq!(dark.color, darker(BLUE));
        assert!(!dark.solid);
        assert_eq!(
            dock_choice(DockAdaptive::Darker, Some(RED), None, None),
            None
        );
    }

    #[test]
    fn a_translucent_tint_keeps_the_themes_alpha() {
        let theme = Theme::dark();
        let a = theme.panel_background.a;
        let t = theme.matched(BLUE, 1.0, 0.0);
        assert!((t.panel_background.a - a).abs() < 1e-5);
        assert!((t.panel_background.b - BLUE.b).abs() < 1e-5);
        assert_eq!(theme.matched(BLUE, 1.0, 1.0).panel_background.a, 1.0);
    }

    #[test]
    fn appears_in_place_and_eases_between_colours() {
        let mut fade = TintFade::new();
        let solid = |color| Some(Tint { color, solid: true });
        let s = fade.update(solid(RED)).unwrap();
        assert_eq!(s.color, RED);
        let s = fade.update(solid(BLUE)).unwrap();
        assert!(s.color.r > 0.5, "starts near the old colour");
        assert!(fade.running());
    }
}
