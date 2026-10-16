//! Editing the dock and the top bar in place, started from their
//! right-click menus. Changes are written to config.toml as they're made,
//! and the usual reload applies them.
//!
//! Dock: drag pinned icons to reorder them, × to unpin, drag the grip to a
//! screen edge to move the dock, −/+ (or the scroll wheel) for icon size.
//! Top bar: drag the bottom edge for height, drag the clock to the left,
//! the centre or the right. Both: Done to finish.

use cnde_config::{ClockPosition, DockPosition};
use smithay_client_toolkit::seat::pointer::{BTN_LEFT, PointerEvent, PointerEventKind};

use crate::{
    App,
    context_menus::write_config,
    dock::{Control, DockDrag, DockEdit, EditHit},
    menu::Value,
    panel::{EditTarget, PanelDrag, PanelEdit, clock_slot},
};

/// Icon size steps for −/+ and the wheel.
const ICON_STEP: i64 = 8;
const ICON_RANGE: (i64, i64) = (24, 128);
/// The grip must travel this far before a release moves the dock.
const MOVE_THRESHOLD: f64 = 40.0;
/// Margins the dock keeps from the screen edge (as in `dock`).
const EDGE_MARGIN: f64 = 8.0;
const OVERVIEW_MARGIN: f64 = 20.0;

impl App {
    pub fn start_dock_edit(&mut self) {
        self.dock_editing = true;
        self.apply_dock_edit();
    }

    pub fn finish_dock_edit(&mut self) {
        self.dock_editing = false;
        self.apply_dock_edit();
    }

    /// Match the dock to `dock_editing` (also after it's recreated).
    pub fn apply_dock_edit(&mut self) {
        let icon = self.config.dock.icon_size as f32;
        if let Some(dock) = &mut self.dock
            && dock.edit.is_some() != self.dock_editing
        {
            dock.edit = self.dock_editing.then(DockEdit::default);
            dock.relayout(icon);
        }
        self.draw_dock();
    }

    pub fn start_panel_edit(&mut self) {
        for i in 0..self.panels.len() {
            self.panels[i].edit = Some(PanelEdit::default());
            self.draw_panel(i);
        }
    }

    pub fn finish_panel_edit(&mut self) {
        for i in 0..self.panels.len() {
            self.panels[i].edit = None;
            self.draw_panel(i);
        }
    }

    pub fn panel_editing(&self) -> bool {
        self.panels.iter().any(|p| p.edit.is_some())
    }

    /// Pointer input on the dock while it's being edited.
    pub fn dock_edit_pointer(&mut self, event: &PointerEvent) {
        let icon = self.config.dock.icon_size as f32;
        // Work out what the event means, then act (acting needs `self`).
        let step = {
            let Some(dock) = &mut self.dock else {
                return;
            };
            if dock.edit.is_none() {
                return;
            }
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    let edit = dock.edit.as_mut().expect("checked above");
                    match &mut edit.drag {
                        Some(DockDrag::Icon { at, .. } | DockDrag::Move { at }) => {
                            *at = event.position;
                            DockStep::Redraw
                        }
                        None => {
                            if dock.hover(Some(event.position), icon) {
                                DockStep::Redraw
                            } else {
                                DockStep::Nothing
                            }
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    let dragging = dock.edit.as_ref().is_some_and(|e| e.drag.is_some());
                    if !dragging && dock.hover(None, icon) {
                        DockStep::Redraw
                    } else {
                        DockStep::Nothing
                    }
                }
                PointerEventKind::Press {
                    button: BTN_LEFT, ..
                } => {
                    let hit = dock.edit_hit(event.position, icon);
                    let edit = dock.edit.as_mut().expect("checked above");
                    match hit {
                        Some(EditHit::Unpin(index)) => DockStep::Unpin(index),
                        Some(EditHit::Icon(index)) => {
                            edit.drag = Some(DockDrag::Icon {
                                index,
                                at: event.position,
                            });
                            DockStep::Redraw
                        }
                        Some(EditHit::Control(Control::Move)) => {
                            edit.drag = Some(DockDrag::Move { at: event.position });
                            self.dock_drag_start = Some(event.position);
                            DockStep::Redraw
                        }
                        Some(EditHit::Control(Control::Smaller)) => DockStep::Size(-ICON_STEP),
                        Some(EditHit::Control(Control::Bigger)) => DockStep::Size(ICON_STEP),
                        Some(EditHit::Control(Control::Done)) => DockStep::Done,
                        None => DockStep::Nothing,
                    }
                }
                PointerEventKind::Release {
                    button: BTN_LEFT, ..
                } => match dock.edit.as_mut().and_then(|e| e.drag.take()) {
                    Some(DockDrag::Icon { index, at }) => {
                        DockStep::Reorder(index, dock.drop_index(at, icon))
                    }
                    Some(DockDrag::Move { at }) => DockStep::Move(at),
                    None => DockStep::Nothing,
                },
                PointerEventKind::Axis { vertical, .. } => {
                    let notches = if vertical.value120 != 0 {
                        vertical.value120 as f64 / 120.0
                    } else {
                        vertical.absolute / 30.0
                    };
                    if notches.abs() >= 0.5 {
                        DockStep::Size(-notches.signum() as i64 * ICON_STEP / 2)
                    } else {
                        DockStep::Nothing
                    }
                }
                _ => DockStep::Nothing,
            }
        };
        match step {
            DockStep::Nothing => {}
            DockStep::Redraw => self.draw_dock(),
            DockStep::Unpin(index) => {
                let id = self
                    .dock
                    .as_ref()
                    .and_then(|d| d.items().get(index))
                    .and_then(|i| i.app.clone());
                if let Some(id) = id {
                    let mut list = self.pinned_order();
                    list.retain(|f| *f != id);
                    write_config(&[("dock.favorites", Value::List(list))]);
                }
            }
            DockStep::Reorder(from, to) => {
                self.reorder_pinned(from, to);
                self.draw_dock();
            }
            DockStep::Move(at) => {
                let start = self.dock_drag_start.take().unwrap_or(at);
                let moved = (at.0 - start.0).hypot(at.1 - start.1);
                if moved > MOVE_THRESHOLD
                    && let Some(edge) = self.dock_edge_at(at)
                    && edge != self.config.dock.position
                {
                    let name = match edge {
                        DockPosition::Left => "left",
                        DockPosition::Right => "right",
                        _ => "bottom",
                    };
                    write_config(&[("dock.position", Value::Str(name.into()))]);
                }
                self.draw_dock();
            }
            DockStep::Size(step) => self.step_icon_size(step),
            DockStep::Done => self.finish_dock_edit(),
        }
    }

    /// Pinned apps in their dock order, plus pinned ones that aren't
    /// installed (kept, so a reinstall brings them back in place).
    fn pinned_order(&self) -> Vec<String> {
        let shown: Vec<String> = self
            .dock
            .as_ref()
            .map(|d| {
                d.items()
                    .iter()
                    .filter(|i| i.favorite)
                    .filter_map(|i| i.app.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut list = shown.clone();
        list.extend(
            self.config
                .dock
                .favorites
                .iter()
                .filter(|f| !shown.contains(f))
                .cloned(),
        );
        list
    }

    fn reorder_pinned(&mut self, from: usize, to: usize) {
        let mut list = self.pinned_order();
        if from >= list.len() {
            return;
        }
        let id = list.remove(from);
        // `to` counted the carried icon itself when it's further along.
        let to = if to > from { to - 1 } else { to }.min(list.len());
        list.insert(to, id);
        if list != self.config.dock.favorites {
            write_config(&[("dock.favorites", Value::List(list))]);
        }
    }

    fn step_icon_size(&mut self, step: i64) {
        let size = (self.config.dock.icon_size as i64 + step).clamp(ICON_RANGE.0, ICON_RANGE.1);
        if size != self.config.dock.icon_size as i64 {
            write_config(&[("dock.icon_size", Value::Int(size))]);
        }
    }

    /// The screen edge nearest to `at` (dock-surface coordinates).
    fn dock_edge_at(&self, at: (f64, f64)) -> Option<DockPosition> {
        let dock = self.dock.as_ref()?;
        let (w, h) = self
            .outputs
            .info(&dock.output)?
            .logical_size
            .map(|(w, h)| (w as f64, h as f64))?;
        let (dw, dh) = dock.size();
        let (dw, dh) = (dw as f64, dh as f64);
        // Where the dock surface is on its monitor.
        let origin = match dock.position() {
            DockPosition::Left => (EDGE_MARGIN, (h - dh) / 2.0),
            DockPosition::Right => (w - EDGE_MARGIN - dw, (h - dh) / 2.0),
            DockPosition::Overview => ((w - dw) / 2.0, h - OVERVIEW_MARGIN - dh),
            _ => ((w - dw) / 2.0, h - EDGE_MARGIN - dh),
        };
        let (x, y) = (origin.0 + at.0, origin.1 + at.1);
        let distances = [
            (DockPosition::Left, x),
            (DockPosition::Right, w - x),
            (DockPosition::Bottom, h - y),
        ];
        distances
            .into_iter()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(edge, _)| edge)
    }

    /// Pointer input on a panel while it's being edited.
    pub fn panel_edit_pointer(&mut self, index: usize, event: &PointerEvent) {
        let Some(panel) = self.panels.get_mut(index) else {
            return;
        };
        let Some(drag) = panel.edit.as_ref().map(|e| e.drag) else {
            return;
        };
        match event.kind {
            PointerEventKind::Motion { .. } => match drag {
                Some(PanelDrag::Height {
                    start_y,
                    start_height,
                }) => {
                    let height = (start_height as f64 + event.position.1 - start_y)
                        .round()
                        .clamp(16.0, 96.0) as u32;
                    panel.set_height(height);
                }
                Some(PanelDrag::Clock { .. }) => {
                    set_panel_drag(
                        panel,
                        Some(PanelDrag::Clock {
                            x: event.position.0 as f32,
                        }),
                    );
                    self.draw_panel(index);
                }
                None => {}
            },
            PointerEventKind::Press {
                button: BTN_LEFT, ..
            } => match panel.edit_target(event.position) {
                Some(EditTarget::Done) => self.finish_panel_edit(),
                Some(EditTarget::Height) => {
                    let start_height = panel.height();
                    set_panel_drag(
                        panel,
                        Some(PanelDrag::Height {
                            start_y: event.position.1,
                            start_height,
                        }),
                    );
                }
                Some(EditTarget::Clock) => {
                    set_panel_drag(
                        panel,
                        Some(PanelDrag::Clock {
                            x: event.position.0 as f32,
                        }),
                    );
                    self.draw_panel(index);
                }
                None => {}
            },
            PointerEventKind::Release {
                button: BTN_LEFT, ..
            } => {
                set_panel_drag(panel, None);
                let (height, width) = (panel.height(), panel.width());
                match drag {
                    Some(PanelDrag::Height { .. }) => {
                        if height != self.config.panel.height {
                            write_config(&[("panel.height", Value::Int(height as i64))]);
                        }
                    }
                    Some(PanelDrag::Clock { x }) => {
                        let slot = clock_slot(x, width as f32);
                        self.draw_panel(index);
                        if slot != self.config.panel.clock_position {
                            let name = match slot {
                                ClockPosition::Left => "left",
                                ClockPosition::Center => "center",
                                ClockPosition::Right => "right",
                            };
                            write_config(&[("panel.clock_position", Value::Str(name.into()))]);
                        }
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}

fn set_panel_drag(panel: &mut crate::panel::Panel, drag: Option<PanelDrag>) {
    if let Some(edit) = &mut panel.edit {
        edit.drag = drag;
    }
}

/// What a pointer event on the dock comes to, while editing.
enum DockStep {
    Nothing,
    Redraw,
    Unpin(usize),
    /// Move the pinned icon at the first index to the second.
    Reorder(usize, usize),
    /// The grip was released here.
    Move((f64, f64)),
    Size(i64),
    Done,
}
