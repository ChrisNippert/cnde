//! Screen locking (ext-session-lock-v1). A locker (cnde-lock, swaylock,
//! ...) asks to lock; from then on each monitor shows only the locker's
//! surface and only it gets input, until the locker unlocks. If the locker
//! dies instead, the screen stays locked and the locker is started again.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::{
    backend::renderer::{
        element::{
            Kind, surface::WaylandSurfaceRenderElement, surface::render_elements_from_surface_tree,
        },
        gles::GlesRenderer,
    },
    delegate_session_lock,
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::{Logical, Point, SERIAL_COUNTER, Scale},
    wayland::session_lock::{
        LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
    },
};
use tracing::{info, warn};

use crate::{render::OutputElement, state::State};

/// How long the screen may be locked with no locker showing before one is
/// started again (it may just be starting up).
const RESPAWN_AFTER: Duration = Duration::from_secs(2);

pub struct Lock {
    pub manager: SessionLockManagerState,
    locked: bool,
    /// Locking: confirmed once a locked frame has been drawn.
    pending: Option<SessionLocker>,
    surfaces: Vec<(LockSurface, Output)>,
    /// When the locker was last seen gone (or started).
    orphaned_since: Option<Instant>,
}

impl Lock {
    pub fn new(manager: SessionLockManagerState) -> Self {
        Self {
            manager,
            locked: false,
            pending: None,
            surfaces: Vec::new(),
            orphaned_since: None,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// The locker's surface on `output`.
    pub fn surface(&self, output: &Output) -> Option<&WlSurface> {
        self.surfaces
            .iter()
            .find(|(s, o)| o == output && s.alive())
            .map(|(s, _)| s.wl_surface())
    }

    fn any_surface(&self) -> Option<&WlSurface> {
        self.surfaces
            .iter()
            .find(|(s, _)| s.alive())
            .map(|(s, _)| s.wl_surface())
    }

    /// What `output` shows while locked: the locker's surface, or nothing
    /// (the clear colour) until it has one.
    pub fn elements(&self, renderer: &mut GlesRenderer, output: &Output) -> Vec<OutputElement> {
        let Some(surface) = self.surface(output) else {
            return Vec::new();
        };
        let scale = Scale::from(output.current_scale().fractional_scale());
        render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            surface,
            (0, 0),
            scale,
            1.0,
            Kind::Unspecified,
        )
        .into_iter()
        .map(OutputElement::Surface)
        .collect()
    }

    /// A locked frame is on screen: tell the locker it's locked.
    pub fn frame_drawn(&mut self) {
        if let Some(locker) = self.pending.take() {
            locker.lock();
            info!("screen locked");
        }
    }
}

impl State {
    /// Super+L: start the locker (it does the locking).
    pub fn lock_screen(&mut self) {
        if self.lock.locked {
            return;
        }
        self.spawn_locker();
    }

    fn spawn_locker(&mut self) {
        let command = self.config.lock.command.trim().to_string();
        if command.is_empty() {
            warn!("no lock command set ([lock] command)");
            return;
        }
        crate::spawn::spawn_command_line(&command, &self.client_env());
    }

    /// Each frame while locked: if the locker has gone without unlocking,
    /// start it again.
    fn check_locker(&mut self) {
        if !self.lock.locked {
            return;
        }
        if self.lock.any_surface().is_some() {
            self.lock.orphaned_since = None;
            return;
        }
        let since = *self.lock.orphaned_since.get_or_insert_with(Instant::now);
        if since.elapsed() > RESPAWN_AFTER {
            warn!("the locker went away while locked; starting it again");
            self.lock.orphaned_since = Some(Instant::now());
            // smithay remembers which monitors the dead locker covered and
            // would refuse the new one's surfaces; start its bookkeeping
            // afresh (behind a global no client can see).
            self.lock.surfaces.clear();
            self.lock.manager =
                SessionLockManagerState::new::<State, _>(&self.display_handle, |_| false);
            self.spawn_locker();
        }
    }

    /// The locker's surface under `pos`: while locked, nothing else is.
    pub fn lock_surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.space.output_under(pos).next()?;
        let loc = self.space.output_geometry(output)?.loc.to_f64();
        Some((self.lock.surface(output)?.clone(), loc))
    }

    fn focus_lock_surface(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        // The one under the pointer, else any.
        let pointer = self
            .seat
            .get_pointer()
            .map(|p| p.current_location())
            .unwrap_or_default();
        let surface = self
            .lock_surface_under(pointer)
            .map(|(s, _)| s)
            .or_else(|| self.lock.any_surface().cloned());
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.lock.manager
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // A new locker while locked: it takes over only from one that died
        // (else any app could lock again and then unlock), and is locked at
        // once, there being nothing else on screen to hide. Dropping the
        // confirmation refuses it.
        if self.lock.locked {
            if self.lock.any_surface().is_none() {
                confirmation.lock();
            }
            return;
        }
        // Nothing else may keep the keyboard or the screen from here on.
        self.lock.locked = true;
        self.night_light.skip_intro();
        // Watch for the locker dying (nothing else would notice: with the
        // screen locked and no locker, nothing redraws).
        let _ = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            |_, _, state| {
                if !state.lock.locked {
                    return TimeoutAction::Drop;
                }
                state.check_locker();
                TimeoutAction::ToDuration(Duration::from_secs(1))
            },
        );
        self.lock.pending = Some(confirmation);
        self.lock.orphaned_since = Some(Instant::now());
        if self.overview.is_open() {
            self.set_overview(false);
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
        self.queue_redraw();
    }

    fn unlock(&mut self) {
        info!("screen unlocked");
        self.lock.locked = false;
        self.lock.pending = None;
        self.lock.surfaces.clear();
        self.lock.orphaned_since = None;
        let top = self
            .workspaces
            .list
            .get(self.workspaces.active)
            .and_then(|ws| ws.windows.last())
            .cloned();
        self.focus_window(top.as_ref());
        self.queue_pointer_refresh();
        self.queue_redraw();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        if let Some(geo) = self.space.output_geometry(&output) {
            surface.with_pending_state(|state| {
                state.size = Some((geo.size.w as u32, geo.size.h as u32).into());
            });
            surface.send_configure();
        }
        self.lock
            .surfaces
            .retain(|(s, o)| s.alive() && *o != output);
        self.lock.surfaces.push((surface, output));
        self.focus_lock_surface();
        self.queue_redraw();
    }
}
delegate_session_lock!(State);
