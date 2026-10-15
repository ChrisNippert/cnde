// Adapted from smithay: anvil/src/cursor.rs (also the fallback cursor image)

//! Cursor drawing for backends where we own the screen (udev). Nested, the
//! host draws the cursor instead.

use std::{collections::HashMap, time::Duration};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    input::pointer::CursorIcon,
    utils::{Logical, Point, Transform},
};
use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

static FALLBACK_CURSOR: &[u8] = include_bytes!("../resources/fallback-cursor.rgba");

/// GNOME's cursor theme, used when `XCURSOR_THEME` isn't set.
pub const DEFAULT_THEME: &str = "Adwaita";
pub const DEFAULT_SIZE: u32 = 24;

struct LoadedFrame {
    image: Image,
    buffer: MemoryRenderBuffer,
}

pub struct Cursors {
    theme: CursorTheme,
    size: u32,
    /// Frames per icon name; `None` caches "this theme doesn't have it".
    icons: HashMap<&'static str, Option<Vec<LoadedFrame>>>,
    fallback: LoadedFrame,
}

impl Cursors {
    pub fn load() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| DEFAULT_THEME.into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_SIZE);
        let fallback_image = Image {
            size: 32,
            width: 64,
            height: 64,
            xhot: 1,
            yhot: 1,
            delay: 1,
            pixels_rgba: FALLBACK_CURSOR.to_vec(),
            pixels_argb: vec![],
        };
        tracing::info!(theme = %theme_name, size, "cursor theme");
        Self {
            theme: CursorTheme::load(&theme_name),
            size,
            icons: HashMap::new(),
            fallback: LoadedFrame {
                buffer: buffer_for(&fallback_image),
                image: fallback_image,
            },
        }
    }

    /// The buffer and hotspot for `icon` at animation time `time`, falling
    /// back to the theme's default arrow and then to a built-in one.
    pub fn get(
        &mut self,
        icon: CursorIcon,
        time: Duration,
    ) -> (MemoryRenderBuffer, Point<i32, Logical>) {
        let names = [icon.name(), CursorIcon::Default.name()];
        for name in names {
            if !self.icons.contains_key(name) {
                let frames = load_icon(&self.theme, name, self.size);
                self.icons.insert(name, frames);
            }
        }
        let frame = names
            .into_iter()
            .find_map(|name| self.icons.get(name)?.as_deref())
            .map_or(&self.fallback, |frames| pick_frame(frames, time));
        let hotspot = Point::from((frame.image.xhot as i32, frame.image.yhot as i32));
        (frame.buffer.clone(), hotspot)
    }
}

fn buffer_for(image: &Image) -> MemoryRenderBuffer {
    MemoryRenderBuffer::from_slice(
        &image.pixels_rgba,
        Fourcc::Argb8888,
        (image.width as i32, image.height as i32),
        1,
        Transform::Normal,
        None,
    )
}

fn load_icon(theme: &CursorTheme, name: &str, size: u32) -> Option<Vec<LoadedFrame>> {
    let path = theme.load_icon(name)?;
    let data = std::fs::read(&path)
        .map_err(|err| warn!(?path, ?err, "failed to read cursor"))
        .ok()?;
    let images = parse_xcursor(&data)?;

    // Keep only the frames of the size closest to the one we want.
    let nearest = images
        .iter()
        .min_by_key(|image| (size as i32 - image.size as i32).abs())?;
    let (width, height) = (nearest.width, nearest.height);
    let frames: Vec<LoadedFrame> = images
        .into_iter()
        .filter(|image| image.width == width && image.height == height)
        .map(|image| LoadedFrame {
            buffer: buffer_for(&image),
            image,
        })
        .collect();
    (!frames.is_empty()).then_some(frames)
}

/// Animated cursors (e.g. "wait") cycle through frames by their delays.
fn pick_frame(frames: &[LoadedFrame], time: Duration) -> &LoadedFrame {
    let total: u32 = frames.iter().map(|f| f.image.delay).sum();
    if total == 0 {
        return &frames[0];
    }
    let mut millis = (time.as_millis() % total as u128) as u32;
    for frame in frames {
        if millis < frame.image.delay {
            return frame;
        }
        millis -= frame.image.delay;
    }
    &frames[0]
}
