//! The desktop background (ARCHITECTURE.md §5.2): an image from the config,
//! with light and dark variants, or a built-in gradient.
//!
//! Each monitor gets its own texture at its exact pixel size, so drawing it is
//! a plain 1:1 copy. Decoding and scaling run on a worker thread; until a
//! monitor's texture is ready it keeps showing the previous one (or the clear
//! color), so nothing ever waits on a large image. The decoded image is
//! dropped as soon as it's been scaled: only the per-monitor textures stay.

use std::{collections::HashMap, path::PathBuf, sync::mpsc, thread};

use cnde_config::{ColorScheme, Config, WallpaperFit};
use image::{RgbaImage, imageops::FilterType};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                texture::{TextureBuffer, TextureRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::Output,
    reexports::calloop::{LoopHandle, channel},
    utils::{Physical, Rectangle, Size, Transform},
};
use tracing::{info, warn};

use crate::state::State;

/// What to draw: everything that affects the pixels.
#[derive(Debug, Clone, PartialEq)]
struct Source {
    path: Option<PathBuf>,
    fit: WallpaperFit,
    scheme: ColorScheme,
}

impl Source {
    fn from_config(config: &Config) -> Self {
        let scheme = config.appearance.color_scheme;
        Self {
            path: config.wallpaper.path(scheme),
            fit: config.wallpaper.fit,
            scheme,
        }
    }
}

struct Job {
    output: String,
    size: Size<i32, Physical>,
    source: Source,
    generation: u64,
}

struct Done {
    output: String,
    size: Size<i32, Physical>,
    generation: u64,
    pixels: Vec<u8>,
}

struct Entry {
    size: Size<i32, Physical>,
    generation: u64,
    texture: Option<TextureBuffer<GlesTexture>>,
    /// Scaled pixels waiting to be uploaded (needs the renderer).
    upload: Option<Vec<u8>>,
}

pub struct Wallpapers {
    source: Source,
    /// Bumped whenever `source` changes; textures from older ones are stale.
    generation: u64,
    entries: HashMap<String, Entry>,
    /// (output, size, generation) already sent to the worker.
    requested: HashMap<String, (Size<i32, Physical>, u64)>,
    jobs: mpsc::Sender<Job>,
}

impl Wallpapers {
    pub fn new(handle: &LoopHandle<'static, State>, config: &Config) -> Self {
        let (done_tx, done_rx) = channel::channel::<Done>();
        let (jobs, jobs_rx) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("wallpaper".into())
            .spawn(move || worker(jobs_rx, done_tx));
        let inserted = handle.insert_source(done_rx, |event, _, state| {
            if let channel::Event::Msg(done) = event {
                state.wallpapers.finished(done);
                state.queue_redraw();
            }
        });
        if let Err(err) = inserted {
            warn!(?err, "can't receive wallpapers; using the plain background");
        }
        Self {
            source: Source::from_config(config),
            generation: 0,
            entries: HashMap::new(),
            requested: HashMap::new(),
            jobs,
        }
    }

    /// The config changed; reload if it affects the wallpaper.
    pub fn update_config(&mut self, config: &Config) {
        let source = Source::from_config(config);
        if source != self.source {
            self.source = source;
            self.generation += 1;
        }
    }

    fn finished(&mut self, done: Done) {
        let entry = self.entries.entry(done.output).or_insert(Entry {
            size: done.size,
            generation: done.generation,
            texture: None,
            upload: None,
        });
        // A newer result may already be here (jobs can finish out of order
        // across a fast config change); keep the newest.
        if done.generation >= entry.generation || entry.texture.is_none() {
            entry.size = done.size;
            entry.generation = done.generation;
            entry.upload = Some(done.pixels);
        }
    }

    /// The background for `output`, or `None` to show the clear color.
    pub fn element(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        let name = output.name();
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);
        let scale = output.current_scale().fractional_scale();
        let logical = size.to_f64().to_logical(scale).to_i32_round();

        let current = self
            .entries
            .get(&name)
            .is_some_and(|e| e.size == size && e.generation == self.generation);
        if !current && self.requested.get(&name) != Some(&(size, self.generation)) {
            self.requested.insert(name.clone(), (size, self.generation));
            let _ = self.jobs.send(Job {
                output: name.clone(),
                size,
                source: self.source.clone(),
                generation: self.generation,
            });
        }

        let entry = self.entries.get_mut(&name)?;
        if let Some(pixels) = entry.upload.take() {
            let buffer_size = (entry.size.w, entry.size.h);
            match TextureBuffer::from_memory(
                renderer,
                &pixels,
                Fourcc::Abgr8888,
                buffer_size,
                false,
                1,
                Transform::Normal,
                Some(vec![Rectangle::from_size(buffer_size.into())]),
            ) {
                Ok(texture) => entry.texture = Some(texture),
                Err(err) => warn!(?err, output = %name, "can't upload wallpaper"),
            }
        }
        let texture = entry.texture.as_ref()?;
        Some(TextureRenderElement::from_texture_buffer(
            (0.0, 0.0),
            texture,
            None,
            None,
            Some(logical),
            Kind::Unspecified,
        ))
    }

    /// A monitor is gone: free its texture.
    pub fn forget(&mut self, output: &str) {
        self.entries.remove(output);
        self.requested.remove(output);
    }
}

fn worker(jobs: mpsc::Receiver<Job>, done: channel::Sender<Done>) {
    while let Ok(first) = jobs.recv() {
        // Take everything queued, so several monitors share one decode.
        let mut batch = vec![first];
        batch.extend(jobs.try_iter());
        let mut decoded: Option<(PathBuf, Option<RgbaImage>)> = None;
        for job in batch {
            let image = match &job.source.path {
                Some(path) => {
                    if decoded.as_ref().is_none_or(|(p, _)| p != path) {
                        decoded = Some((path.clone(), load(path)));
                    }
                    decoded.as_ref().and_then(|(_, image)| image.as_ref())
                }
                None => None,
            };
            let (w, h) = (job.size.w.max(1) as u32, job.size.h.max(1) as u32);
            let pixels = match image {
                Some(image) => scale_to(image, w, h, job.source.fit),
                None => gradient(w, h, job.source.scheme),
            };
            let sent = done.send(Done {
                output: job.output,
                size: job.size,
                generation: job.generation,
                pixels: pixels.into_raw(),
            });
            if sent.is_err() {
                return;
            }
        }
    }
}

fn load(path: &PathBuf) -> Option<RgbaImage> {
    let started = std::time::Instant::now();
    let result = image::ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(image::ImageError::IoError)
        .and_then(|reader| reader.decode());
    match result {
        Ok(image) => {
            info!(path = %path.display(), elapsed = ?started.elapsed(), "loaded wallpaper");
            Some(image.into_rgba8())
        }
        Err(err) => {
            warn!(path = %path.display(), %err, "can't load wallpaper; using the default");
            None
        }
    }
}

fn scale_to(image: &RgbaImage, w: u32, h: u32, fit: WallpaperFit) -> RgbaImage {
    let (iw, ih) = image.dimensions();
    let filter = FilterType::CatmullRom;
    match fit {
        WallpaperFit::Stretch => image::imageops::resize(image, w, h, filter),
        WallpaperFit::Fill => {
            // The largest crop with the screen's aspect ratio, centered.
            let (cw, ch) = if iw as u64 * h as u64 > ih as u64 * w as u64 {
                ((ih as u64 * w as u64 / h as u64) as u32, ih)
            } else {
                (iw, (iw as u64 * h as u64 / w as u64) as u32)
            };
            let (cw, ch) = (cw.clamp(1, iw), ch.clamp(1, ih));
            let crop = image::imageops::crop_imm(image, (iw - cw) / 2, (ih - ch) / 2, cw, ch);
            image::imageops::resize(&*crop, w, h, filter)
        }
        WallpaperFit::Fit => {
            let ratio = (w as f64 / iw as f64).min(h as f64 / ih as f64);
            let (sw, sh) = (
                ((iw as f64 * ratio).round() as u32).clamp(1, w),
                ((ih as f64 * ratio).round() as u32).clamp(1, h),
            );
            let scaled = image::imageops::resize(image, sw, sh, filter);
            let mut canvas = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]));
            image::imageops::overlay(
                &mut canvas,
                &scaled,
                ((w - sw) / 2) as i64,
                ((h - sh) / 2) as i64,
            );
            canvas
        }
    }
}

/// The built-in wallpaper: a diagonal gradient with a soft glow, dithered so
/// it doesn't band on 8-bit panels.
fn gradient(w: u32, h: u32, scheme: ColorScheme) -> RgbaImage {
    let (from, to, glow) = match scheme {
        ColorScheme::Dark => ([0.11, 0.13, 0.25], [0.04, 0.04, 0.07], [0.30, 0.22, 0.45]),
        ColorScheme::Light => ([0.78, 0.84, 0.95], [0.93, 0.94, 0.97], [0.98, 0.86, 0.80]),
    };
    let (fw, fh) = (w as f32, h as f32);
    RgbaImage::from_fn(w, h, |x, y| {
        let (u, v) = (x as f32 / fw, y as f32 / fh);
        let t = ((u + v) / 2.0).clamp(0.0, 1.0);
        // Glow centered toward the lower right, fading out by ~60% of the diagonal.
        let (dx, dy) = (u - 0.75, (v - 0.8) * fh / fw);
        let g = (1.0 - (dx * dx + dy * dy).sqrt() / 0.6).clamp(0.0, 1.0);
        let g = g * g * 0.5;
        // Cheap hash noise in [-0.5, 0.5) of one 8-bit step.
        let noise = ((x.wrapping_mul(1_973) ^ y.wrapping_mul(9_277)).wrapping_mul(2_654_435_761)
            >> 24) as f32
            / 256.0
            - 0.5;
        let channel = |i: usize| {
            let base = from[i] + (to[i] - from[i]) * t;
            let c = base + (glow[i] - base) * g;
            (c * 255.0 + noise).round().clamp(0.0, 255.0) as u8
        };
        image::Rgba([channel(0), channel(1), channel(2), 255])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_crops_to_cover() {
        // A wide image on a tall screen: the sides are cropped, no bars.
        let mut image = RgbaImage::from_pixel(400, 100, image::Rgba([255, 0, 0, 255]));
        image.put_pixel(0, 50, image::Rgba([0, 255, 0, 255]));
        let out = scale_to(&image, 100, 200, WallpaperFit::Fill);
        assert_eq!(out.dimensions(), (100, 200));
        assert!(out.pixels().all(|p| p[0] > 200 && p[1] < 50));
    }

    #[test]
    fn fit_letterboxes() {
        let image = RgbaImage::from_pixel(200, 100, image::Rgba([255, 255, 255, 255]));
        let out = scale_to(&image, 100, 100, WallpaperFit::Fit);
        assert_eq!(out.get_pixel(50, 0)[0], 0, "bar at the top");
        assert_eq!(out.get_pixel(50, 50)[0], 255, "image in the middle");
    }

    #[test]
    fn gradient_is_opaque_and_sized() {
        let out = gradient(64, 32, ColorScheme::Dark);
        assert_eq!(out.dimensions(), (64, 32));
        assert!(out.pixels().all(|p| p[3] == 255));
    }
}

/// `CNDE_WALLPAPER_PREVIEW=/dir cargo test -p cnde-compositor wallpaper_preview -- --ignored`
/// writes the built-in gradients (and `CNDE_WALLPAPER_IMAGE`, filled to 1440p) as PNGs.
#[cfg(test)]
#[test]
#[ignore]
fn wallpaper_preview() {
    let Some(dir) = std::env::var_os("CNDE_WALLPAPER_PREVIEW").map(PathBuf::from) else {
        return;
    };
    gradient(1280, 720, ColorScheme::Dark)
        .save(dir.join("dark.png"))
        .unwrap();
    gradient(1280, 720, ColorScheme::Light)
        .save(dir.join("light.png"))
        .unwrap();
    if let Some(image) = std::env::var_os("CNDE_WALLPAPER_IMAGE").and_then(|p| load(&p.into())) {
        let started = std::time::Instant::now();
        let filled = scale_to(&image, 2560, 1440, WallpaperFit::Fill);
        eprintln!("scaled in {:?}", started.elapsed());
        filled.save(dir.join("image.png")).unwrap();
    }
}
