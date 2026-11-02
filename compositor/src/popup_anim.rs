//! Open and close animations for popups of layer surfaces: the shell's
//! quick settings, bar menus (ARCHITECTURE.md §4.3: clients never animate
//! themselves). A popup fades in while growing slightly from its top edge,
//! and fades out the reverse way from a snapshot of its last frame, since
//! its buffers are gone by the time we learn it closed. The snapshot is
//! taken while drawing, whenever the popup's content changes: by the time
//! the client destroys a popup its surface (and textures) may already be
//! gone, and a snapshot taken then comes out empty.

use std::{collections::HashMap, time::Instant};

use smithay::{
    backend::renderer::utils::{CommitCounter, with_renderer_surface_state},
    backend::renderer::{
        element::{
            Kind,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            utils::RescaleRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::{PopupKind, PopupManager, utils::bbox_from_surface_tree},
    reexports::wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle, Scale},
    wayland::compositor::with_states,
};

use crate::{
    animation::{Animation, presets},
    render::OutputElement,
    snapshot::Snapshot,
    state::State,
};

/// Popups start this much smaller and grow to full size.
const START_SCALE: f64 = 0.94;

/// When a popup was first drawn; kept on the surface itself.
struct Opened(Instant);

/// A popup's last drawn frame, for its close animation.
struct LastPopup {
    snapshot: Option<Snapshot>,
    /// The surface commit `snapshot` shows, and the scale it was taken at.
    commit: CommitCounter,
    scale: Scale<f64>,
    /// Where its geometry was on screen (global).
    rect: Rectangle<f64, Logical>,
}

struct ClosingPopup {
    snapshot: Snapshot,
    rect: Rectangle<f64, Logical>,
    anim: Animation,
}

#[derive(Default)]
pub struct PopupAnims {
    last: HashMap<ObjectId, LastPopup>,
    closing: Vec<ClosingPopup>,
    /// The latest time any popup drawn so far is still opening.
    opening_until: Option<Instant>,
}

/// How far `surface` (a popup) has opened, 0 to 1: its alpha, and its
/// scale is `START_SCALE` on the way to 1. 0 until it's first drawn.
pub fn open_progress(surface: &WlSurface, now: Instant) -> f64 {
    let opened = with_states(surface, |states| states.data_map.get::<Opened>().map(|o| o.0));
    opened.map_or(0.0, |at| Animation::progress_since(at, presets::POPUP_OPEN, now))
}

/// How big a popup is drawn at `progress` through opening (or closing,
/// counting down), relative to its size.
pub fn scale_at(progress: f64) -> f64 {
    START_SCALE + (1.0 - START_SCALE) * progress
}

impl PopupAnims {
    /// The GPU was reset: the pictures of popups that were open or closing
    /// are gone.
    pub fn clear_gpu(&mut self) {
        self.last.clear();
        self.closing.clear();
    }

    /// Popups fading out: where each was (global) and how far it's still
    /// open, 1 to 0.
    pub fn closing(&self, now: Instant) -> impl Iterator<Item = (Rectangle<f64, Logical>, f64)> + '_ {
        self.closing
            .iter()
            .map(move |c| (c.rect, 1.0 - c.anim.value_at(now)))
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.opening_until.is_some_and(|until| until > now) || !self.closing.is_empty()
    }

    pub fn finish(&mut self, now: Instant) {
        self.closing.retain(|c| !c.anim.is_done(now));
    }

    /// Elements for a layer surface's popups, top first, animating any that
    /// just opened. `location` is the layer surface's origin (output-local,
    /// physical); `output_loc` its output's position.
    pub fn layer_popup_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        parent: &WlSurface,
        location: Point<i32, smithay::utils::Physical>,
        output_loc: Point<f64, Logical>,
        scale: Scale<f64>,
        now: Instant,
    ) -> Vec<OutputElement> {
        let mut elements = Vec::new();
        for (popup, offset) in PopupManager::popups_for_surface(parent) {
            let PopupKind::Xdg(xdg) = &popup else {
                continue;
            };
            let surface = xdg.wl_surface();
            let geometry = popup.geometry();
            let opened = with_states(surface, |states| {
                states.data_map.insert_if_missing(|| Opened(now));
                states.data_map.get::<Opened>().map_or(now, |o| o.0)
            });
            let progress = Animation::progress_since(opened, presets::POPUP_OPEN, now);
            if progress < 1.0 {
                let until = opened + presets::POPUP_OPEN.scaled_duration();
                self.opening_until = Some(self.opening_until.map_or(until, |u| u.max(until)));
            }
            let alpha = progress as f32;
            let grow = START_SCALE + (1.0 - START_SCALE) * progress;

            let surface_loc = location + (offset - geometry.loc).to_physical_precise_round(scale);
            // Grow from the middle of the top edge, where it hangs from.
            let geometry_loc = location + offset.to_physical_precise_round(scale);
            let origin = geometry_loc
                + Point::from(((geometry.size.w as f64 * scale.x / 2.0).round() as i32, 0));
            let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    surface,
                    surface_loc,
                    scale,
                    alpha,
                    Kind::Unspecified,
                );
            elements.extend(surfaces.into_iter().map(|element| {
                OutputElement::Window(RescaleRenderElement::from_element(
                    element,
                    origin,
                    Scale::from(grow),
                ))
            }));

            // Keep a snapshot of its current content for when it closes.
            let commit =
                with_renderer_surface_state(surface, |s| s.current_commit()).unwrap_or_default();
            let logical_loc = geometry_loc.to_f64().to_logical(scale) + output_loc;
            let rect = Rectangle::new(logical_loc, geometry.size.to_f64());
            let stale = self.last.get(&surface.id()).is_none_or(|last| {
                last.snapshot.is_none() || last.commit != commit || last.scale != scale
            });
            if stale {
                let bbox = bbox_from_surface_tree(surface, (0, 0));
                let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        renderer,
                        surface,
                        (Point::default() - bbox.loc).to_physical_precise_round(scale),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                let snapshot = Snapshot::from_elements(renderer, elements, scale, geometry, bbox);
                self.last.insert(
                    surface.id(),
                    LastPopup {
                        snapshot,
                        commit,
                        scale,
                        rect,
                    },
                );
            } else if let Some(last) = self.last.get_mut(&surface.id()) {
                last.rect = rect;
            }
        }
        elements
    }

    /// Closing popups on an output at `output_loc`, top first.
    pub fn closing_elements(
        &self,
        output_geo: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        now: Instant,
    ) -> Vec<OutputElement> {
        self.closing
            .iter()
            .filter(|c| c.rect.overlaps(output_geo))
            .map(|closing| {
                let progress = closing.anim.value_at(now);
                let rect = Rectangle::new(closing.rect.loc - output_geo.loc, closing.rect.size);
                let (element, origin, _) =
                    closing
                        .snapshot
                        .render_element(rect, scale, (1.0 - progress) as f32);
                let top_middle =
                    origin + Point::from(((rect.size.w * scale.x / 2.0).round() as i32, 0));
                let shrink = 1.0 - (1.0 - START_SCALE) * progress;
                OutputElement::Snapshot(RescaleRenderElement::from_element(
                    element,
                    top_middle,
                    Scale::from(shrink),
                ))
            })
            .collect()
    }
}

impl State {
    /// A popup closed: animate it out if we drew it.
    pub fn popup_closed(&mut self, surface: &WlSurface) {
        let Some(last) = self.popup_anims.last.remove(&surface.id()) else {
            return;
        };
        if let Some(snapshot) = last.snapshot {
            self.popup_anims.closing.push(ClosingPopup {
                snapshot,
                rect: last.rect,
                anim: Animation::new(0.0, 1.0, presets::POPUP_CLOSE),
            });
            self.queue_redraw();
        }
    }
}
