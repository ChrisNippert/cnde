//! Window animations and how they turn into on-screen geometry each frame:
//! open (fade + grow), resize (move/stretch with a crossfade from the old
//! contents) and close (fade + shrink a snapshot). ARCHITECTURE.md §4.3.

use std::time::Instant;

use smithay::utils::{Logical, Point, Rectangle, Serial, Size};

use crate::{
    animation::{Animation, presets},
    snapshot::Snapshot,
    window::WindowState,
};

/// A placement change (maximize, snap, …) in progress.
pub struct ResizeAnim {
    /// Where the window geometry was drawn when the change started.
    from: Rectangle<f64, Logical>,
    /// The old contents, crossfaded out over the live window.
    pub snapshot: Option<Snapshot>,
    phase: ResizePhase,
}

enum ResizePhase {
    /// Configure sent; keep showing the window where it was until the client
    /// commits a buffer at the new size (or takes too long). This is what
    /// keeps a maximize from flashing a stretched, stale buffer.
    Waiting {
        serial: Serial,
        since: Instant,
    },
    Running(Animation),
}

impl ResizeAnim {
    /// `serial` is the configure the client must ack; `None` if nothing was
    /// sent (the size isn't changing), so the motion can start right away.
    pub fn new(
        from: Rectangle<f64, Logical>,
        snapshot: Option<Snapshot>,
        serial: Option<Serial>,
    ) -> Self {
        let phase = match serial {
            Some(serial) => ResizePhase::Waiting {
                serial,
                since: Instant::now(),
            },
            None => ResizePhase::Running(Animation::new(0.0, 1.0, presets::WINDOW_RESIZE)),
        };
        Self {
            from,
            snapshot,
            phase,
        }
    }

    /// Called on each commit with the configure serial the client has acked.
    pub fn on_commit(&mut self, acked: Option<Serial>) {
        if let ResizePhase::Waiting { serial, .. } = self.phase
            && acked.is_some_and(|acked| acked.is_no_older_than(&serial))
        {
            self.phase = ResizePhase::Running(Animation::new(0.0, 1.0, presets::WINDOW_RESIZE));
        }
    }
}

/// A size change applied without animation (dragging a fullscreen or
/// maximized window off, or a workspace slide handing it to a monitor of
/// another size): until the client draws at `size`, show its old contents
/// squeezed into `size` at the window's live position, so a stale
/// full-screen buffer never spills onto other monitors.
///
/// Generic over the snapshot only so the commit logic can be unit-tested;
/// the compositor uses `Snapshot`.
pub struct PendingSize<S = Snapshot> {
    size: Size<i32, Logical>,
    /// The client's size when we asked for `size`.
    old_size: Size<i32, Logical>,
    serial: Serial,
    since: Instant,
    /// The old contents, if they should crossfade into the redrawn window
    /// rather than being swapped for it in one frame.
    snapshot: Option<S>,
    /// That crossfade, once the client has caught up.
    fade: Option<Animation>,
}

impl<S> PendingSize<S> {
    pub fn new(size: Size<i32, Logical>, old_size: Size<i32, Logical>, serial: Serial) -> Self {
        Self {
            size,
            old_size,
            serial,
            since: Instant::now(),
            snapshot: None,
            fade: None,
        }
    }

    /// Crossfade from `snapshot` once the client redraws.
    pub fn with_snapshot(mut self, snapshot: Option<S>) -> Self {
        self.snapshot = snapshot;
        self
    }

    /// Called on each commit. Whether it's over: the client has caught up
    /// and there are no old contents to fade out. If there are, the fade
    /// starts now and `window_frame` drops it once done.
    pub fn on_commit(&mut self, acked: Option<Serial>, current: Size<i32, Logical>) -> bool {
        if self.fade.is_some() || !self.is_resolved(acked, current) {
            return false;
        }
        if self.snapshot.is_none() {
            return true;
        }
        self.fade = Some(Animation::new(0.0, 1.0, presets::WINDOW_RESIZE));
        false
    }

    fn is_fading(&self) -> bool {
        self.fade.is_some()
    }

    /// Whether the client has caught up. Acking isn't enough: Chromium and
    /// Electron ack at once but redraw at the new size a few frames later.
    /// The size must also have moved off the old one (not necessarily to
    /// exactly `size`; terminals round to whole cells).
    pub fn is_resolved(&self, acked: Option<Serial>, current: Size<i32, Logical>) -> bool {
        acked.is_some_and(|acked| acked.is_no_older_than(&self.serial)) && current != self.old_size
    }
}

pub struct ClosingWindow {
    snapshot: Snapshot,
    /// Where the window geometry was drawn when it closed.
    rect: Rectangle<f64, Logical>,
    anim: Animation,
    /// Stacking position: how many windows were below it.
    pub below: usize,
}

impl ClosingWindow {
    pub fn new(snapshot: Snapshot, rect: Rectangle<f64, Logical>, below: usize) -> Self {
        Self {
            snapshot,
            rect,
            anim: Animation::new(0.0, 1.0, presets::WINDOW_CLOSE),
            below,
        }
    }

    pub fn is_done(&self, now: Instant) -> bool {
        self.anim.is_done(now)
    }

    /// The snapshot to draw, where (space coordinates) and how opaque.
    pub fn frame(&self, now: Instant) -> (&Snapshot, Rectangle<f64, Logical>, f32) {
        let progress = self.anim.value_at(now);
        let scale = 1.0 - (1.0 - presets::WINDOW_CLOSE_END_SCALE) * progress;
        (
            &self.snapshot,
            scale_about_centre(self.rect, scale),
            (1.0 - progress) as f32,
        )
    }
}

/// What to draw for one live window this frame.
pub struct WindowFrame {
    /// Where the window geometry appears (space coordinates).
    pub rect: Rectangle<f64, Logical>,
    pub alpha: f32,
    /// Opacity of the old-contents snapshot drawn over the window, if any.
    pub snapshot_alpha: Option<f32>,
}

/// Advance `state`'s animations to `now` and work out how to draw the window,
/// whose real (layout) geometry is `live`. Finished animations are dropped.
pub fn window_frame(
    state: &mut WindowState,
    live: Rectangle<f64, Logical>,
    now: Instant,
) -> WindowFrame {
    let mut frame = WindowFrame {
        rect: live,
        alpha: 1.0,
        snapshot_alpha: None,
    };

    if let Some(resize) = &mut state.resize_anim {
        if let ResizePhase::Waiting { since, .. } = resize.phase
            && now.saturating_duration_since(since) > presets::RESIZE_COMMIT_TIMEOUT
        {
            resize.phase = ResizePhase::Running(Animation::new(0.0, 1.0, presets::WINDOW_RESIZE));
        }
        match &resize.phase {
            ResizePhase::Waiting { .. } => frame.rect = resize.from,
            ResizePhase::Running(anim) => {
                let progress = anim.value_at(now);
                frame.rect = lerp_rect(resize.from, live, progress);
                frame.snapshot_alpha = resize.snapshot.as_ref().map(|_| (1.0 - progress) as f32);
                if anim.is_done(now) {
                    state.resize_anim = None;
                }
            }
        }
    }

    if let Some(pending) = &state.pending_size {
        match &pending.fade {
            // The client has redrawn: show it as it is, with its old
            // contents fading out over it.
            Some(fade) if fade.is_done(now) => state.pending_size = None,
            Some(fade) => {
                if state.resize_anim.is_none() {
                    frame.snapshot_alpha = Some((1.0 - fade.value_at(now)) as f32);
                }
            }
            None if now.saturating_duration_since(pending.since)
                > presets::PENDING_SIZE_TIMEOUT =>
            {
                state.pending_size = None;
            }
            None if state.resize_anim.is_none() => {
                frame.rect = Rectangle::new(frame.rect.loc, pending.size.to_f64());
            }
            None => {}
        }
    }

    if let Some(open) = &state.open_anim {
        let progress = open.value_at(now);
        let start = presets::WINDOW_OPEN_START_SCALE;
        frame.rect = scale_about_centre(frame.rect, start + (1.0 - start) * progress);
        frame.alpha = progress as f32;
        if open.is_done(now) {
            state.open_anim = None;
        }
    }

    frame
}

/// The old contents to draw over the window at `WindowFrame::snapshot_alpha`.
pub fn fading_snapshot(state: &WindowState) -> Option<&Snapshot> {
    match (&state.resize_anim, &state.pending_size) {
        (Some(resize), _) => resize.snapshot.as_ref(),
        (None, Some(pending)) if pending.is_fading() => pending.snapshot.as_ref(),
        _ => None,
    }
}

pub fn is_animating(state: &WindowState) -> bool {
    state.open_anim.is_some() || state.resize_anim.is_some() || state.pending_size.is_some()
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

pub fn lerp_rect(
    a: Rectangle<f64, Logical>,
    b: Rectangle<f64, Logical>,
    t: f64,
) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((lerp(a.loc.x, b.loc.x, t), lerp(a.loc.y, b.loc.y, t))),
        Size::from((lerp(a.size.w, b.size.w, t), lerp(a.size.h, b.size.h, t))),
    )
}

fn scale_about_centre(rect: Rectangle<f64, Logical>, scale: f64) -> Rectangle<f64, Logical> {
    let size = rect.size.upscale(scale);
    let loc = Point::from((
        rect.loc.x + (rect.size.w - size.w) / 2.0,
        rect.loc.y + (rect.size.h - size.h) / 2.0,
    ));
    Rectangle::new(loc, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_rect_endpoints() {
        let a = Rectangle::new(Point::from((0.0, 0.0)), Size::from((100.0, 50.0)));
        let b = Rectangle::new(Point::from((10.0, 20.0)), Size::from((300.0, 150.0)));
        assert_eq!(lerp_rect(a, b, 0.0), a);
        assert_eq!(lerp_rect(a, b, 1.0), b);
    }

    #[test]
    fn pending_size_needs_ack_and_a_new_size() {
        let full = Size::from((2560, 1440));
        let pending = PendingSize::<()>::new(Size::from((1536, 864)), full, Serial::from(10));
        // Acked, but still drawing at the old size (Chromium/Electron).
        assert!(!pending.is_resolved(Some(Serial::from(10)), full));
        // New size, but not acked yet.
        assert!(!pending.is_resolved(Some(Serial::from(9)), Size::from((1536, 864))));
        // Acked and resized, even if not to the exact size (terminal cells).
        assert!(pending.is_resolved(Some(Serial::from(11)), Size::from((1530, 860))));
    }

    #[test]
    fn pending_size_without_snapshot_ends_on_redraw() {
        let full = Size::from((2560, 1440));
        let mut pending = PendingSize::<()>::new(Size::from((1920, 1080)), full, Serial::from(3));
        assert!(!pending.on_commit(Some(Serial::from(3)), full));
        assert!(pending.on_commit(Some(Serial::from(3)), Size::from((1920, 1080))));
        assert!(!pending.is_fading());
    }

    #[test]
    fn pending_size_with_snapshot_fades_once() {
        let full = Size::from((2560, 1440));
        let small = Size::from((1920, 1080));
        let mut pending = PendingSize::new(small, full, Serial::from(3)).with_snapshot(Some(()));
        // Still drawing at the old size: keep squeezing, no fade yet.
        assert!(!pending.on_commit(Some(Serial::from(3)), full));
        assert!(!pending.is_fading());
        // Redrawn: the fade starts instead of ending it.
        assert!(!pending.on_commit(Some(Serial::from(3)), small));
        assert!(pending.is_fading());
        // Later commits neither restart nor end it; window_frame does.
        assert!(!pending.on_commit(Some(Serial::from(4)), small));
        assert!(pending.is_fading());
    }

    #[test]
    fn scaling_keeps_centre() {
        let r = Rectangle::new(Point::from((10.0, 10.0)), Size::from((100.0, 100.0)));
        let s = scale_about_centre(r, 0.5);
        assert_eq!(s.loc, Point::from((35.0, 35.0)));
        assert_eq!(s.size, Size::from((50.0, 50.0)));
    }
}
