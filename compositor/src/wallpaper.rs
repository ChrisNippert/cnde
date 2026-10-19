//! The desktop background (ARCHITECTURE.md §5.2): an image from the config,
//! with light and dark variants, or a built-in gradient.
//!
//! Each monitor gets its own texture at its exact pixel size, so drawing it is
//! a plain 1:1 copy. Decoding and scaling run on a worker thread; until a
//! monitor's texture is ready it keeps showing the previous one (or the clear
//! color), so nothing ever waits on a large image. The decoded image is
//! dropped as soon as it's been scaled: only the per-monitor textures stay.

use std::{collections::HashMap, path::PathBuf, sync::mpsc, thread};

use cnde_config::{ColorScheme, Config, Palette, WallpaperFit};
use image::{RgbaImage, imageops::FilterType};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem,
            element::{
                Kind,
                texture::{TextureBuffer, TextureRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::Output,
    reexports::calloop::{LoopHandle, channel},
    utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform},
};
use tracing::{info, warn};

use crate::{state::State, tint::dominant};

/// What to draw: everything that affects the pixels.
#[derive(Debug, Clone, PartialEq)]
struct Source {
    path: Option<PathBuf>,
    fit: WallpaperFit,
    scheme: ColorScheme,
    palette: Palette,
    /// `palette = "custom"`: the base and accent colours.
    custom: (String, String),
}

impl Source {
    fn from_config(config: &Config) -> Self {
        let scheme = config.appearance.color_scheme;
        let path = config.wallpaper.path(scheme);
        if path.is_some() {
            // The style only colours the built-in gradient: switching it
            // over an image mustn't decode and scale it all again.
            return Self {
                path,
                fit: config.wallpaper.fit,
                scheme: ColorScheme::default(),
                palette: Palette::default(),
                custom: Default::default(),
            };
        }
        Self {
            path,
            fit: config.wallpaper.fit,
            scheme,
            palette: config.appearance.palette,
            custom: (
                config.appearance.base_color.clone(),
                config.appearance.accent_color.clone(),
            ),
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
    /// The frosted copy, at 1/`BLUR_DOWNSCALE` size.
    blurred: Vec<u8>,
    blurred_size: Size<i32, Buffer>,
}

/// Whether the top strip (where the bar sits) of an RGBA image is light:
/// its average luminance, as text contrast sees it, is past the middle.
fn top_is_light(pixels: &[u8], size: Size<i32, Buffer>) -> bool {
    let (w, h) = (size.w.max(0) as usize, size.h.max(0) as usize);
    // The bar's height and a bit, on a typical monitor.
    let rows = (h / 16).max(1).min(h);
    let strip = pixels.get(..w * rows * 4).unwrap_or_default();
    let (sum, n) = strip.as_chunks::<4>().0.iter().fold((0.0, 0usize), |(sum, n), p| {
        let lin = |c: u8| (c as f64 / 255.0).powf(2.2);
        let y = 0.2126 * lin(p[0]) + 0.7152 * lin(p[1]) + 0.0722 * lin(p[2]);
        (sum + y, n + 1)
    });
    n > 0 && sum / n as f64 > 0.3
}

/// The spread between a pixel's strongest and weakest channel; low is grey.
fn chroma(p: &[u8; 3]) -> u8 {
    p.iter().max().unwrap_or(&0) - p.iter().min().unwrap_or(&0)
}

/// A colour to tint UI with, from a wallpaper's pixels: the dominant
/// colour, unless that is near-grey or near-black while a colourful group
/// holds at least 15% of the pixels, in which case that group's.
fn wallpaper_tint(pixels: &[[u8; 3]]) -> [u8; 3] {
    let main = dominant(pixels);
    if chroma(&main) >= 40 && main.iter().any(|&c| c >= 60) {
        return main;
    }
    let colourful: Vec<[u8; 3]> = pixels
        .iter()
        .copied()
        .filter(|p| chroma(p) >= 40 && p.iter().any(|&c| c >= 60))
        .collect();
    if colourful.len() * 100 >= pixels.len() * 15 && !colourful.is_empty() {
        dominant(&colourful)
    } else {
        main
    }
}

/// `wallpaper_tint` of an RGBA image, sampled down to at most 64 columns.
fn image_tint(pixels: &[u8], size: Size<i32, Buffer>) -> Option<[u8; 3]> {
    let (w, h) = (size.w.max(0) as usize, size.h.max(0) as usize);
    if w == 0 || h == 0 || pixels.len() < w * h * 4 {
        return None;
    }
    let step = w.div_ceil(64);
    let sample: Vec<[u8; 3]> = (0..h)
        .step_by(step)
        .flat_map(|y| (0..w).step_by(step).map(move |x| (y * w + x) * 4))
        .map(|i| [pixels[i], pixels[i + 1], pixels[i + 2]])
        .collect();
    Some(wallpaper_tint(&sample))
}

/// The blurred copy is made this many times smaller; the GPU's filtering
/// smooths it back up, and it costs 1/16 of the memory.
const BLUR_DOWNSCALE: u32 = 4;
/// Gaussian sigma at the reduced size (x4 on screen).
const BLUR_SIGMA: f32 = 16.0;

struct Entry {
    size: Size<i32, Physical>,
    generation: u64,
    texture: Option<TextureBuffer<GlesTexture>>,
    blurred: Option<TextureBuffer<GlesTexture>>,
    blurred_size: Size<i32, Buffer>,
    /// Pixels waiting to be uploaded (needs the renderer): sharp, blurred.
    upload: Option<(Vec<u8>, Vec<u8>)>,
    /// The strip along the top is light (see `top_is_light`).
    top_light: bool,
    /// A colour to tint UI with (see `wallpaper_tint`).
    tint: Option<[u8; 3]>,
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
            blurred: None,
            blurred_size: done.blurred_size,
            upload: None,
            top_light: false,
            tint: None,
        });
        // A newer result may already be here (jobs can finish out of order
        // across a fast config change); keep the newest.
        if done.generation >= entry.generation || entry.texture.is_none() {
            entry.size = done.size;
            entry.generation = done.generation;
            entry.blurred_size = done.blurred_size;
            entry.top_light = top_is_light(&done.blurred, done.blurred_size);
            entry.tint = image_tint(&done.blurred, done.blurred_size);
            entry.upload = Some((done.pixels, done.blurred));
        }
    }

    /// Whether the wallpaper along `output`'s top is light, so text drawn
    /// straight on it (a clear bar) should be dark.
    pub fn top_light(&self, output: &str) -> bool {
        self.entries.get(output).is_some_and(|e| e.top_light)
    }

    /// The wallpaper's tint colour on `output`, once it has loaded.
    #[allow(dead_code)]
    pub fn tint(&self, output: &str) -> Option<[u8; 3]> {
        self.entries.get(output).and_then(|e| e.tint)
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
        if let Some((pixels, blurred)) = entry.upload.take() {
            let size = Size::<i32, Buffer>::from((entry.size.w, entry.size.h));
            entry.texture = upload(renderer, &pixels, size, &name);
            entry.blurred = upload(renderer, &blurred, entry.blurred_size, &name);
        }
        let texture = entry.texture.as_ref()?;
        Some(full_screen(
            texture,
            entry.size.w,
            entry.size.h,
            logical,
            None,
        ))
    }

    /// The frosted (blurred) wallpaper for `output`, faded to `alpha`.
    /// Call after [`element`](Self::element), which does the loading.
    pub fn blurred_element(
        &self,
        output: &Output,
        alpha: f32,
    ) -> Option<TextureRenderElement<GlesTexture>> {
        let entry = self.entries.get(&output.name())?;
        let texture = entry.blurred.as_ref()?;
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);
        let scale = output.current_scale().fractional_scale();
        let logical = size.to_f64().to_logical(scale).to_i32_round();
        let blurred = entry.blurred_size;
        Some(full_screen(
            texture,
            blurred.w,
            blurred.h,
            logical,
            Some(alpha),
        ))
    }

    /// The part of the frosted wallpaper under `area` (logical,
    /// output-local), and where that part sits in the texture (x0, y0, x1,
    /// y1 in texture coordinates).
    pub fn blurred_crop(
        &self,
        output: &Output,
        area: Rectangle<i32, Logical>,
        alpha: f32,
    ) -> Option<(TextureRenderElement<GlesTexture>, [f32; 4])> {
        let entry = self.entries.get(&output.name())?;
        let texture = entry.blurred.as_ref()?;
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);
        let scale = output.current_scale().fractional_scale();
        let logical = size.to_f64().to_logical(scale);
        let blurred = entry.blurred_size;
        let (fx, fy) = (
            blurred.w as f64 / logical.w.max(1.0),
            blurred.h as f64 / logical.h.max(1.0),
        );
        let area = area.to_f64();
        let src = Rectangle::new(
            Point::from((area.loc.x * fx, area.loc.y * fy)),
            Size::from((area.size.w * fx, area.size.h * fy)),
        );
        let crop = [
            (src.loc.x / blurred.w as f64) as f32,
            (src.loc.y / blurred.h as f64) as f32,
            ((src.loc.x + src.size.w) / blurred.w as f64) as f32,
            ((src.loc.y + src.size.h) / blurred.h as f64) as f32,
        ];
        let element = TextureRenderElement::from_texture_buffer(
            area.loc.to_physical(scale),
            texture,
            Some(alpha),
            Some(src),
            Some(area.size.to_i32_round()),
            Kind::Unspecified,
        );
        Some((element, crop))
    }

    /// Whether `output` shows the wallpaper the config asks for.
    pub fn ready(&self, output: &Output) -> bool {
        self.entries.get(&output.name()).is_some_and(|e| {
            e.generation == self.generation && e.upload.is_none() && e.texture.is_some()
        })
    }

    /// A monitor is gone: free its texture.
    pub fn forget(&mut self, output: &str) {
        self.entries.remove(output);
        self.requested.remove(output);
    }
}

fn upload(
    renderer: &mut GlesRenderer,
    pixels: &[u8],
    size: Size<i32, Buffer>,
    output: &str,
) -> Option<TextureBuffer<GlesTexture>> {
    match renderer.import_memory(pixels, Fourcc::Abgr8888, size, false) {
        Ok(texture) => Some(TextureBuffer::from_texture(
            renderer,
            texture,
            1,
            Transform::Normal,
            Some(vec![Rectangle::from_size(size)]),
        )),
        Err(err) => {
            warn!(?err, output, "can't upload wallpaper");
            None
        }
    }
}

/// `texture` (`w` x `h` pixels) stretched over the whole output.
pub(crate) fn full_screen(
    texture: &TextureBuffer<GlesTexture>,
    w: i32,
    h: i32,
    logical: Size<i32, Logical>,
    alpha: Option<f32>,
) -> TextureRenderElement<GlesTexture> {
    // Without an explicit source, a size override *crops* the texture to
    // that many logical pixels instead of scaling it.
    let src = Rectangle::from_size(Size::<f64, Logical>::from((w as f64, h as f64)));
    TextureRenderElement::from_texture_buffer(
        (0.0, 0.0),
        texture,
        alpha,
        Some(src),
        Some(logical),
        Kind::Unspecified,
    )
}

fn worker(jobs: mpsc::Receiver<Job>, done: channel::Sender<Done>) {
    while let Ok(first) = jobs.recv() {
        // Take everything queued, so several monitors share one decode.
        let mut batch = vec![first];
        batch.extend(jobs.try_iter());
        let mut decoded: Vec<(PathBuf, Option<RgbaImage>)> = Vec::new();
        for path in batch.iter().filter_map(|job| job.source.path.as_ref()) {
            if !decoded.iter().any(|(p, _)| p == path) {
                decoded.push((path.clone(), load(path)));
            }
        }
        // Each monitor's scaling and blurring at once, on its own thread.
        let sent = thread::scope(|scope| {
            let handles: Vec<_> = batch
                .into_iter()
                .map(|job| {
                    let image = job.source.path.as_ref().and_then(|path| {
                        decoded
                            .iter()
                            .find(|(p, _)| p == path)
                            .and_then(|(_, image)| image.as_ref())
                    });
                    let done = done.clone();
                    scope.spawn(move || {
                        let (w, h) = (job.size.w.max(1) as u32, job.size.h.max(1) as u32);
                        let pixels = match image {
                            Some(image) => scale_to(image, w, h, job.source.fit),
                            None => gradient(w, h, &job.source),
                        };
                        let blurred = frost(&pixels);
                        let blurred_size =
                            Size::from((blurred.width() as i32, blurred.height() as i32));
                        done.send(Done {
                            output: job.output,
                            size: job.size,
                            generation: job.generation,
                            pixels: pixels.into_raw(),
                            blurred: blurred.into_raw(),
                            blurred_size,
                        })
                        .is_ok()
                    })
                })
                .collect();
            handles.into_iter().all(|h| h.join().unwrap_or(false))
        });
        if !sent {
            return;
        }
    }
}

/// The frosted-glass copy: shrunk, then blurred hard.
fn frost(image: &RgbaImage) -> RgbaImage {
    let (w, h) = image.dimensions();
    let small = image::imageops::resize(
        image,
        (w / BLUR_DOWNSCALE).max(1),
        (h / BLUR_DOWNSCALE).max(1),
        FilterType::Triangle,
    );
    // fast_blur is a stack of box blurs; a second, gentler pass removes the
    // faint blockiness one pass leaves.
    let once = image::imageops::fast_blur(&small, BLUR_SIGMA);
    image::imageops::fast_blur(&once, BLUR_SIGMA / 2.0)
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

/// The built-in wallpaper's colours for a custom palette: the base's hue,
/// dark or light, with a glow of the accent.
fn custom_gradient(
    base: &str,
    accent: &str,
    scheme: ColorScheme,
) -> ([f32; 3], [f32; 3], [f32; 3]) {
    use cnde_ui::Color;
    let color = |text: &str, fallback: Color| {
        cnde_config::parse_color(text).map_or(fallback, |(r, g, b, _)| Color::rgb(r, g, b))
    };
    let (h, s, _) = color(base, Color::rgb(0x2b, 0x1a, 0x40)).hsl();
    let (ah, as_, _) = color(accent, Color::rgb(0xf4, 0xa7, 0xc6)).hsl();
    let s = (s * 0.6).min(0.5);
    let rgb = |c: Color| [c.r, c.g, c.b];
    match scheme {
        ColorScheme::Dark => (
            rgb(Color::from_hsl(h, s, 0.17)),
            rgb(Color::from_hsl(h, s, 0.05)),
            rgb(Color::from_hsl(ah, as_ * 0.6, 0.38)),
        ),
        ColorScheme::Light => (
            rgb(Color::from_hsl(h, s, 0.9)),
            rgb(Color::from_hsl(h, s * 0.6, 0.95)),
            rgb(Color::from_hsl(ah, as_ * 0.7, 0.86)),
        ),
    }
}

/// The built-in wallpaper: a diagonal gradient with a soft glow, dithered so
/// it doesn't band on 8-bit panels.
fn gradient(w: u32, h: u32, source: &Source) -> RgbaImage {
    let (from, to, glow) = match (source.palette, source.scheme) {
        (Palette::Custom, scheme) => custom_gradient(&source.custom.0, &source.custom.1, scheme),
        // Quiet: dark sage into near-black, an amber glow.
        (Palette::Quiet, ColorScheme::Dark) => {
            ([0.18, 0.24, 0.20], [0.055, 0.07, 0.06], [0.42, 0.31, 0.17])
        }
        // Light sage, a little deeper at the far end, a warm glow.
        (Palette::Quiet, ColorScheme::Light) => {
            ([0.84, 0.88, 0.82], [0.66, 0.73, 0.65], [0.95, 0.84, 0.64])
        }
        // Dusk: deep plum fading to near-black, a pink glow.
        (Palette::Cnde, ColorScheme::Dark) => {
            ([0.17, 0.10, 0.25], [0.05, 0.035, 0.08], [0.46, 0.22, 0.40])
        }
        // Blossom: blush into lavender, a warm pink glow.
        (Palette::Cnde, ColorScheme::Light) => {
            ([0.96, 0.85, 0.91], [0.91, 0.89, 0.97], [1.0, 0.78, 0.86])
        }
        (Palette::Adwaita, ColorScheme::Dark) => {
            ([0.11, 0.13, 0.25], [0.04, 0.04, 0.07], [0.30, 0.22, 0.45])
        }
        (Palette::Adwaita, ColorScheme::Light) => {
            ([0.78, 0.84, 0.95], [0.93, 0.94, 0.97], [0.98, 0.86, 0.80])
        }
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

    #[test]
    fn top_strip_brightness() {
        let size = Size::from((4, 32));
        let image = |top: u8, rest: u8| -> Vec<u8> {
            (0..32)
                .flat_map(|row| {
                    let v = if row < 2 { top } else { rest };
                    std::iter::repeat_n([v, v, v, 255], 4).flatten()
                })
                .collect()
        };
        // Only the top strip counts.
        assert!(top_is_light(&image(240, 10), size));
        assert!(!top_is_light(&image(30, 250), size));
        assert!(!top_is_light(&[], size));
    }

    use super::*;

    #[test]
    fn tint_prefers_a_colourful_group_over_black() {
        let mut px = vec![[5, 5, 8]; 70];
        px.extend([[20, 60, 200]; 30]);
        assert_eq!(wallpaper_tint(&px), [20, 60, 200]);
        // Too small a share of colour: stay with the black.
        let mut px = vec![[5, 5, 8]; 95];
        px.extend([[20, 60, 200]; 5]);
        assert_eq!(wallpaper_tint(&px), [5, 5, 8]);
    }

    #[test]
    fn tint_of_grey_and_mixes() {
        assert_eq!(wallpaper_tint(&[[128, 128, 128]; 20]), [128, 128, 128]);
        let mut px = vec![[200, 120, 40]; 50];
        px.extend([[20, 90, 160]; 30]);
        px.extend([[30, 30, 30]; 20]);
        assert_eq!(wallpaper_tint(&px), [200, 120, 40]);
        assert_eq!(wallpaper_tint(&[]), [0, 0, 0]);
    }

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
        let out = gradient(64, 32, &test_source(ColorScheme::Dark));
        assert_eq!(out.dimensions(), (64, 32));
        assert!(out.pixels().all(|p| p[3] == 255));
    }
}

/// The built-in wallpaper's source in the default palette.
#[cfg(test)]
fn test_source(scheme: ColorScheme) -> Source {
    Source {
        path: None,
        fit: WallpaperFit::Fill,
        scheme,
        palette: Palette::Cnde,
        custom: Default::default(),
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
    gradient(1280, 720, &test_source(ColorScheme::Dark))
        .save(dir.join("dark.png"))
        .unwrap();
    gradient(1280, 720, &test_source(ColorScheme::Light))
        .save(dir.join("light.png"))
        .unwrap();
    if let Some(image) = std::env::var_os("CNDE_WALLPAPER_IMAGE").and_then(|p| load(&p.into())) {
        let started = std::time::Instant::now();
        let filled = scale_to(&image, 2560, 1440, WallpaperFit::Fill);
        eprintln!("scaled in {:?}", started.elapsed());
        filled.save(dir.join("image.png")).unwrap();
        frost(&filled).save(dir.join("frosted.png")).unwrap();
    }
}
