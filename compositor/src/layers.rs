// Partly adapted from smithay: anvil/src/shell/mod.rs (layer commit handling)

//! wlr-layer-shell: panels, launchers, wallpapers and other desktop UI that
//! clients draw as part of the desktop rather than as windows. This is what
//! cnde-shell will use (ARCHITECTURE.md §5); until then fuzzel and waybar.
//!
//! Stacking per monitor, bottom to top: background, bottom, windows, top,
//! overlay. A fullscreen window covers the top layer (the panel hides).

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    delegate_layer_shell,
    desktop::{LayerSurface, PopupKind, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle},
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
                WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};
use tracing::{debug, warn};

use crate::state::State;

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        // Clients that don't pick a monitor get the one under the pointer.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| {
                let pointer = self.seat.get_pointer()?.current_location();
                self.space.output_under(pointer).next().cloned()
            })
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            warn!(%namespace, "layer surface but no monitor; closing it");
            surface.send_close();
            return;
        };
        debug!(%namespace, ?layer, output = %output.name(), "new layer surface");
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            warn!(?err, "failed to map layer surface");
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        // cnde's own menus, Waybar tray menus etc.: kept on the monitor.
        self.unconstrain_popup(&popup);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(popup)) {
            warn!(?err, "failed to track layer popup");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (output.clone(), layer))
        });
        let Some((output, layer)) = found else {
            return;
        };
        {
            let mut map = layer_map_for_output(&output);
            map.unmap_layer(&layer);
            map.arrange();
        }
        // e.g. fuzzel closing: hand the keyboard back to the top window.
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|focus| &focus == layer.wl_surface());
        if focused {
            // The overview's search box closing hands focus to whatever the
            // overview picked; otherwise the top window.
            let next = self
                .overview
                .take_focus_after()
                .or_else(|| self.space.elements().last().cloned());
            self.focus_window(next.as_ref());
        }
        self.relayout_after_output_change();
        self.queue_redraw();
        self.queue_pointer_refresh();
    }
}
delegate_layer_shell!(State);

impl State {
    /// Commit handling for layer surfaces: arrange, initial configure,
    /// exclusive keyboard focus. Returns whether `surface` was a layer.
    pub fn handle_layer_commit(&mut self, surface: &WlSurface) -> bool {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| {
                layer_map_for_output(o)
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned()
        else {
            return false;
        };

        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .and_then(|data| data.lock().ok().map(|d| d.initial_configure_sent))
                .unwrap_or(true)
        });

        let (zone_changed, layer) = {
            let mut map = layer_map_for_output(&output);
            let zone_before = map.non_exclusive_zone();
            // Arrange first so the initial configure respects the client's size.
            map.arrange();
            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .cloned();
            (map.non_exclusive_zone() != zone_before, layer)
        };
        let Some(layer) = layer else {
            return true;
        };
        if !initial_configure_sent {
            layer.layer_surface().send_configure();
        }
        // The panel's first buffer ends the black hold at startup.
        if layer.namespace() == "cnde-panel"
            && with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false)
            && self.night_light.release_intro()
        {
            self.queue_redraw();
        }

        // A panel appeared or changed size: maximized/snapped windows refit.
        if zone_changed {
            self.relayout_after_output_change();
        }
        if wants_exclusive_keyboard(&layer) {
            self.focus_layer(&layer);
        }
        self.queue_redraw();
        true
    }

    pub fn is_layer_surface(&self, surface: &WlSurface) -> bool {
        self.space.outputs().any(|output| {
            layer_map_for_output(output)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
    }

    fn focus_layer(&mut self, layer: &LayerSurface) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if self.lock.is_locked() {
            return;
        }
        if keyboard.current_focus().as_ref() != Some(layer.wl_surface()) {
            keyboard.set_focus(
                self,
                Some(layer.wl_surface().clone()),
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
        }
    }

    /// A top/overlay layer that holds the keyboard exclusively (a launcher
    /// that's open). While it exists, window focus changes don't steal input.
    pub fn exclusive_layer(&self) -> Option<LayerSurface> {
        self.space.outputs().find_map(|output| {
            layer_map_for_output(output)
                .layers()
                .rev()
                .find(|layer| wants_exclusive_keyboard(layer))
                .cloned()
        })
    }

    /// The topmost layer surface under `pos` in `layers`, with the surface
    /// and its location in global coordinates.
    pub fn layer_surface_under(
        &self,
        layers: &[Layer],
        pos: Point<f64, Logical>,
    ) -> Option<(LayerSurface, WlSurface, Point<f64, Logical>)> {
        let output = self.space.output_under(pos).next()?;
        let output_geo = self.space.output_geometry(output)?;
        let map = layer_map_for_output(output);
        let local = pos - output_geo.loc.to_f64();
        layers.iter().find_map(|&wanted| {
            let layer = map.layer_under(wanted, local)?;
            let layer_loc = map.layer_geometry(layer)?.loc;
            let (surface, surface_loc) =
                layer.surface_under(local - layer_loc.to_f64(), WindowSurfaceType::ALL)?;
            Some((
                layer.clone(),
                surface,
                (surface_loc + layer_loc + output_geo.loc).to_f64(),
            ))
        })
    }

    /// The part of `output` not reserved by panels, in global coordinates.
    pub fn usable_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let output_geo = self.space.output_geometry(output)?;
        let mut zone = layer_map_for_output(output).non_exclusive_zone();
        zone.loc += output_geo.loc;
        Some(zone)
    }

    /// How far an auto bar or dock floats off its edge while no window
    /// touches it. Its reserved space is the attached one, so the snap zones
    /// and the touch that attaches it measure from where it really sits.
    pub fn auto_float_offsets(&self, output: &Output) -> FloatOffsets {
        use cnde_config::{BarMode, DockPosition};
        let reserving = |namespace: &str| {
            layer_map_for_output(output).layers().any(|layer| {
                layer.namespace() == namespace
                    && matches!(
                        layer.cached_state().exclusive_zone,
                        smithay::wayland::shell::wlr_layer::ExclusiveZone::Exclusive(_)
                    )
            })
        };
        let mut offsets = FloatOffsets::default();
        if self.config.panel.mode == BarMode::Auto && reserving("cnde-panel") {
            offsets.top = cnde_ipc::panel::GAP;
        }
        if self.config.dock.mode == BarMode::Auto && reserving(cnde_ipc::dock::NAMESPACE) {
            let pad = cnde_ipc::dock::EDGE_MARGIN;
            match self.config.dock.position {
                DockPosition::Left => offsets.left = pad,
                DockPosition::Right => offsets.right = pad,
                DockPosition::Bottom => offsets.bottom = pad,
                _ => {}
            }
        }
        offsets
    }

    /// `usable_area` as far as snap zones go: measured from the floating
    /// bar and dock rather than the space they reserve.
    pub fn snap_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let area = self.usable_area(output)?;
        let o = self.auto_float_offsets(output);
        Some(Rectangle::new(
            (area.loc.x + o.left, area.loc.y + o.top).into(),
            (
                (area.size.w - o.left - o.right).max(1),
                (area.size.h - o.top - o.bottom).max(1),
            )
                .into(),
        ))
    }

    /// `usable_area` for tiling: a bar or dock that floats for good keeps
    /// a gap of its own free beyond itself, but tiles are spaced by the
    /// tiling gap alone, so that padding is given back.
    pub fn tiling_area(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        let area = self.usable_area(output)?;
        let reserving = |namespace: &str| {
            layer_map_for_output(output).layers().any(|layer| {
                layer.namespace() == namespace
                    && matches!(
                        layer.cached_state().exclusive_zone,
                        smithay::wayland::shell::wlr_layer::ExclusiveZone::Exclusive(_)
                    )
            })
        };
        let floating = cnde_config::BarMode::Floating;
        let top = if self.config.panel.mode == floating && reserving("cnde-panel") {
            cnde_ipc::panel::GAP
        } else {
            0
        };
        let dock = (self.config.dock.mode == floating && reserving(cnde_ipc::dock::NAMESPACE))
            .then_some(self.config.dock.position);
        Some(give_back_padding(area, top, dock))
    }

    pub fn send_layer_frame_callbacks(&self, output: &Output, elapsed: std::time::Duration) {
        for layer in layer_map_for_output(output).layers() {
            layer.send_frame(output, elapsed, Some(std::time::Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }
}

fn wants_exclusive_keyboard(layer: &LayerSurface) -> bool {
    let state = layer.cached_state();
    state.keyboard_interactivity == KeyboardInteractivity::Exclusive
        && matches!(state.layer, Layer::Top | Layer::Overlay)
}

/// See `State::auto_float_offsets`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FloatOffsets {
    pub top: i32,
    pub left: i32,
    pub right: i32,
    pub bottom: i32,
}

/// `area` grown by the padding a permanently floating bar (`top`) and dock
/// (on its edge) reserve beyond themselves, so tiles end where the bar and
/// dock do.
fn give_back_padding(
    area: Rectangle<i32, Logical>,
    top: i32,
    dock: Option<cnde_config::DockPosition>,
) -> Rectangle<i32, Logical> {
    use cnde_config::DockPosition;
    let pad = cnde_ipc::dock::EDGE_MARGIN;
    let (left, right, bottom) = match dock {
        Some(DockPosition::Left) => (pad, 0, 0),
        Some(DockPosition::Right) => (0, pad, 0),
        Some(DockPosition::Bottom) => (0, 0, pad),
        _ => (0, 0, 0),
    };
    Rectangle::new(
        (area.loc.x - left, area.loc.y - top).into(),
        (area.size.w + left + right, area.size.h + top + bottom).into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cnde_config::DockPosition;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn floating_bar_gives_back_its_padding_above_the_tiles() {
        // A 40px bar floated GAP off the top, GAP of padding below it.
        let g = cnde_ipc::panel::GAP;
        let area = rect(0, 40 + 2 * g, 1000, 800 - 40 - 2 * g);
        let back = give_back_padding(area, g, None);
        assert_eq!(back, rect(0, 40 + g, 1000, 800 - 40 - g));
    }

    #[test]
    fn dock_padding_is_given_back_on_its_own_edge_only() {
        let pad = cnde_ipc::dock::EDGE_MARGIN;
        let area = rect(0, 30, 1000, 700);
        assert_eq!(
            give_back_padding(area, 0, Some(DockPosition::Bottom)),
            rect(0, 30, 1000, 700 + pad)
        );
        assert_eq!(
            give_back_padding(area, 0, Some(DockPosition::Left)),
            rect(-pad, 30, 1000 + pad, 700)
        );
        assert_eq!(
            give_back_padding(area, 0, Some(DockPosition::Right)),
            rect(0, 30, 1000 + pad, 700)
        );
        assert_eq!(give_back_padding(area, 0, Some(DockPosition::Overview)), area);
        assert_eq!(give_back_padding(area, 0, None), area);
    }
}
