//! Editing the dock and the top bar in place, started from their
//! right-click menus. Changes are written to config.toml as they're made,
//! and the usual reload applies them.
//!
//! Dock: drag pinned icons to reorder them, × to unpin, drag the grip to a
//! screen edge to move the dock, −/+ (or the scroll wheel) for icon size.
//! Top bar: drag the bottom edge for height, drag the clock to the left,
//! the centre or the right. Both: Done to finish.

use cnde_config::{ClockPosition, DockPosition};
use cnde_ui::{Canvas, Color};
use smithay_client_toolkit::{
    reexports::client::protocol::wl_shm,
    seat::pointer::{BTN_LEFT, PointerEvent, PointerEventKind},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
    },
};
use tracing::warn;

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

/// Where a dragged dock would land: an outline on that screen edge.
pub struct DockPreview {
    pub layer: LayerSurface,
    edge: DockPosition,
    size: (u32, u32),
    configured: bool,
}

impl App {
    pub fn start_dock_edit(&mut self) {
        self.dock_editing = true;
        self.apply_dock_edit();
    }

    pub fn finish_dock_edit(&mut self) {
        self.dock_editing = false;
        self.dock_preview = None;
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
            DockStep::Redraw => {
                self.draw_dock();
                self.update_dock_preview();
            }
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
                self.dock_preview = None;
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

    /// While the grip is dragged: show where the dock would land.
    fn update_dock_preview(&mut self) {
        let Some(dock) = &self.dock else {
            return;
        };
        let Some(DockDrag::Move { at }) = dock.edit.as_ref().and_then(|e| e.drag) else {
            self.dock_preview = None;
            return;
        };
        let start = self.dock_drag_start.unwrap_or(at);
        let edge = ((at.0 - start.0).hypot(at.1 - start.1) > MOVE_THRESHOLD)
            .then(|| self.dock_edge_at(at))
            .flatten()
            .filter(|edge| *edge != self.config.dock.position);
        let Some(edge) = edge else {
            self.dock_preview = None;
            return;
        };
        if self.dock_preview.as_ref().is_some_and(|p| p.edge == edge) {
            return;
        }
        let Some(dock) = &self.dock else {
            return;
        };
        // The dock's length and thickness, turned to fit the new edge.
        let (w, h) = dock.size();
        let (long, thick) = if w >= h { (w, h) } else { (h, w) };
        let (anchor, size) = match edge {
            DockPosition::Left => (Anchor::LEFT, (thick, long)),
            DockPosition::Right => (Anchor::RIGHT, (thick, long)),
            _ => (Anchor::BOTTOM, (long, thick)),
        };
        // A stretched dock would stretch along the new edge too: the
        // compositor sizes the preview (see `dock_preview_configured`).
        let stretch = self.config.dock.extend && edge != DockPosition::Overview;
        let (anchor, size, margin) = match (stretch, edge) {
            (false, _) => (anchor, size, EDGE_MARGIN as i32),
            (true, DockPosition::Left | DockPosition::Right) => {
                (anchor | Anchor::TOP | Anchor::BOTTOM, (thick, 0), 0)
            }
            (true, _) => (anchor | Anchor::LEFT | Anchor::RIGHT, (0, thick), 0),
        };
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("cnde-dock-preview"),
            Some(&dock.output),
        );
        layer.set_anchor(anchor);
        layer.set_margin(margin, margin, margin, margin);
        layer.set_size(size.0, size.1);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
            // Never in the way of the pointer.
            layer
                .wl_surface()
                .set_input_region(Some(region.wl_region()));
        }
        layer.commit();
        self.dock_preview = Some(DockPreview {
            layer,
            edge,
            size,
            configured: false,
        });
    }

    /// The preview's layer surface was configured: draw it.
    pub fn dock_preview_configured(&mut self, size: (u32, u32)) {
        let Some(preview) = &mut self.dock_preview else {
            return;
        };
        preview.configured = true;
        if size.0 > 0 && size.1 > 0 {
            preview.size = size;
        }
        let scale = self.dock.as_ref().map_or(1, |d| d.scale());
        let (w, h) = (preview.size.0 as i32 * scale, preview.size.1 as i32 * scale);
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate dock preview");
                return;
            }
        };
        if let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) {
            let (lw, lh) = (preview.size.0 as f32, preview.size.1 as f32);
            let accent = self.theme.accent;
            let radius = if self.config.dock.extend {
                0.0
            } else {
                cnde_ipc::dock::RADIUS as f32
            };
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            canvas.rounded_rect(0.0, 0.0, lw, lh, radius, accent.with_alpha(0.22));
            canvas.stroke_rounded_rect(0.0, 0.0, lw, lh, radius, 2.0, accent);
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = preview.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
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
                    panel.update_input_region(&self.compositor);
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
