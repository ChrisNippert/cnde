//! Builds the per-output list of render elements, front to back.

use std::time::Instant;

use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, surface::WaylandSurfaceRenderElement, utils::RescaleRenderElement,
        },
        gles::GlesRenderer,
    },
    desktop::{Space, Window},
    output::Output,
    utils::Scale,
};

use crate::window;

/// Every window surface is wrapped in a rescale element so animations can
/// scale it about its centre; at rest the scale is exactly 1.
pub type WindowElement = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>;

pub fn output_elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    output: &Output,
    now: Instant,
) -> Vec<WindowElement> {
    let Some(output_geo) = space.output_geometry(output) else {
        return Vec::new();
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    let mut elements = Vec::new();
    // The space stores elements bottom to top; the renderer wants top first.
    for win in space.elements_for_output(output).rev() {
        let Some(location) = space.element_location(win) else {
            continue;
        };
        let geometry = win.geometry();
        let render_location =
            (location - geometry.loc - output_geo.loc).to_physical_precise_round(scale);
        let centre = (location - output_geo.loc + geometry.size.downscale(2).to_point())
            .to_physical_precise_round(scale);

        let look = window::animated_look(win, now);
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            win.render_elements(renderer, render_location, scale, look.alpha);
        elements.extend(
            surfaces
                .into_iter()
                .map(|element| RescaleRenderElement::from_element(element, centre, look.scale)),
        );
    }
    elements
}
