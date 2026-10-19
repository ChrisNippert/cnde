//! The colours of windows, for cnde's UI to match
//! (`appearance.adaptive_color`): a maximized window in front tints the bar
//! and dock, and a window under one of cnde's own windows or popups tints
//! its solid backing.
//!
//! A window is drawn small into a texture and read back, only when it
//! committed something new, and at most every `EVERY` per window.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{
        element::{AsRenderElements, Element, surface::WaylandSurfaceRenderElement},
        gles::GlesRenderer,
    },
    desktop::Window,
    reexports::calloop::timer::{TimeoutAction, Timer},
    reexports::wayland_server::{Resource, backend::ObjectId},
    utils::{Buffer, Physical, Point, Rectangle, Scale, Size},
};

/// The most often a window is read back: a playing video commits every
/// frame, and each read waits for the GPU.
const EVERY: Duration = Duration::from_millis(250);
/// The read-back's width, px.
const WIDTH: f64 = 160.0;
/// Logical px along the top and bottom edges that count as "the edge".
const EDGE: f64 = 24.0;
/// How long a window no longer matched keeps its sample, so switching
/// between windows doesn't read them back again (or drop to no colour
/// for a moment).
const KEEP: Duration = Duration::from_secs(10);
/// A read-back with less than this share drawn (a frame not in yet, or
/// a see-through window) keeps the last colour.
const MIN_DRAWN: f32 = 0.5;

/// A window's colours, sRGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Along its top edge (its header) and its bottom edge.
    pub top: [u8; 3],
    pub bottom: [u8; 3],
    /// Over the whole window.
    pub main: [u8; 3],
}

#[derive(Default)]
pub struct Tints {
    samples: HashMap<ObjectId, Entry>,
    /// A window is waiting out `EVERY`: look again then.
    pub retry_at: Option<Instant>,
}

struct Entry {
    sample: Sample,
    /// The commits it was taken at.
    commits: u64,
    at: Instant,
    /// When it was last wanted.
    wanted: Instant,
}

impl Tints {
    pub fn get(&self, window: &Window) -> Option<Sample> {
        Some(self.samples.get(&key(window)?)?.sample)
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.retry_at = None;
    }

    /// Bring the samples of `windows` up to date, and forget the rest.
    /// Whether any changed.
    pub fn update(&mut self, renderer: &mut GlesRenderer, windows: &[Window], now: Instant) -> bool {
        for window in windows {
            if let Some(entry) = key(window).and_then(|id| self.samples.get_mut(&id)) {
                entry.wanted = now;
            }
        }
        self.samples.retain(|_, e| now < e.wanted + KEEP);
        self.retry_at = None;
        let mut changed = false;
        for window in windows {
            let Some(id) = key(window) else {
                continue;
            };
            let geometry = window.geometry();
            if geometry.size.w <= 0 || geometry.size.h <= 0 {
                continue;
            }
            let scale = Scale::from(WIDTH / geometry.size.w as f64);
            let size: Size<i32, Physical> = geometry.size.to_f64().to_physical(scale).to_i32_ceil();
            let location: Point<i32, Physical> =
                (Point::default() - geometry.loc).to_physical_precise_round(scale);
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                window.render_elements(renderer, location, scale, 1.0);
            let commits = elements.iter().fold(0u64, |acc, e| {
                let count = e.current_commit().distance(Some(0.into())).unwrap_or(0);
                acc.wrapping_mul(31).wrapping_add(count as u64)
            });
            if let Some(entry) = self.samples.get(&id) {
                if entry.commits == commits {
                    continue;
                }
                let due = entry.at + EVERY;
                if due > now {
                    self.retry_at = Some(self.retry_at.map_or(due, |r| r.min(due)));
                    continue;
                }
            }
            let Some(pixels) = read_back(renderer, &elements, size, scale) else {
                continue;
            };
            let (w, h) = (size.w as usize, size.h as usize);
            let edge = ((EDGE * scale.y).ceil() as usize).clamp(1, h);
            let last = self.samples.get(&id).map(|e| e.sample);
            let strip = |from: usize, to: usize, last: Option<[u8; 3]>| {
                let all = &pixels[from * w..to * w];
                let drawn: Vec<[u8; 3]> = all
                    .iter()
                    .filter(|p| p[3] >= 128)
                    .map(|p| [p[0], p[1], p[2]])
                    .collect();
                if (drawn.len() as f32) < all.len() as f32 * MIN_DRAWN {
                    last
                } else {
                    Some(dominant(&drawn))
                }
            };
            let (Some(top), Some(bottom), Some(main)) = (
                strip(0, edge, last.map(|s| s.top)),
                strip(h - edge, h, last.map(|s| s.bottom)),
                strip(0, h, last.map(|s| s.main)),
            ) else {
                continue;
            };
            let sample = Sample { top, bottom, main };
            let entry = Entry {
                sample,
                commits,
                at: now,
                wanted: now,
            };
            if self.samples.insert(id, entry).is_none_or(|old| old.sample != sample) {
                changed = true;
            }
        }
        changed
    }
}

impl crate::state::State {
    /// Sample the windows cnde's UI matches, when it does.
    pub fn update_tints(&mut self) {
        if !self.config.appearance.adaptive_color {
            self.tints.clear();
            return;
        }
        let targets = self.tint_targets();
        let now = Instant::now();
        let Some(renderer) = self.backend.renderer() else {
            return;
        };
        if self.tints.update(renderer, &targets, now) {
            self.queue_redraw();
        }
        if let Some(at) = self.tints.retry_at
            && !self.tint_wake
        {
            self.tint_wake = true;
            let timer = Timer::from_deadline(at);
            let _ = self.loop_handle.insert_source(timer, |_, _, state| {
                state.tint_wake = false;
                TimeoutAction::Drop
            });
        }
    }

    /// The colour the bar on `output` takes: the top edge of the frontmost
    /// window against the top.
    pub fn bar_tint(&self, output: &smithay::output::Output) -> Option<[u8; 3]> {
        if !self.config.appearance.adaptive_color {
            return None;
        }
        let geo = self.space.output_geometry(output)?;
        let top = self.touching_edges(output, geo).top?;
        Some(self.tints.get(&top)?.top)
    }

    /// Per monitor: the frontmost windows against its top and bottom edges
    /// (the bar and dock take their colours; a floating window elsewhere
    /// changes nothing), and the windows under cnde's own windows and
    /// popups.
    fn tint_targets(&self) -> Vec<Window> {
        let mut targets: Vec<Window> = Vec::new();
        let mut add = |window: &Window| {
            if !targets.contains(window) {
                targets.push(window.clone());
            }
        };
        for output in self.space.outputs() {
            let behind = crate::frost::Behind::new(&self.space, output, None);
            if let Some(geo) = self.space.output_geometry(output) {
                let touching = self.touching_edges(output, geo);
                for window in touching.top.iter().chain(&touching.bottom) {
                    add(window);
                }
            }
            for (index, (window, rect)) in behind.windows.iter().enumerate() {
                if crate::frost::WindowFrost::wants(window)
                    && let Some((under, _)) =
                        behind.windows[..index].iter().rev().find(|(_, r)| r.overlaps(*rect))
                {
                    add(under);
                }
            }
            for (_, rect) in crate::frost::shell_popups(output) {
                if let Some((under, _)) = behind.windows.iter().rev().find(|(_, r)| r.overlaps(rect))
                {
                    add(under);
                }
            }
        }
        targets
    }
}

/// `elements` drawn into a `size` texture over nothing, read back as
/// RGBA rows, top down.
fn read_back(
    renderer: &mut GlesRenderer,
    elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> Option<Vec<[u8; 4]>> {
    use smithay::backend::{
        allocator::Fourcc,
        renderer::{Bind, ExportMem, Offscreen, Texture, TextureMapping, damage::OutputDamageTracker},
    };
    let buffer_size = Size::<i32, Buffer>::from((size.w, size.h));
    let mut texture: smithay::backend::renderer::gles::GlesTexture =
        renderer.create_buffer(Fourcc::Abgr8888, buffer_size).ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut tracker = OutputDamageTracker::new(size, scale, smithay::utils::Transform::Normal);
        let result = tracker
            .render_output(renderer, &mut target, 0, elements, [0.0, 0.0, 0.0, 0.0])
            .ok()?;
        let _ = result.sync.wait();
    }
    let rect = Rectangle::<i32, Buffer>::from_size(buffer_size);
    let mapping = renderer.copy_texture(&texture, rect, Fourcc::Abgr8888).ok()?;
    let top_down = mapping.flipped();
    let bytes = renderer.map_texture(&mapping).ok()?;
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    let pixels: Vec<[u8; 4]> = bytes.as_chunks::<4>().0.to_vec();
    if pixels.len() < w * h {
        return None;
    }
    Some(if top_down {
        pixels[..w * h].to_vec()
    } else {
        pixels[..w * h].chunks(w).rev().flatten().copied().collect()
    })
}

fn key(window: &Window) -> Option<ObjectId> {
    Some(window.toplevel()?.wl_surface().id())
}

/// The most common colour among `pixels`, roughly: they're grouped by
/// their top 4 bits per channel and the biggest group is averaged.
pub fn dominant(pixels: &[[u8; 3]]) -> [u8; 3] {
    let mut groups: HashMap<u16, (u32, [u32; 3])> = HashMap::new();
    for p in pixels {
        let bucket = (p[0] as u16 >> 4) << 8 | (p[1] as u16 >> 4) << 4 | p[2] as u16 >> 4;
        let group = groups.entry(bucket).or_default();
        group.0 += 1;
        for (sum, c) in group.1.iter_mut().zip(p) {
            *sum += *c as u32;
        }
    }
    groups
        .into_values()
        .max_by_key(|(count, _)| *count)
        .map_or([0, 0, 0], |(count, sum)| sum.map(|s| (s / count) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominant_is_the_biggest_group() {
        let mut pixels = vec![[30, 30, 32]; 10];
        pixels.extend([[200, 10, 10]; 4]);
        pixels.extend([[31, 29, 33]; 6]);
        assert_eq!(dominant(&pixels), [30, 29, 32]);
        assert_eq!(dominant(&[]), [0, 0, 0]);
    }
}
