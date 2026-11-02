//! Screenshots (ARCHITECTURE.md §5.2): Super+Shift+S or Print Screen.
//!
//! GNOME style: the screen freezes the moment you press it (every monitor
//! is copied into a texture and drawn instead of the live desktop), and
//! cnde-shell draws the selection tool on top. When you pick a region, a
//! monitor or a window, the shot is cut from the frozen copy (a window is
//! drawn on its own, so overlapping windows don't get in), saved as a PNG
//! under ~/Pictures/Screenshots and put on the clipboard.
//!
//! Without the shell (no IPC client), the key captures the monitor under
//! the pointer straight away.

use std::{collections::HashMap, os::fd::OwnedFd, path::PathBuf, sync::Arc, thread};

use cnde_ipc::{Event, ScreenshotTarget, ScreenshotWindow};
use image::RgbaImage;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen, TextureMapping,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, RenderElement,
                surface::WaylandSurfaceRenderElement,
                texture::{TextureBuffer, TextureRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
            utils::with_renderer_surface_state,
        },
    },
    desktop::layer_map_for_output,
    output::Output,
    reexports::calloop::{
        RegistrationToken, channel,
        timer::{TimeoutAction, Timer},
    },
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::selection::data_device::set_data_device_selection,
};
use tracing::{info, warn};

use crate::{state::State, window::with_state};

struct Frozen {
    texture: TextureBuffer<GlesTexture>,
    raw: GlesTexture,
    size: Size<i32, Physical>,
    scale: f64,
}

#[derive(Default)]
pub struct Screenshot {
    pub active: bool,
    /// Monitors still to be frozen (they're copied as they next render).
    freezing: Vec<String>,
    frozen: HashMap<String, Frozen>,
    /// Every monitor is frozen: the shell should be told (once).
    ready: bool,
    /// The last screenshot, offered on the clipboard as image/png.
    pub clipboard: Option<Arc<Vec<u8>>>,
    saved: Option<channel::Sender<Saved>>,
    /// Ends the freeze if the tool hasn't appeared in time (see `WATCHDOG`).
    watchdog: Option<RegistrationToken>,
}

/// How long the shell has to show its tool before the freeze is dropped, so
/// a stuck shell can't leave the screen frozen for good.
const WATCHDOG: std::time::Duration = std::time::Duration::from_secs(2);

struct Saved {
    path: Option<PathBuf>,
    png: Option<Arc<Vec<u8>>>,
}

impl Screenshot {
    /// The GPU was reset: the frozen copies are gone. While the tool is up,
    /// freeze again from what the monitors show now.
    pub fn clear_gpu(&mut self) {
        let names: Vec<String> = self.frozen.drain().map(|(name, _)| name).collect();
        if self.active {
            for name in names {
                if !self.freezing.contains(&name) {
                    self.freezing.push(name);
                }
            }
        }
    }

    pub fn wants_freeze(&self, output: &Output) -> bool {
        self.active && self.freezing.contains(&output.name())
    }

    /// The frozen copy of `output`, drawn in place of the desktop.
    pub fn frozen_element(&self, output: &Output) -> Option<TextureRenderElement<GlesTexture>> {
        let frozen = self.frozen.get(&output.name())?;
        let logical = frozen.size.to_f64().to_logical(frozen.scale).to_i32_round();
        Some(crate::wallpaper::full_screen(
            &frozen.texture,
            frozen.size.w,
            frozen.size.h,
            logical,
            None,
        ))
    }

    /// Copy what `output` shows (`elements`, without the pointer).
    pub fn freeze<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        elements: &[E],
    ) {
        let name = output.name();
        self.freezing.retain(|n| n != &name);
        let (Some(mode), scale) = (
            output.current_mode(),
            output.current_scale().fractional_scale(),
        ) else {
            return;
        };
        let size = output.current_transform().transform_size(mode.size);
        match render_to_texture(renderer, elements, size, Scale::from(scale)) {
            Some(raw) => {
                let texture =
                    TextureBuffer::from_texture(renderer, raw.clone(), 1, Transform::Normal, None);
                self.frozen.insert(
                    name,
                    Frozen {
                        texture,
                        raw,
                        size,
                        scale,
                    },
                );
            }
            None => warn!(output = %name, "can't freeze the screen for a screenshot"),
        }
        if self.freezing.is_empty() {
            self.ready = true;
        }
    }
}

impl State {
    pub fn start_screenshot(&mut self) {
        if self.screenshot.active {
            return;
        }
        if self.overview.is_open() {
            self.set_overview(false);
        }
        if !self.ipc.has_clients() {
            // No shell to draw the tool: the monitor under the pointer, now.
            if let Some(output) = self.output_under_pointer() {
                self.screenshot.active = true;
                self.screenshot.freezing = vec![output.name()];
                self.screenshot.ready = false;
                self.pending_quick_shot = Some(output.name());
                self.start_screenshot_watchdog();
                self.queue_redraw();
            }
            return;
        }
        info!("screenshot tool opened");
        self.screenshot.active = true;
        self.screenshot.ready = false;
        self.screenshot.freezing = self.space.outputs().map(|o| o.name()).collect();
        self.screenshot.frozen.clear();
        self.start_screenshot_watchdog();
        self.queue_pointer_refresh();
        self.queue_redraw();
    }

    fn start_screenshot_watchdog(&mut self) {
        let timer =
            self.loop_handle
                .insert_source(Timer::from_duration(WATCHDOG), |_, _, state| {
                    state.screenshot.watchdog = None;
                    if state.screenshot.active && !state.screenshot_tool_shown() {
                        warn!("the screenshot tool didn't appear; unfreezing");
                        state.cancel_screenshot();
                    }
                    TimeoutAction::Drop
                });
        match timer {
            Ok(token) => self.screenshot.watchdog = Some(token),
            Err(err) => warn!(?err, "can't start the screenshot watchdog"),
        }
    }

    /// The shell's tool has drawn on some monitor.
    fn screenshot_tool_shown(&self) -> bool {
        self.space.outputs().any(|output| {
            layer_map_for_output(output).layers().any(|layer| {
                layer.namespace() == "cnde-screenshot"
                    && with_renderer_surface_state(layer.wl_surface(), |s| s.buffer().is_some())
                        .unwrap_or(false)
            })
        })
    }

    /// Every loop iteration: once everything is frozen, open the tool (or
    /// take the quick shot).
    pub fn screenshot_ready_check(&mut self) {
        if !std::mem::take(&mut self.screenshot.ready) {
            return;
        }
        if let Some(name) = self.pending_quick_shot.take() {
            self.take_screenshot(ScreenshotTarget::Output { name });
            return;
        }
        // Windows the shell can offer in window mode, top first.
        let windows = self
            .space
            .elements()
            .rev()
            .filter_map(|window| {
                let geo = self.space.element_geometry(window)?;
                Some(ScreenshotWindow {
                    id: with_state(window, |s| s.id),
                    x: geo.loc.x,
                    y: geo.loc.y,
                    w: geo.size.w,
                    h: geo.size.h,
                })
            })
            .collect();
        self.ipc.broadcast(&Event::ScreenshotUi { windows });
    }

    pub fn cancel_screenshot(&mut self) {
        self.end_screenshot();
        self.ipc.broadcast(&Event::ScreenshotDone { path: None });
    }

    fn end_screenshot(&mut self) {
        if let Some(token) = self.screenshot.watchdog.take() {
            self.loop_handle.remove(token);
        }
        self.screenshot.active = false;
        self.pending_quick_shot = None;
        self.screenshot.freezing.clear();
        self.screenshot.frozen.clear();
        self.queue_pointer_refresh();
        self.queue_redraw();
    }

    pub fn take_screenshot(&mut self, target: ScreenshotTarget) {
        let image = self.cut(&target);
        self.end_screenshot();
        let Some(image) = image else {
            warn!(?target, "screenshot failed");
            self.ipc.broadcast(&Event::ScreenshotDone { path: None });
            return;
        };
        let sender = self.screenshot_sender();
        // PNG encoding takes a moment for a big screen: off the main thread.
        let _ = thread::Builder::new()
            .name("screenshot".into())
            .spawn(move || {
                let _ = sender.send(save(image));
            });
    }

    fn screenshot_sender(&mut self) -> channel::Sender<Saved> {
        if let Some(sender) = &self.screenshot.saved {
            return sender.clone();
        }
        let (tx, rx) = channel::channel::<Saved>();
        let inserted = self.loop_handle.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg(saved) = event {
                state.screenshot_saved(saved);
            }
        });
        if let Err(err) = inserted {
            warn!(?err, "can't receive screenshots");
        }
        self.screenshot.saved = Some(tx.clone());
        tx
    }

    fn screenshot_saved(&mut self, saved: Saved) {
        if let Some(png) = saved.png {
            // On the clipboard; served from memory (see `send_clipboard`).
            self.screenshot.clipboard = Some(png);
            set_data_device_selection(
                &self.display_handle,
                &self.seat,
                vec!["image/png".into()],
                (),
            );
        }
        let path = saved.path.map(|p| p.to_string_lossy().into_owned());
        if let Some(path) = &path {
            info!(%path, "screenshot saved");
        }
        self.ipc.broadcast(&Event::ScreenshotDone { path });
    }

    /// The pixels for `target`, from the frozen screen (windows: drawn alone).
    fn cut(&mut self, target: &ScreenshotTarget) -> Option<RgbaImage> {
        match target {
            ScreenshotTarget::Output { name } => {
                let frozen = self.screenshot.frozen.get(name)?;
                let rect = Rectangle::from_size(Size::from((frozen.size.w, frozen.size.h)));
                let raw = frozen.raw.clone();
                read_texture(self.backend.renderer()?, &raw, rect)
            }
            ScreenshotTarget::Region { x, y, w, h } => {
                let region = Rectangle::<i32, Logical>::new((*x, *y).into(), (*w, *h).into());
                // The monitor holding the region's centre; cut to it.
                let centre = region.loc + Point::from((region.size.w / 2, region.size.h / 2));
                let output = self
                    .space
                    .outputs()
                    .find(|o| {
                        self.space
                            .output_geometry(o)
                            .is_some_and(|g| g.contains(centre))
                    })?
                    .clone();
                let geo = self.space.output_geometry(&output)?;
                let local = region.intersection(geo)?;
                let frozen = self.screenshot.frozen.get(&output.name())?;
                let local = Rectangle::new(local.loc - geo.loc, local.size);
                let physical = local
                    .to_f64()
                    .to_physical(frozen.scale)
                    .to_i32_round::<i32>();
                let rect = Rectangle::<i32, Buffer>::new(
                    (physical.loc.x, physical.loc.y).into(),
                    (physical.size.w.max(1), physical.size.h.max(1)).into(),
                );
                let raw = frozen.raw.clone();
                read_texture(self.backend.renderer()?, &raw, rect)
            }
            ScreenshotTarget::Window { id } => {
                let window = self
                    .space
                    .elements()
                    .find(|w| with_state(w, |s| s.id) == *id)?
                    .clone();
                let scale = self
                    .output_for(&window)
                    .map_or(1.0, |o| o.current_scale().fractional_scale());
                let scale = Scale::from(scale);
                let geometry = window.geometry();
                let size = geometry.size.to_f64().to_physical(scale).to_i32_round();
                let renderer = self.backend.renderer()?;
                let location: Point<i32, Physical> =
                    (Point::default() - geometry.loc).to_physical_precise_round(scale);
                let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    window.render_elements(renderer, location, scale, 1.0);
                let texture = render_to_texture(renderer, &elements, size, scale)?;
                read_texture(
                    renderer,
                    &texture,
                    Rectangle::from_size(Size::from((size.w, size.h))),
                )
            }
        }
    }

    /// A client pasted our screenshot.
    pub fn send_clipboard(&self, mime_type: &str, fd: OwnedFd) {
        if mime_type != "image/png" {
            return;
        }
        let Some(png) = self.screenshot.clipboard.clone() else {
            return;
        };
        // The reader may be slow; never block the compositor on it.
        let _ = thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut file = std::fs::File::from(fd);
                let _ = std::io::Write::write_all(&mut file, &png);
            });
    }
}

/// Draw `elements` into a new texture of `size`.
pub(crate) fn render_to_texture<E: RenderElement<GlesRenderer>>(
    renderer: &mut GlesRenderer,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> Option<GlesTexture> {
    let buffer_size = Size::<i32, Buffer>::from((size.w, size.h));
    let mut texture: GlesTexture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        let result = tracker
            .render_output(renderer, &mut target, 0, elements, [0.0, 0.0, 0.0, 1.0])
            .ok()?;
        let _ = result.sync.wait();
    }
    Some(texture)
}

/// Read `rect` of `texture` back as an image, the right way up.
pub(crate) fn read_texture(
    renderer: &mut GlesRenderer,
    texture: &GlesTexture,
    rect: Rectangle<i32, Buffer>,
) -> Option<RgbaImage> {
    let mapping = renderer
        .copy_texture(texture, rect, Fourcc::Abgr8888)
        .map_err(|err| warn!(?err, "screenshot readback failed"))
        .ok()?;
    // "Flipped" relative to GL's bottom-left origin: rows run top down.
    let top_down = mapping.flipped();
    let pixels = renderer.map_texture(&mapping).ok()?;
    let (w, h) = (rect.size.w as u32, rect.size.h as u32);
    let row = w as usize * 4;
    if pixels.len() < row * h as usize {
        return None;
    }
    let mut image = RgbaImage::new(w, h);
    for y in 0..h as usize {
        let src = if top_down { y } else { h as usize - 1 - y };
        image.as_flat_samples_mut().samples[y * row..(y + 1) * row]
            .copy_from_slice(&pixels[src * row..(src + 1) * row]);
    }
    // Shots are opaque; make sure nothing translucent sneaks in.
    for pixel in image.pixels_mut() {
        pixel[3] = 255;
    }
    Some(image)
}

/// Encode and write to ~/Pictures/Screenshots (GNOME's naming).
fn save(image: RgbaImage) -> Saved {
    let mut png = Vec::new();
    let encoded = image::codecs::png::PngEncoder::new(&mut png);
    let ok = image::ImageEncoder::write_image(
        encoded,
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    );
    if let Err(err) = ok {
        warn!(%err, "can't encode screenshot");
        return Saved {
            path: None,
            png: None,
        };
    }
    let png = Arc::new(png);
    let path = screenshots_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let name = chrono::Local::now()
            .format("Screenshot from %Y-%m-%d %H-%M-%S.png")
            .to_string();
        let path = dir.join(name);
        match std::fs::write(&path, png.as_slice()) {
            Ok(()) => Some(path),
            Err(err) => {
                warn!(path = %path.display(), %err, "can't save screenshot");
                None
            }
        }
    });
    Saved {
        path,
        png: Some(png),
    }
}

/// $XDG_PICTURES_DIR/Screenshots, else ~/Pictures/Screenshots.
fn screenshots_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let pictures = std::fs::read_to_string(home.join(".config/user-dirs.dirs"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let value = line.strip_prefix("XDG_PICTURES_DIR=")?;
                let value = value
                    .trim_matches('"')
                    .replace("$HOME", &home.to_string_lossy());
                Some(PathBuf::from(value))
            })
        })
        .unwrap_or_else(|| home.join("Pictures"));
    Some(pictures.join("Screenshots"))
}
