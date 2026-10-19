//! Drawing into a caller-owned pixel buffer.

use tiny_skia::{
    FillRule, FilterQuality, Mask, Paint, PathBuilder, Pattern, PixmapMut, PixmapRef, Rect,
    SpreadMode, Stroke, Transform,
};

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

    /// The whole canvas in `color` with its corners rounded by `radius`,
    /// transparent outside them: `clear` and `rounded_rect` in one pass
    /// when `color` is opaque.
    pub fn rounded_fill(&mut self, radius: f32, color: Color) {
        let (w, h) = self.size();
        if color.a < 1.0 {
            self.clear(Color::rgba(0, 0, 0, 0.0));
            self.rounded_rect(0.0, 0.0, w, h, radius, color);
            return;
        }
        self.clear(color);
        let r = radius.min(w / 2.0).min(h / 2.0);
        if r <= 0.0 {
            return;
        }
        // Corner squares on whole device pixels, so clearing them and
        // filling them again meet without a seam.
        let s = self.scale;
        let (pw, ph) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        let side = (r * s).ceil() + 1.0;
        for (x, y) in [(0.0, 0.0), (pw - side, 0.0), (pw - side, ph - side), (0.0, ph - side)] {
            if let Some(rect) = Rect::from_xywh(x, y, side, side) {
                let paint = Paint {
                    blend_mode: tiny_skia::BlendMode::Clear,
                    ..Paint::default()
                };
                self.pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
            let clip = (x / s, y / s, side / s, side / s);
            self.rounded_rect_clipped(0.0, 0.0, w, h, radius, color, clip);
        }
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

    /// [`Canvas::rounded_rect`], drawing only inside `clip` (logical x, y, w, h).
    #[allow(clippy::too_many_arguments)]
    pub fn rounded_rect_clipped(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: Color,
        clip: (f32, f32, f32, f32),
    ) {
        let s = self.scale;
        let Some(clip_rect) = Rect::from_xywh(clip.0 * s, clip.1 * s, clip.2 * s, clip.3 * s)
        else {
            return;
        };
        let (x, y, w, h) = (x * s, y * s, w * s, h * s);
        let r = (radius * s).min(w / 2.0).min(h / 2.0).max(0.0);
        let mut paint = Paint::default();
        paint.set_color(skia_color(color));
        paint.anti_alias = true;
        // Usually the clip just cuts corners off: one path, no mask.
        if let Some(clipped) = clipped_rounded_rect(x, y, w, h, r, clip_rect) {
            if let Some(path) = clipped {
                self.pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            return;
        }
        let Some(path) = rounded_rect_path(x, y, w, h, r) else {
            return;
        };
        let Some(mut mask) = Mask::new(self.pixmap.width(), self.pixmap.height()) else {
            return;
        };
        mask.fill_path(
            &PathBuilder::from_rect(clip_rect),
            FillRule::Winding,
            false,
            Transform::identity(),
        );
        // Keep only what the rounded rectangle covers inside the clip.
        self.pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            Some(&mask),
        );
    }

    /// The outline of a rounded rectangle, `width` logical px thick, drawn
    /// inside the rectangle's edge.
    #[allow(clippy::too_many_arguments)]
    pub fn stroke_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        width: f32,
        color: Color,
    ) {
        let s = self.scale;
        let inset = width / 2.0;
        let (x, y, w, h) = (
            (x + inset) * s,
            (y + inset) * s,
            (w - width) * s,
            (h - width) * s,
        );
        let r = ((radius - inset) * s).min(w / 2.0).min(h / 2.0).max(0.0);
        let Some(path) = rounded_rect_path(x, y, w, h, r) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(skia_color(color));
        paint.anti_alias = true;
        let stroke = Stroke {
            width: width * s,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    /// `image` scaled to cover the rectangle (cropping what doesn't fit),
    /// with rounded corners.
    #[allow(clippy::too_many_arguments)]
    pub fn image(&mut self, image: PixmapRef<'_>, x: f32, y: f32, w: f32, h: f32, radius: f32) {
        let s = self.scale;
        let (x, y, w, h) = (x * s, y * s, w * s, h * s);
        let (iw, ih) = (image.width() as f32, image.height() as f32);
        let zoom = (w / iw).max(h / ih);
        let (tx, ty) = (x + (w - iw * zoom) / 2.0, y + (h - ih * zoom) / 2.0);
        let r = (radius * s).min(w / 2.0).min(h / 2.0).max(0.0);
        let Some(path) = rounded_rect_path(x, y, w, h, r) else {
            return;
        };
        let paint = Paint {
            shader: Pattern::new(
                image,
                SpreadMode::Pad,
                FilterQuality::Bilinear,
                1.0,
                Transform::from_row(zoom, 0.0, 0.0, zoom, tx, ty),
            ),
            anti_alias: true,
            ..Paint::default()
        };
        self.pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Size in device pixels.
    pub fn pixel_size(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    /// Composite `layer` (same pixel size as this canvas, drawn offscreen)
    /// with `opacity`, zoomed by `zoom` about logical point (`cx`, `cy`):
    /// for fading something in as a whole.
    pub fn composite(&mut self, layer: PixmapRef<'_>, opacity: f32, zoom: f32, cx: f32, cy: f32) {
        let (cx, cy) = (cx * self.scale, cy * self.scale);
        let transform = Transform::from_row(zoom, 0.0, 0.0, zoom, cx - cx * zoom, cy - cy * zoom);
        let paint = tiny_skia::PixmapPaint {
            opacity: opacity.clamp(0.0, 1.0),
            quality: FilterQuality::Bilinear,
            ..Default::default()
        };
        self.pixmap
            .draw_pixmap(0, 0, layer, &paint, transform, None);
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

    /// A ring of `width` around (`cx`, `cy`): a full `track` circle with a
    /// `color` arc over it, from the top clockwise, covering `fraction` (0-1).
    #[allow(clippy::too_many_arguments)]
    pub fn ring(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        width: f32,
        fraction: f32,
        track: Color,
        color: Color,
    ) {
        let s = self.scale;
        let stroke = Stroke {
            width: width * s,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        let mut paint = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        let circle = PathBuilder::from_circle(cx * s, cy * s, radius * s);
        let fraction = fraction.clamp(0.0, 1.0);
        let mut arc = PathBuilder::new();
        if fraction < 0.999 && fraction > 0.0 {
            let steps = ((fraction * 96.0).ceil() as usize).max(2);
            for i in 0..=steps {
                let turn = fraction * i as f32 / steps as f32;
                let a = std::f32::consts::TAU * turn - std::f32::consts::FRAC_PI_2;
                let (x, y) = ((cx + radius * a.cos()) * s, (cy + radius * a.sin()) * s);
                if i == 0 {
                    arc.move_to(x, y);
                } else {
                    arc.line_to(x, y);
                }
            }
        }
        paint.set_color(skia_color(track));
        if let Some(path) = &circle {
            self.pixmap
                .stroke_path(path, &paint, &stroke, Transform::identity(), None);
        }
        paint.set_color(skia_color(color));
        let path = if fraction >= 0.999 { circle } else { arc.finish() };
        if fraction > 0.0
            && let Some(path) = path
        {
            self.pixmap
                .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
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
        self.slider_rounded(x, y, w, h, h / 2.0, value, track, fill);
    }

    /// Like `slider`, with corners of `radius` instead of a full pill.
    #[allow(clippy::too_many_arguments)]
    pub fn slider_rounded(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        value: f32,
        track: Color,
        fill: Color,
    ) {
        let radius = radius.min(h / 2.0);
        self.rounded_rect(x, y, w, h, radius, track);
        let s = self.scale;
        let (px, py, pw, ph) = (x * s, y * s, w * s, h * s);
        let Some(rect) = Rect::from_xywh(px, py, pw * value.clamp(0.0, 1.0), ph) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(skia_color(fill));
        paint.anti_alias = true;
        let r = (radius * s).min(pw / 2.0).min(ph / 2.0).max(0.0);
        if let Some(clipped) = clipped_rounded_rect(px, py, pw, ph, r, rect) {
            if let Some(path) = clipped {
                self.pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            return;
        }
        let Some(path) = rounded_rect_path(px, py, pw, ph, radius * s) else {
            return;
        };
        let Some(mut mask) = Mask::new(self.pixmap.width(), self.pixmap.height()) else {
            return;
        };
        mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
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
        let (w, h) = self.size();
        self.text_clipped(fonts, layout, x, y, color, (0.0, 0.0, w, h));
    }

    /// [`Canvas::text`], drawing only inside `clip` (logical x, y, w, h).
    pub fn text_clipped(
        &mut self,
        fonts: &mut Fonts,
        layout: &mut TextLayout,
        x: f32,
        y: f32,
        color: Color,
        clip: (f32, f32, f32, f32),
    ) {
        let s = self.scale;
        let origin = ((x * s).round() as i32, (y * s).round() as i32);
        let (width, height) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let x0 = ((clip.0 * s).floor() as i32).max(0);
        let y0 = ((clip.1 * s).floor() as i32).max(0);
        let x1 = (((clip.0 + clip.2) * s).ceil() as i32).min(width);
        let y1 = (((clip.1 + clip.3) * s).ceil() as i32).min(height);
        let data = self.pixmap.data_mut();
        fonts.draw(layout, color, |px, py, glyph_alpha, rgb| {
            let (px, py) = (origin.0 + px, origin.1 + py);
            if px < x0 || py < y0 || px >= x1 || py >= y1 {
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
        let (w, h) = self.size();
        self.icon_clipped(icons, name, x, y, size, color, (0.0, 0.0, w, h))
    }

    /// [`Canvas::icon`], drawing only inside `clip` (logical x, y, w, h).
    #[allow(clippy::too_many_arguments)]
    pub fn icon_clipped(
        &mut self,
        icons: &mut Icons,
        name: &str,
        x: f32,
        y: f32,
        size: f32,
        color: Color,
        clip: (f32, f32, f32, f32),
    ) -> bool {
        let s = self.scale;
        let cx0 = (clip.0 * s).floor() as i32;
        let cy0 = (clip.1 * s).floor() as i32;
        let cx1 = ((clip.0 + clip.2) * s).ceil() as i32;
        let cy1 = ((clip.1 + clip.3) * s).ceil() as i32;
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
                if coverage <= 0.0
                    || px < cx0.max(0)
                    || py < cy0.max(0)
                    || px >= width.min(cx1)
                    || py >= height.min(cy1)
                {
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
    corners_path(x, y, w, h, [r; 4])
}

/// A rectangle with its own radius at each corner: top-left, top-right,
/// bottom-right, bottom-left.
fn corners_path(x: f32, y: f32, w: f32, h: f32, r: [f32; 4]) -> Option<tiny_skia::Path> {
    if r.iter().all(|&r| r <= 0.0) {
        return Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?));
    }
    // Quarter circles approximated with cubics (k = 4/3 * (sqrt(2) - 1)).
    const K: f32 = 0.552_284_8;
    let [tl, tr, br, bl] = r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + tl, y);
    pb.line_to(x + w - tr, y);
    if tr > 0.0 {
        let k = K * tr;
        pb.cubic_to(x + w - tr + k, y, x + w, y + tr - k, x + w, y + tr);
    }
    pb.line_to(x + w, y + h - br);
    if br > 0.0 {
        let k = K * br;
        pb.cubic_to(x + w, y + h - br + k, x + w - br + k, y + h, x + w - br, y + h);
    }
    pb.line_to(x + bl, y + h);
    if bl > 0.0 {
        let k = K * bl;
        pb.cubic_to(x + bl - k, y + h, x, y + h - bl + k, x, y + h - bl);
    }
    pb.line_to(x, y + tl);
    if tl > 0.0 {
        let k = K * tl;
        pb.cubic_to(x, y + tl - k, x + tl - k, y, x + tl, y);
    }
    pb.close();
    pb.finish()
}

/// The part of a rounded rectangle (device px, corners `r`) inside `clip`,
/// as one path, when the clip doesn't cut through a corner's curve: each
/// corner is either wholly inside the clip or cut off past its curve.
/// `None` when it does (draw through a mask); `Some(None)` when nothing's
/// left.
fn clipped_rounded_rect(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    clip: Rect,
) -> Option<Option<tiny_skia::Path>> {
    let (x0, y0) = (x.max(clip.left()), y.max(clip.top()));
    let (x1, y1) = ((x + w).min(clip.right()), (y + h).min(clip.bottom()));
    if x1 <= x0 || y1 <= y0 {
        return Some(None);
    }
    // Per corner: its r x r square, as (left, top).
    let squares = [(x, y), (x + w - r, y), (x + w - r, y + h - r), (x, y + h - r)];
    let mut radii = [0.0; 4];
    for (radius, (sx, sy)) in radii.iter_mut().zip(squares) {
        let inside = sx >= x0 && sy >= y0 && sx + r <= x1 && sy + r <= y1;
        let outside = sx + r <= x0 || sy + r <= y0 || sx >= x1 || sy >= y1;
        // Cut off past the curve: the clip edge leaves the curve's band.
        let past = (sx == x && x0 >= x + r)
            || (sx != x && x1 <= sx)
            || (sy == y && y0 >= y + r)
            || (sy != y && y1 <= sy);
        if inside {
            *radius = r;
        } else if !(outside || past) && r > 0.0 {
            return None;
        }
    }
    Some(corners_path(x0, y0, x1 - x0, y1 - y0, radii))
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

    /// The one-path clip draws what the mask did.
    #[test]
    fn clipped_path_matches_mask() {
        let mask_fill = |x: f32, y: f32, w: f32, h: f32, r: f32, clip: Rect| {
            let mut pixels = vec![0u8; 60 * 60 * 4];
            let mut pixmap = PixmapMut::from_bytes(&mut pixels, 60, 60).unwrap();
            let mut mask = Mask::new(60, 60).unwrap();
            mask.fill_path(
                &PathBuilder::from_rect(clip),
                FillRule::Winding,
                false,
                Transform::identity(),
            );
            let mut paint = Paint::default();
            paint.set_color(tiny_skia::Color::WHITE);
            paint.anti_alias = true;
            let path = rounded_rect_path(x, y, w, h, r).unwrap();
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), Some(&mask));
            pixels
        };
        let path_fill = |x: f32, y: f32, w: f32, h: f32, r: f32, clip: Rect| {
            let mut pixels = vec![0u8; 60 * 60 * 4];
            let mut pixmap = PixmapMut::from_bytes(&mut pixels, 60, 60).unwrap();
            let mut paint = Paint::default();
            paint.set_color(tiny_skia::Color::WHITE);
            paint.anti_alias = true;
            let path = clipped_rounded_rect(x, y, w, h, r, clip).expect("no mask needed");
            if let Some(path) = path {
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
            pixels
        };
        let cases = [
            // The sidebar: rounded on the left, cut square on the right.
            (0.0, 0.0, 40.0, 50.0, 10.0, Rect::from_xywh(0.0, 0.0, 30.0, 50.0)),
            // The content: cut on the left.
            (10.0, 0.0, 50.0, 60.0, 10.0, Rect::from_xywh(20.0, 0.0, 40.0, 60.0)),
            // A slider fill halfway.
            (5.0, 20.0, 50.0, 12.0, 6.0, Rect::from_xywh(5.0, 20.0, 25.0, 12.0)),
            // The whole thing inside.
            (5.0, 5.0, 40.0, 40.0, 8.0, Rect::from_xywh(0.0, 0.0, 60.0, 60.0)),
        ];
        for (x, y, w, h, r, clip) in cases {
            let clip = clip.unwrap();
            let (a, b) = (mask_fill(x, y, w, h, r, clip), path_fill(x, y, w, h, r, clip));
            let worst = a.iter().zip(&b).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(worst <= 8, "{x} {y} {w} {h} {r} {clip:?}: off by {worst}");
        }
        // Cutting through a corner's curve needs the mask.
        let through = Rect::from_xywh(5.0, 20.0, 3.0, 12.0).unwrap();
        assert!(clipped_rounded_rect(5.0, 20.0, 50.0, 12.0, 6.0, through).is_none());
    }

    #[test]
    fn rounded_fill_matches_clear_and_rounded_rect() {
        let colour = Color::rgb(30, 40, 50);
        for scale in [1.0, 1.25, 2.0] {
            let (w, h) = (80u32, 50u32);
            let mut a = vec![0u8; (w * h * 4) as usize];
            let mut b = a.clone();
            let mut canvas = Canvas::new(&mut a, w, h, scale).unwrap();
            let (lw, lh) = canvas.size();
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            canvas.rounded_rect(0.0, 0.0, lw, lh, 10.0, colour);
            Canvas::new(&mut b, w, h, scale).unwrap().rounded_fill(10.0, colour);
            let worst = a.iter().zip(&b).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(worst <= 8, "{scale}x: off by {worst}");
        }
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
