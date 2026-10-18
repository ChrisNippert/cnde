//! Text via cosmic-text: shaping, layout, glyph rasterizing.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};

use crate::theme::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Normal,
    Bold,
}

pub struct Fonts {
    system: FontSystem,
    cache: SwashCache,
    family: String,
}

/// Shaped text ready to draw, with its logical size.
pub struct TextLayout {
    buffer: Buffer,
    pub width: f32,
    pub height: f32,
    /// Device pixels per logical pixel it was shaped for.
    scale: f32,
}

impl Fonts {
    /// Loads font metadata from the standard directories and picks the first
    /// installed family from `families`. Glyphs are only loaded when drawn.
    pub fn new(families: &[&str]) -> Self {
        let system = FontSystem::new();
        let installed = |name: &str| {
            system
                .db()
                .faces()
                .any(|face| face.families.iter().any(|(f, _)| f == name))
        };
        let family = families
            .iter()
            .find(|name| installed(name))
            .map_or_else(|| "sans-serif".to_string(), |name| name.to_string());
        Self {
            system,
            cache: SwashCache::new(),
            family,
        }
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    /// Shape `text` on one line at `size` logical px for a `scale`x buffer.
    pub fn layout(&mut self, text: &str, size: f32, weight: Weight, scale: f32) -> TextLayout {
        let px = size * scale;
        let mut buffer = Buffer::new(&mut self.system, Metrics::new(px, (px * 1.25).ceil()));
        buffer.set_size(None, None);
        let attrs = Attrs::new()
            .family(Family::Name(&self.family))
            .weight(match weight {
                Weight::Normal => cosmic_text::Weight::NORMAL,
                Weight::Bold => cosmic_text::Weight::BOLD,
            });
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.system, false);
        let (width, height) = buffer.layout_runs().fold((0.0f32, 0.0f32), |(w, h), run| {
            (w.max(run.line_w), h + run.line_height)
        });
        TextLayout {
            buffer,
            width: width / scale,
            height: height / scale,
            scale,
        }
    }

    /// Calls `pixel(x, y, alpha, rgb)` for every covered device pixel,
    /// relative to the layout's top-left.
    pub(crate) fn draw(
        &mut self,
        layout: &mut TextLayout,
        color: Color,
        mut pixel: impl FnMut(i32, i32, u8, [u8; 3]),
    ) {
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let text_color = cosmic_text::Color::rgba(
            to_u8(color.r),
            to_u8(color.g),
            to_u8(color.b),
            to_u8(color.a),
        );
        layout.buffer.draw(
            &mut self.system,
            &mut self.cache,
            text_color,
            |x, y, w, h, c| {
                // cosmic-text drops the colour's own alpha (its glyph alpha
                // is the coverage alone), so dim text would draw solid.
                let a = (c.a() as u32 * text_color.a() as u32 / 255) as u8;
                for dy in 0..h as i32 {
                    for dx in 0..w as i32 {
                        pixel(x + dx, y + dy, a, [c.r(), c.g(), c.b()]);
                    }
                }
            },
        );
    }
}

impl TextLayout {
    pub fn scale(&self) -> f32 {
        self.scale
    }
}
