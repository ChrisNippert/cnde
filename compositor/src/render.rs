//! Builds the per-output list of render elements, front to back.

use std::time::Instant;

use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, render_elements, solid::SolidColorRenderElement,
            surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
            utils::RescaleRenderElement,
        },
        gles::{GlesRenderer, GlesTexture},
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Rectangle, Scale},
};

use crate::{
    effects::{self, ClosingWindow},
    snap::SnapPreview,
    snapshot::Snapshot,
    window::with_state,
};

render_elements! {
    pub OutputElement<=GlesRenderer>;
    // Every element is wrapped in a rescale so animations can move and stretch
    // it; at rest the scale is exactly 1.
    Window = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Snapshot = RescaleRenderElement<TextureRenderElement<GlesTexture>>,
    Solid = SolidColorRenderElement,
}

pub fn output_elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    closing: &[ClosingWindow],
    snap_preview: Option<&mut SnapPreview>,
    output: &Output,
    now: Instant,
) -> Vec<OutputElement> {
    let Some(output_geo) = space.output_geometry(output) else {
        return Vec::new();
    };
    let output_loc = output_geo.loc.to_f64();
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Build bottom to top, so closing windows can slot back into the stacking
    // position they had; then flip, since the renderer wants top first.
    let windows: Vec<&Window> = space.elements_for_output(output).collect();
    let mut layers: Vec<Vec<OutputElement>> = Vec::with_capacity(windows.len() + closing.len());
    let mut top_window_layer = None;
    for (index, win) in windows.iter().enumerate() {
        for closed in closing.iter().filter(|c| c.below == index) {
            layers.push(closing_elements(closed, output_loc, scale, now));
        }
        top_window_layer = Some(layers.len());
        layers.push(window_elements(
            renderer, space, win, output_loc, scale, now,
        ));
    }
    for closed in closing.iter().filter(|c| c.below >= windows.len()) {
        layers.push(closing_elements(closed, output_loc, scale, now));
    }
    // The snap preview sits just below the window being dragged (always the
    // topmost one, since grabbing raises it).
    if let Some(preview) = snap_preview {
        let elements = preview
            .render_elements(output_loc, scale, now)
            .into_iter()
            .map(OutputElement::Solid)
            .collect();
        layers.insert(top_window_layer.unwrap_or(layers.len()), elements);
    }

    layers.into_iter().rev().flatten().collect()
}

/// One live window, top element first: the crossfading snapshot (if a
/// resize is running) above the window's own surfaces.
fn window_elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    win: &Window,
    output_loc: smithay::utils::Point<f64, Logical>,
    scale: Scale<f64>,
    now: Instant,
) -> Vec<OutputElement> {
    let Some(live) = space.element_geometry(win) else {
        return Vec::new();
    };
    with_state(win, |state| {
        let frame = effects::window_frame(state, live.to_f64(), now);
        let rect = Rectangle::new(frame.rect.loc - output_loc, frame.rect.size);
        let mut elements = Vec::new();

        if let (Some(alpha), Some(snapshot)) = (
            frame.snapshot_alpha,
            state.resize_anim.as_ref().and_then(|r| r.snapshot.as_ref()),
        ) {
            elements.push(snapshot_element(snapshot, rect, scale, alpha * frame.alpha));
        }

        // Draw the surfaces so the window geometry's top-left lands on
        // `rect.loc`, then stretch about that point to fill `rect`.
        let geometry = win.geometry();
        let origin = rect.loc.to_physical(scale).to_i32_round();
        let render_location = (rect.loc - geometry.loc.to_f64())
            .to_physical(scale)
            .to_i32_round();
        let stretch = Scale {
            x: rect.size.w / geometry.size.w.max(1) as f64,
            y: rect.size.h / geometry.size.h.max(1) as f64,
        };
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            win.render_elements(renderer, render_location, scale, frame.alpha);
        elements.extend(surfaces.into_iter().map(|surface| {
            OutputElement::Window(RescaleRenderElement::from_element(surface, origin, stretch))
        }));
        elements
    })
}

fn closing_elements(
    closed: &ClosingWindow,
    output_loc: smithay::utils::Point<f64, Logical>,
    scale: Scale<f64>,
    now: Instant,
) -> Vec<OutputElement> {
    let (snapshot, rect, alpha) = closed.frame(now);
    let rect = Rectangle::new(rect.loc - output_loc, rect.size);
    vec![snapshot_element(snapshot, rect, scale, alpha)]
}

fn snapshot_element(
    snapshot: &Snapshot,
    rect: Rectangle<f64, Logical>,
    scale: Scale<f64>,
    alpha: f32,
) -> OutputElement {
    let (element, origin, stretch) = snapshot.render_element(rect, scale, alpha);
    OutputElement::Snapshot(RescaleRenderElement::from_element(element, origin, stretch))
}

/// Whether anything needs another frame soon.
pub fn animations_running(
    space: &Space<Window>,
    closing: &[ClosingWindow],
    snap_preview: Option<&SnapPreview>,
    now: Instant,
) -> bool {
    !closing.is_empty()
        || snap_preview.is_some_and(|p| p.is_animating(now))
        || space
            .elements()
            .any(|w| with_state(w, |s| effects::is_animating(s)))
}
