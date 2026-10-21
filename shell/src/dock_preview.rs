//! Hover previews for the dock: rest the pointer on a running app's icon and
//! the compositor draws live thumbnails of its windows beside it (it already
//! does so for Alt+Tab). This is the timing: a short wait before the first
//! shows, instant switching between neighbours, a short grace before it goes
//! so the pointer can cross to it. The compositor tells us when the pointer
//! is on the panel (and handles clicks on it).

use std::time::Duration;

use cnde_config::DockPosition;
use cnde_ipc::{DockIcon, Request, Side};
use smithay_client_toolkit::reexports::calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};

use crate::App;

/// Resting on an icon this long shows the first preview.
pub const SHOW_AFTER: Duration = Duration::from_millis(400);
/// Off the icon and the panel this long hides it.
pub const HIDE_AFTER: Duration = Duration::from_millis(250);

/// (dock, item) in `App::docks`.
type Target = (usize, usize);

#[derive(Default)]
pub struct Preview {
    /// What the compositor is showing.
    shown: Option<Target>,
    /// The running icon under the pointer.
    hovered: Option<Target>,
    /// The pointer's on the panel.
    inside: bool,
    /// An icon that was just clicked: no preview until the pointer's been
    /// elsewhere.
    clicked: Option<Target>,
    /// A click opened it (the compositor keeps it until a pick, Escape or a
    /// click elsewhere): hovering no longer moves or hides it.
    pinned: bool,
    show_timer: Option<(RegistrationToken, Target)>,
    hide_timer: Option<RegistrationToken>,
    /// The icon positions last sent to the compositor, per monitor.
    icons_sent: Vec<(String, Vec<DockIcon>)>,
}

/// Which side of its icon the panel opens on for a dock at `position`.
pub fn side_for(position: DockPosition) -> Side {
    match position {
        DockPosition::Left => Side::Right,
        DockPosition::Right => Side::Left,
        _ => Side::Above,
    }
}

impl App {
    /// The pointer is at `point` on dock `dock` (`None`: it left).
    pub fn dock_preview_pointer(&mut self, dock: usize, point: Option<(f64, f64)>) {
        let icon = self.config.dock.icon_size as f32;
        self.preview.hovered = point.and_then(|p| {
            let d = self.docks.get(dock)?;
            let (item, _) = d.cell_at(p, icon)?;
            (!d.items().get(item)?.windows.is_empty()).then_some((dock, item))
        });
        self.dock_preview_eval();
    }

    /// The compositor says the pointer moved on or off the panel.
    pub fn dock_preview_inside(&mut self, inside: bool) {
        self.preview.inside = inside;
        self.dock_preview_eval();
    }

    /// The panel went away by itself (a thumbnail was clicked).
    pub fn dock_preview_gone(&mut self) {
        self.preview.shown = None;
        self.preview.inside = false;
        self.preview.pinned = false;
        self.dock_preview_cancel_timers();
    }

    /// Nothing may be previewed: a menu, a drag, editing, the overview.
    fn dock_preview_blocked(&self) -> bool {
        self.menu.is_some()
            || self.overview_open
            || self
                .docks
                .first()
                .is_some_and(|d| d.edit.is_some() || d.drag.is_some())
    }

    fn dock_preview_cancel_timers(&mut self) {
        if let Some((token, _)) = self.preview.show_timer.take() {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.preview.hide_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// A button went down on the dock: the panel goes, and doesn't come back
    /// for that icon until the pointer has left it.
    pub fn dock_preview_press(&mut self, left: bool) {
        // A left click on an app with several windows opens the panel
        // (or keeps it): the click itself decides, so leave it be.
        if left && self.preview.hovered.is_some_and(|t| self.dock_preview_multi(t)) {
            return;
        }
        self.preview.clicked = self.preview.hovered;
        self.dock_preview_hide();
    }

    /// Take the panel down now (a click, a menu, a drag).
    pub fn dock_preview_hide(&mut self) {
        self.dock_preview_cancel_timers();
        self.preview.inside = false;
        self.preview.pinned = false;
        if self.preview.shown.take().is_some() {
            self.ipc.send(&Request::DockPreviewHide);
        }
    }

    fn dock_preview_multi(&self, (dock, item): Target) -> bool {
        self.docks
            .get(dock)
            .and_then(|d| d.items().get(item))
            .is_some_and(|i| i.windows.len() > 1)
    }

    /// A click on an icon with several windows: show its panel now and keep
    /// it there until a window is picked, Escape, or a click elsewhere.
    pub fn dock_preview_pin(&mut self, dock: usize, item: usize) {
        if self.dock_preview_blocked() {
            return;
        }
        self.dock_preview_cancel_timers();
        self.dock_preview_show((dock, item));
        if self.preview.shown == Some((dock, item)) {
            self.preview.pinned = true;
            self.preview.clicked = None;
            self.ipc.send(&Request::DockPreviewPin);
        }
    }

    fn dock_preview_eval(&mut self) {
        if self.dock_preview_blocked() {
            self.preview.hovered = None;
            return self.dock_preview_hide();
        }
        if self.preview.pinned {
            return;
        }
        if self.preview.clicked.is_some() && self.preview.clicked != self.preview.hovered {
            self.preview.clicked = None;
        }
        match self.preview.hovered {
            Some(target) if self.preview.clicked == Some(target) => {}
            Some(target) => {
                if let Some(token) = self.preview.hide_timer.take() {
                    self.loop_handle.remove(token);
                }
                if self.preview.shown == Some(target) {
                    self.dock_preview_cancel_show();
                } else if self.preview.shown.is_some() {
                    // A neighbour while one's up: switch at once.
                    self.dock_preview_cancel_show();
                    self.dock_preview_show(target);
                } else if self.preview.show_timer.map(|(_, t)| t) != Some(target) {
                    self.dock_preview_cancel_show();
                    let timer = self.loop_handle.insert_source(
                        Timer::from_duration(SHOW_AFTER),
                        move |_, _, app: &mut App| {
                            app.preview.show_timer = None;
                            if app.preview.hovered == Some(target) {
                                app.dock_preview_show(target);
                            }
                            TimeoutAction::Drop
                        },
                    );
                    self.preview.show_timer = timer.ok().map(|t| (t, target));
                }
            }
            None if self.preview.inside => {
                self.dock_preview_cancel_show();
                if let Some(token) = self.preview.hide_timer.take() {
                    self.loop_handle.remove(token);
                }
            }
            None => {
                self.dock_preview_cancel_show();
                if self.preview.shown.is_some() && self.preview.hide_timer.is_none() {
                    let timer = self.loop_handle.insert_source(
                        Timer::from_duration(HIDE_AFTER),
                        |_, _, app: &mut App| {
                            app.preview.hide_timer = None;
                            if app.preview.hovered.is_none() && !app.preview.inside {
                                app.dock_preview_hide();
                            }
                            TimeoutAction::Drop
                        },
                    );
                    self.preview.hide_timer = timer.ok();
                }
            }
        }
    }

    fn dock_preview_cancel_show(&mut self) {
        if let Some((token, _)) = self.preview.show_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Ask the compositor for the panel for `target`; hides it if the icon's
    /// gone or has no windows.
    fn dock_preview_show(&mut self, (dock, item): Target) {
        let icon = self.config.dock.icon_size as f32;
        let request = self.docks.get(dock).and_then(|d| {
            let windows = d.items().get(item)?.windows.clone();
            let cell = d.cell_rect(item, icon)?;
            let output = self.outputs.info(&d.output)?.name?;
            (!windows.is_empty()).then(|| Request::DockPreview {
                output,
                windows,
                cell: [cell.x, cell.y, cell.w, cell.h].map(f64::from),
                side: side_for(d.position()),
            })
        });
        match request {
            Some(request) => {
                self.ipc.send(&request);
                self.preview.shown = Some((dock, item));
            }
            None => self.dock_preview_hide(),
        }
    }

    /// Tell the compositor where the dock's icons are, if that changed, so
    /// windows minimize into them and grow back out.
    pub fn dock_icons_sync(&mut self) {
        let icon = self.config.dock.icon_size as f32;
        let mut now = Vec::new();
        for dock in &self.docks {
            let Some(output) = self.outputs.info(&dock.output).and_then(|i| i.name) else {
                continue;
            };
            let icons: Vec<DockIcon> = dock
                .items()
                .iter()
                .enumerate()
                .filter(|(_, item)| !item.windows.is_empty())
                .filter_map(|(i, item)| {
                    let cell = dock.cell_rect(i, icon)?;
                    Some(DockIcon {
                        windows: item.windows.clone(),
                        cell: [cell.x, cell.y, cell.w, cell.h].map(|v| f64::from(v).round()),
                    })
                })
                .collect();
            now.push((output, icons));
        }
        for (output, icons) in &now {
            if !self
                .preview
                .icons_sent
                .iter()
                .any(|(o, i)| o == output && i == icons)
            {
                self.ipc.send(&Request::DockIcons {
                    output: output.clone(),
                    icons: icons.clone(),
                });
            }
        }
        self.preview.icons_sent = now;
    }

    /// The dock's items changed: refresh the panel, or drop it.
    pub fn dock_preview_sync(&mut self) {
        if let Some(target) = self.preview.shown {
            if self.dock_preview_blocked() {
                self.dock_preview_hide();
            } else {
                self.dock_preview_show(target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_opens_away_from_the_dock() {
        assert_eq!(side_for(DockPosition::Bottom), Side::Above);
        assert_eq!(side_for(DockPosition::Overview), Side::Above);
        assert_eq!(side_for(DockPosition::Left), Side::Right);
        assert_eq!(side_for(DockPosition::Right), Side::Left);
    }
}
