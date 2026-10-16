//! Drawing into a caller-owned pixel buffer.

use tiny_skia::{FillRule, Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::{
    text::{Fonts, TextLayout},
    theme::Color,
};

/// A view onto premultiplied RGBA pixels (wl_shm ABGR8888 on little-endian),
/// with drawing in logical units scaled by `scale`.
pub struct Canvas<'a> {
    pixmap: PixmapMut<'a>,
    scale: f32,
}

impl<'a> Canvas<'a> {
    /// `None` if the buffer doesn't match `width * height * 4`.
    pub fn new(pixels: &'a mut [u8], width: u32, height: u32, scale: f32) -> Option<Self> {
        Some(Self {
            pixmap: PixmapMut::from_bytes(pixels, width, height)?,
            scale,
        })
    }

    /// Logical size.
    pub fn size(&self) -> (f32, f32) {
        (
            self.pixmap.width() as f32 / self.scale,
            self.pixmap.height() as f32 / self.scale,
        )
    }

    pub fn clear(&mut self, color: Color) {
        self.pixmap.fill(skia_color(color));
    }

    /// A rectangle with all corners rounded by `radius` (clamped to fit).
    pub fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
        let s = self.scale;
        let (x, y, w, h) = (x * s, y * s, w * s, h * s);
        let r = (radius * s).min(w / 2.0).min(h / 2.0).max(0.0);
        let Some(path) = rounded_rect_path(x, y, w, h, r) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(skia_color(color));
        paint.anti_alias = true;
        self.pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    /// Draw `layout` with its top-left at logical (`x`, `y`).
    pub fn text(
        &mut self,
        fonts: &mut Fonts,
        layout: &mut TextLayout,
        x: f32,
        y: f32,
        color: Color,
    ) {
        let s = self.scale;
        let origin = ((x * s).round() as i32, (y * s).round() as i32);
        let (width, height) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let data = self.pixmap.data_mut();
        fonts.draw(layout, color, |px, py, glyph_alpha, rgb| {
            let (px, py) = (origin.0 + px, origin.1 + py);
            if px < 0 || py < 0 || px >= width || py >= height {
                return;
            }
            let i = ((py * width + px) * 4) as usize;
            blend_premultiplied(&mut data[i..i + 4], rgb, glyph_alpha);
        });
    }
}

/// Source-over blend of a straight-alpha colour onto a premultiplied pixel.
fn blend_premultiplied(dst: &mut [u8], rgb: [u8; 3], alpha: u8) {
    let a = alpha as u32;
    let inv = 255 - a;
    for c in 0..3 {
        let src = rgb[c] as u32 * a / 255;
        dst[c] = (src + dst[c] as u32 * inv / 255) as u8;
    }
    dst[3] = (a + dst[3] as u32 * inv / 255) as u8;
}

fn skia_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba(c.r, c.g, c.b, c.a).unwrap_or(tiny_skia::Color::BLACK)
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if r <= 0.0 {
        return Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?));
    }
    // Quarter circles approximated with cubics (k = 4/3 * (sqrt(2) - 1)).
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_glyph_replaces_pixel() {
        let mut px = [10, 20, 30, 255];
        blend_premultiplied(&mut px, [200, 100, 50], 255);
        assert_eq!(px, [200, 100, 50, 255]);
    }

    #[test]
    fn transparent_glyph_keeps_pixel() {
        let mut px = [10, 20, 30, 255];
        blend_premultiplied(&mut px, [200, 100, 50], 0);
        assert_eq!(px, [10, 20, 30, 255]);
    }

    #[test]
    fn rounded_rect_fills_centre_not_corner() {
        let mut pixels = vec![0u8; 20 * 20 * 4];
        let mut canvas = Canvas::new(&mut pixels, 20, 20, 1.0).unwrap();
        canvas.rounded_rect(0.0, 0.0, 20.0, 20.0, 8.0, Color::rgb(255, 255, 255));
        let at = |x: usize, y: usize| pixels[(y * 20 + x) * 4 + 3];
        assert_eq!(at(10, 10), 255);
        assert_eq!(at(0, 0), 0);
    }
}
