//! Builds the per-output list of render elements, front to back.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements, Kind,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            solid::SolidColorRenderElement,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            texture::TextureRenderElement,
            utils::RescaleRenderElement,
        },
        gles::{GlesRenderer, GlesTexture},
    },
    desktop::{Space, Window, layer_map_for_output},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::wayland_server::Resource,
    utils::{Logical, Point, Rectangle, Scale},
    wayland::{compositor::with_states, shell::wlr_layer::Layer},
};
use tracing::warn;

use crate::{
    cursor::Cursors,
    effects::{self, ClosingWindow},
    snap::SnapPreview,
    snapshot::{LastFrame, Snapshot},
    window::{Placement, with_state},
};

/// Scales a rect about the origin, then shifts it: the overview's zoom-out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zoom {
    pub scale: f64,
    pub offset: Point<f64, Logical>,
}

impl Zoom {
    pub const IDENTITY: Zoom = Zoom {
        scale: 1.0,
        offset: Point::new(0.0, 0.0),
    };

    pub fn apply(self, rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((
                rect.loc.x * self.scale + self.offset.x,
                rect.loc.y * self.scale + self.offset.y,
            )),
            rect.size.upscale(self.scale),
        )
    }
}

/// A window and where to draw it (space coordinates, before `zoom`).
pub struct Placed {
    pub window: Window,
    pub rect: Rectangle<f64, Logical>,
    pub zoom: Zoom,
    /// Index of its workspace in the strip.
    pub workspace: usize,
}

render_elements! {
    pub OutputElement<=GlesRenderer>;
    // Every element is wrapped in a rescale so animations can move and stretch
    // it; at rest the scale is exactly 1.
    Window = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Snapshot = RescaleRenderElement<TextureRenderElement<GlesTexture>>,
    Solid = SolidColorRenderElement,
    Cursor = MemoryRenderBufferRenderElement<GlesRenderer>,
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Wallpaper = TextureRenderElement<GlesTexture>,
}

/// The pointer, if it's on this output: a themed image, or the surface a
/// client set as its cursor. `Kind::Cursor` lets the DRM compositor put it on
/// the hardware cursor plane, so moving the mouse doesn't recomposite.
pub fn cursor_elements(
    renderer: &mut GlesRenderer,
    cursors: &mut Cursors,
    status: &mut CursorImageStatus,
    pointer: Point<f64, Logical>,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
    time: Duration,
) -> Vec<OutputElement> {
    if !output_geo.to_f64().contains(pointer) {
        return Vec::new();
    }
    // Clients may destroy their cursor surface without telling us.
    if let CursorImageStatus::Surface(surface) = status
        && !surface.is_alive()
    {
        *status = CursorImageStatus::default_named();
    }

    let position = pointer - output_geo.loc.to_f64();
    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Named(icon) => {
            let (buffer, hotspot) = cursors.get(*icon, time);
            let location = (position - hotspot.to_f64()).to_physical(scale);
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                &buffer,
                None,
                None,
                None,
                Kind::Cursor,
            ) {
                Ok(element) => vec![OutputElement::Cursor(element)],
                Err(err) => {
                    warn!(?err, "failed to upload cursor image");
                    Vec::new()
                }
            }
        }
        CursorImageStatus::Surface(surface) => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .and_then(|data| data.lock().ok().map(|attrs| attrs.hotspot))
                    .unwrap_or_default()
            });
            let location = (position - hotspot.to_f64())
                .to_physical(scale)
                .to_i32_round();
            render_elements_from_surface_tree(renderer, surface, location, scale, 1.0, Kind::Cursor)
                .into_iter()
                .map(OutputElement::Surface)
                .collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn output_elements(
    renderer: &mut GlesRenderer,
    space: &Space<Window>,
    closing: &[ClosingWindow],
    snap_preview: Option<&mut SnapPreview>,
    // While the workspace strip moves or the overview is open: windows to
    // draw and where (see `State::scene`). Otherwise the space's windows as
    // they are.
    moving: Option<Vec<Placed>>,
    // Drawn below the windows (the overview's workspace cards and dimming);
    // replaces the bottom and background layers when not empty.
    backdrop: Vec<OutputElement>,
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
    let windows: Vec<Placed> = match moving {
        Some(layout) => layout,
        None => space
            .elements_for_output(output)
            .filter_map(|w| {
                Some(Placed {
                    window: w.clone(),
                    rect: space.element_geometry(w)?.to_f64(),
                    zoom: Zoom::IDENTITY,
                    workspace: 0,
                })
            })
            .collect(),
    };
    let mut layers: Vec<Vec<OutputElement>> = Vec::with_capacity(windows.len() + closing.len());
    let mut top_window_layer = None;
    for (index, placed) in windows.iter().enumerate() {
        for closed in closing.iter().filter(|c| c.below == index) {
            layers.push(closing_elements(closed, output_loc, scale, now));
        }
        top_window_layer = Some(layers.len());
        layers.push(window_elements(
            renderer,
            &placed.window,
            placed.rect,
            placed.zoom,
            output_loc,
            scale,
            now,
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
    let window_elements = layers.into_iter().rev().flatten();

    // Layer-shell stacking (see layers.rs); a fullscreen window covers the panel.
    let overview = !backdrop.is_empty();
    let fullscreen = !overview
        && windows
            .last()
            .is_some_and(|p| with_state(&p.window, |s| s.placement == Placement::Fullscreen));
    let mut elements = layer_elements(renderer, output, Layer::Overlay, scale);
    if !fullscreen {
        elements.extend(layer_elements(renderer, output, Layer::Top, scale));
    }
    elements.extend(window_elements);
    if fullscreen {
        elements.extend(layer_elements(renderer, output, Layer::Top, scale));
    }
    if overview {
        elements.extend(backdrop);
    } else {
        elements.extend(layer_elements(renderer, output, Layer::Bottom, scale));
        elements.extend(layer_elements(renderer, output, Layer::Background, scale));
    }
    elements
}

/// Every layer surface of one kind on `output`, top first.
fn layer_elements(
    renderer: &mut GlesRenderer,
    output: &Output,
    layer: Layer,
    scale: Scale<f64>,
) -> Vec<OutputElement> {
    let map = layer_map_for_output(output);
    let mut elements = Vec::new();
    for surface in map.layers_on(layer).rev() {
        let Some(geometry) = map.layer_geometry(surface) else {
            continue;
        };
        let location = geometry.loc.to_physical_precise_round(scale);
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            surface.render_elements(renderer, location, scale, 1.0);
        elements.extend(surfaces.into_iter().map(OutputElement::Surface));
    }
    elements
}

/// One live window, top element first: the crossfading snapshot (if a
/// resize is running) above the window's own surfaces.
fn window_elements(
    renderer: &mut GlesRenderer,
    win: &Window,
    // Where the window geometry is laid out (space coordinates).
    live: Rectangle<f64, Logical>,
    zoom: Zoom,
    output_loc: smithay::utils::Point<f64, Logical>,
    scale: Scale<f64>,
    now: Instant,
) -> Vec<OutputElement> {
    with_state(win, |state| {
        let frame = effects::window_frame(state, live, now);
        if !win.geometry().is_empty() {
            state.last_frame = Some(LastFrame::record(renderer, win, scale, frame.rect));
        }
        let zoomed = zoom.apply(frame.rect);
        let rect = Rectangle::new(zoomed.loc - output_loc, zoomed.size);
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
    workspaces_moving: bool,
    now: Instant,
) -> bool {
    workspaces_moving
        || !closing.is_empty()
        || snap_preview.is_some_and(|p| p.is_animating(now))
        || space
            .elements()
            .any(|w| with_state(w, |s| effects::is_animating(s)))
}
