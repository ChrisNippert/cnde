//! Builds the per-output list of render elements, front to back.

use crate::downscale::Downscaled;
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
            utils::{CropRenderElement, RescaleRenderElement},
        },
        gles::{GlesRenderer, GlesTexture, element::PixelShaderElement},
    },
    desktop::{Space, Window, layer_map_for_output},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::wayland_server::Resource,
    utils::{Logical, Point, Rectangle, Scale, Size},
    wayland::{compositor::with_states, shell::wlr_layer::Layer},
};
use tracing::warn;

use crate::{
    cursor::Cursors,
    effects::{self, ClosingWindow},
    popup_anim::PopupAnims,
    snap::SnapPreview,
    snapshot::{LastFrame, Snapshot},
    window::{Placement, with_state},
};

/// Scales a rect about the origin, then shifts it: how the overview moves
/// windows from where they really are to where it shows them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zoom {
    pub scale_x: f64,
    pub scale_y: f64,
    pub offset: Point<f64, Logical>,
}

impl Zoom {
    pub const IDENTITY: Zoom = Zoom {
        scale_x: 1.0,
        scale_y: 1.0,
        offset: Point::new(0.0, 0.0),
    };

    /// The zoom that takes `from` exactly onto `to`.
    pub fn mapping(from: Rectangle<f64, Logical>, to: Rectangle<f64, Logical>) -> Zoom {
        let scale_x = to.size.w / from.size.w.max(1.0);
        let scale_y = to.size.h / from.size.h.max(1.0);
        Zoom {
            scale_x,
            scale_y,
            offset: Point::from((
                to.loc.x - from.loc.x * scale_x,
                to.loc.y - from.loc.y * scale_y,
            )),
        }
    }

    pub fn apply(self, rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((
                rect.loc.x * self.scale_x + self.offset.x,
                rect.loc.y * self.scale_y + self.offset.y,
            )),
            Size::from((rect.size.w * self.scale_x, rect.size.h * self.scale_y)),
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
    // A fullscreen window, cut off at its monitor's edge.
    WindowClipped = CropRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    Snapshot = RescaleRenderElement<TextureRenderElement<GlesTexture>>,
    Solid = SolidColorRenderElement,
    Cursor = MemoryRenderBufferRenderElement<GlesRenderer>,
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Wallpaper = TextureRenderElement<GlesTexture>,
    Shader = PixelShaderElement,
    Frost = crate::frost::Frost,
    // Under a shell popup, growing and fading with it.
    PopupFrost = RescaleRenderElement<crate::frost::Frost>,
    // A popup's solid backing over a window (see `frost`).
    PopupSolid = RescaleRenderElement<PixelShaderElement>,
    Night = crate::night_light::Filter,
    Titlebar = RescaleRenderElement<MemoryRenderBufferRenderElement<GlesRenderer>>,
    // Drawn small (thumbnails): averaged down, see `downscale`.
    SmallWindow = Downscaled<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    SmallWindowClipped = Downscaled<CropRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>>,
    SmallTitlebar = Downscaled<RescaleRenderElement<MemoryRenderBufferRenderElement<GlesRenderer>>>,
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

/// The icon a drag-and-drop source asked for, hanging off the pointer (the
/// surface's own offset is its hotspot). Above everything but the cursor.
pub fn dnd_icon_elements(
    renderer: &mut GlesRenderer,
    icon: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    pointer: Point<f64, Logical>,
    output_geo: Rectangle<i32, Logical>,
    scale: Scale<f64>,
) -> Vec<OutputElement> {
    if !icon.is_alive() || !output_geo.to_f64().contains(pointer) {
        return Vec::new();
    }
    let location = (pointer - output_geo.loc.to_f64())
        .to_physical(scale)
        .to_i32_round();
    render_elements_from_surface_tree(renderer, icon, location, scale, 1.0, Kind::Unspecified)
        .into_iter()
        .map(OutputElement::Surface)
        .collect()
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
    // Frosted glass under the dock (see `frost`): just below the layers
    // above the windows.
    frost: Vec<OutputElement>,
    // Just above the windows (the overview's close button).
    over_windows: Vec<OutputElement>,
    decorations: &mut crate::decorations::Decorations,
    popups: &mut PopupAnims,
    // Frost under cnde's own floating windows; None for solid.
    mut window_frost: Option<crate::frost::WindowFrost<'_>>,
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
        // The topmost window it covers, for a solid backing under a cnde
        // window's see-through parts.
        let under = windows[..index]
            .iter()
            .rev()
            .find(|below| {
                let bar = crate::decorations::titlebar_height(&below.window) as f64;
                let rect = Rectangle::new(
                    Point::from((below.rect.loc.x, below.rect.loc.y - bar)),
                    Size::from((below.rect.size.w, below.rect.size.h + bar)),
                );
                !with_state(&below.window, |s| s.minimized) && rect.overlaps(placed.rect)
            })
            .map(|below| &below.window);
        layers.push(window_elements(
            renderer,
            decorations,
            window_frost.as_mut(),
            under,
            &placed.window,
            placed.rect,
            placed.zoom,
            output_loc,
            scale,
            now,
            1.0,
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
    // Judged by the windows that sit on this monitor: one that merely reaches
    // over from the next monitor must not un-fullscreen it.
    let fullscreen = !overview
        && windows
            .iter()
            .rev()
            .find(|p| output_geo.to_f64().contains(p.rect.loc + p.rect.size.downscale(2.0).to_point()))
            .is_some_and(|p| with_state(&p.window, |s| s.placement == Placement::Fullscreen));
    let mut layer = |renderer: &mut GlesRenderer, which| {
        layer_elements(renderer, popups, output, output_loc, which, scale, now)
    };
    let mut elements = layer(renderer, Layer::Overlay);
    if !fullscreen {
        elements.extend(layer(renderer, Layer::Top));
        elements.extend(frost);
    }
    elements.extend(over_windows);
    elements.extend(window_elements);
    if fullscreen {
        elements.extend(layer(renderer, Layer::Top));
    }
    if overview {
        elements.extend(backdrop);
    } else {
        elements.extend(layer(renderer, Layer::Bottom));
        elements.extend(layer(renderer, Layer::Background));
    }
    // Popups that just closed, fading out on top of everything.
    let output_geo = Rectangle::new(
        output_loc,
        output.current_mode().map_or_else(Default::default, |mode| {
            mode.size.to_f64().to_logical(scale.x)
        }),
    );
    let mut closing = popups.closing_elements(output_geo, scale, now);
    closing.extend(elements);
    closing
}

/// The overlay layer only (the screenshot tool, over a frozen screen).
pub fn overlay_elements(
    renderer: &mut GlesRenderer,
    popups: &mut PopupAnims,
    output: &Output,
    output_loc: Point<f64, Logical>,
    now: Instant,
) -> Vec<OutputElement> {
    let scale = Scale::from(output.current_scale().fractional_scale());
    layer_elements(
        renderer,
        popups,
        output,
        output_loc,
        Layer::Overlay,
        scale,
        now,
    )
}

/// Every layer surface of one kind on `output`, top first, each with its
/// popups (animated, see `popup_anim`) above it.
fn layer_elements(
    renderer: &mut GlesRenderer,
    popups: &mut PopupAnims,
    output: &Output,
    output_loc: Point<f64, Logical>,
    layer: Layer,
    scale: Scale<f64>,
    now: Instant,
) -> Vec<OutputElement> {
    let map = layer_map_for_output(output);
    let mut elements = Vec::new();
    for surface in map.layers_on(layer).rev() {
        let Some(geometry) = map.layer_geometry(surface) else {
            continue;
        };
        let location = geometry.loc.to_physical_precise_round(scale);
        elements.extend(popups.layer_popup_elements(
            renderer,
            surface.wl_surface(),
            location,
            output_loc,
            scale,
            now,
        ));
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                surface.wl_surface(),
                location,
                scale,
                1.0,
                Kind::Unspecified,
            );
        elements.extend(surfaces.into_iter().map(OutputElement::Surface));
    }
    elements
}

/// One live window, top element first: the crossfading snapshot (if a
/// resize or a refit is running) above the window's own surfaces.
#[allow(clippy::too_many_arguments)]
pub(crate) fn window_elements(
    renderer: &mut GlesRenderer,
    decorations: &mut crate::decorations::Decorations,
    frost: Option<&mut crate::frost::WindowFrost<'_>>,
    under: Option<&Window>,
    win: &Window,
    // Where the window geometry is laid out (space coordinates).
    live: Rectangle<f64, Logical>,
    zoom: Zoom,
    output_loc: smithay::utils::Point<f64, Logical>,
    scale: Scale<f64>,
    now: Instant,
    // Multiplies the window's own opacity (1 for a window as it is).
    opacity: f32,
) -> Vec<OutputElement> {
    let bar = crate::decorations::titlebar_height(win) as f64;
    with_state(win, |state| {
        let mut frame = effects::window_frame(state, live, now);
        frame.alpha *= opacity;
        if !win.geometry().is_empty() {
            state.last_frame = Some(LastFrame::record(renderer, win, scale, frame.rect));
        }
        let zoomed = zoom.apply(frame.rect);
        let rect = Rectangle::new(zoomed.loc - output_loc, zoomed.size);
        let mut elements = Vec::new();

        if let (Some(alpha), Some(snapshot)) =
            (frame.snapshot_alpha, effects::fading_snapshot(state))
        {
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
        // A fullscreen window can be bigger than its monitor (Wine keeps a
        // hidden frame around the game); don't let it bleed onto a neighbour.
        let clip = (state.placement == Placement::Fullscreen)
            .then(|| rect.to_physical_precise_round::<_, i32>(scale));
        let surfaces: Vec<_> = surfaces
            .into_iter()
            .map(|surface| RescaleRenderElement::from_element(surface, origin, stretch))
            .collect();
        // Only crop when something actually sticks out: the common case
        // draws exactly as an unclipped window does.
        let clip = clip.filter(|clip| {
            surfaces
                .iter()
                .any(|surface| !clip.contains_rect(smithay::backend::renderer::element::Element::geometry(surface, scale)))
        });
        elements.extend(surfaces.into_iter().filter_map(|surface| match clip {
            Some(clip) => CropRenderElement::from_element(surface, scale, clip)
                .map(OutputElement::WindowClipped),
            None => Some(OutputElement::Window(surface)),
        }));

        // cnde's title bar, just above the geometry. It grows with the
        // window as it opens; otherwise it keeps its height.
        if bar > 0.0 {
            let grow = if state.open_anim.is_some() {
                frame.rect.size.w / live.size.w.max(1.0)
            } else {
                1.0
            };
            let height = bar * grow;
            let unzoomed = Rectangle::new(
                Point::from((frame.rect.loc.x, frame.rect.loc.y - height)),
                Size::from((frame.rect.size.w, height)),
            );
            let zoomed = zoom.apply(unzoomed);
            let rect = Rectangle::new(zoomed.loc - output_loc, zoomed.size);
            let width = live.size.w.round() as i32;
            if let Some(element) =
                decorations.element(renderer, win, state, rect, width, scale, frame.alpha)
            {
                elements.push(element);
            }
        }
        // Drawn well below its size: averaged, so it stays legible.
        let shrink = 1.0 / stretch.x.min(stretch.y).max(1e-3);
        if shrink >= crate::downscale::MIN_SHRINK
            && let Some(program) = decorations.downscale.program(renderer)
        {
            let program = &program;
            elements = elements
                .into_iter()
                .map(|element| match element {
                    OutputElement::Window(e) => {
                        OutputElement::SmallWindow(Downscaled::new(e, program.clone(), shrink))
                    }
                    OutputElement::WindowClipped(e) => OutputElement::SmallWindowClipped(
                        Downscaled::new(e, program.clone(), shrink),
                    ),
                    OutputElement::Titlebar(e) => {
                        OutputElement::SmallTitlebar(Downscaled::new(e, program.clone(), shrink))
                    }
                    other => other,
                })
                .collect();
        }
        // Last, so under it all: the blurred wallpaper its see-through
        // parts show.
        if let Some(frost) = frost
            && crate::frost::WindowFrost::wants(win)
        {
            let key = format!("{:?}", win.toplevel().map(|t| t.wl_surface().id()));
            elements.extend(frost.elements(renderer, &key, rect, frame.alpha, under));
        }
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
