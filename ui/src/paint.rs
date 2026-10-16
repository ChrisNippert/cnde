//! Drawing into a caller-owned pixel buffer.

use tiny_skia::{FillRule, Mask, Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::{
    icons::Icons,
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

    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        self.rounded_rect(
            cx - radius,
            cy - radius,
            radius * 2.0,
            radius * 2.0,
            radius,
            color,
        );
    }

    /// An iOS-style fat slider: a fully rounded track, filled from the left
    /// to `value` (0-1). The fill is clipped to the track, so it keeps the
    /// rounded left end and has a straight right edge.
    #[allow(clippy::too_many_arguments)]
    pub fn slider(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        value: f32,
        track: Color,
        fill: Color,
    ) {
        self.rounded_rect(x, y, w, h, h / 2.0, track);
        let s = self.scale;
        let (px, py, pw, ph) = (x * s, y * s, w * s, h * s);
        let Some(path) = rounded_rect_path(px, py, pw, ph, ph / 2.0) else {
            return;
        };
        let Some(mut mask) = Mask::new(self.pixmap.width(), self.pixmap.height()) else {
            return;
        };
        mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        let Some(rect) = Rect::from_xywh(px, py, pw * value.clamp(0.0, 1.0), ph) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(skia_color(fill));
        paint.anti_alias = true;
        self.pixmap
            .fill_rect(rect, &paint, Transform::identity(), Some(&mask));
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

impl Canvas<'_> {
    /// A symbolic icon tinted `color`, `size` logical px square, top-left at
    /// (`x`, `y`). Returns false if the theme doesn't have it.
    pub fn icon(
        &mut self,
        icons: &mut Icons,
        name: &str,
        x: f32,
        y: f32,
        size: f32,
        color: Color,
    ) -> bool {
        let s = self.scale;
        let px = (size * s).round().max(1.0) as u32;
        let Some(mask) = icons.get(name, px) else {
            return false;
        };
        let (ox, oy) = ((x * s).round() as i32, (y * s).round() as i32);
        let (width, height) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let rgb = [
            (color.r * 255.0).round() as u8,
            (color.g * 255.0).round() as u8,
            (color.b * 255.0).round() as u8,
        ];
        let data = self.pixmap.data_mut();
        for my in 0..mask.size as i32 {
            for mx in 0..mask.size as i32 {
                let coverage = mask.alpha[(my * mask.size as i32 + mx) as usize] as f32 * color.a;
                let (px, py) = (ox + mx, oy + my);
                if coverage <= 0.0 || px < 0 || py < 0 || px >= width || py >= height {
                    continue;
                }
                let i = ((py * width + px) * 4) as usize;
                blend_premultiplied(&mut data[i..i + 4], rgb, coverage.round() as u8);
            }
        }
        true
    }
}

impl Canvas<'_> {
    /// A full-colour app icon (see [`Icons::app`]), `size` logical px square,
    /// top-left at (`x`, `y`). Returns false if no icon was found.
    pub fn app_icon(&mut self, icons: &mut Icons, name: &str, x: f32, y: f32, size: f32) -> bool {
        let s = self.scale;
        let px = (size * s).round().max(1.0) as u32;
        let Some(icon) = icons.app(name, px) else {
            return false;
        };
        self.pixmap.draw_pixmap(
            (x * s).round() as i32,
            (y * s).round() as i32,
            icon.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        true
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
    fn slider_fill_is_clipped_to_the_track() {
        let mut pixels = vec![0u8; 100 * 20 * 4];
        let mut canvas = Canvas::new(&mut pixels, 100, 20, 1.0).unwrap();
        let track = Color::rgba(255, 255, 255, 0.2);
        canvas.slider(0.0, 0.0, 100.0, 20.0, 0.5, track, Color::rgb(255, 255, 255));
        let alpha = |x: usize, y: usize| pixels[(y * 100 + x) * 4 + 3];
        assert_eq!(alpha(25, 10), 255, "filled half is opaque");
        assert!(
            alpha(75, 10) > 40 && alpha(75, 10) < 60,
            "rest is the track"
        );
        assert_eq!(alpha(0, 0), 0, "rounded corner stays empty");
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
