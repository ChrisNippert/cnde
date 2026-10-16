//! Controls for app windows (the settings app), drawn the libadwaita way.
//!
//! Immediate mode: the app paints its whole window each time something
//! changes, and each call here draws one control and returns where it is,
//! so the app can keep those rects for hit-testing the pointer.

use crate::{
    icons::Icons,
    paint::Canvas,
    text::{Fonts, Weight},
    theme::{Color, Theme},
};

/// A logical-pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, (px, py): (f64, f64)) -> bool {
        let (px, py) = (px as f32, py as f32);
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Everything drawing needs, borrowed for one paint.
pub struct Painter<'a, 'c> {
    pub canvas: &'a mut Canvas<'c>,
    pub fonts: &'a mut Fonts,
    pub icons: &'a mut Icons,
    pub theme: &'a Theme,
}

impl Painter<'_, '_> {
    pub fn measure(&mut self, text: &str, size: f32, weight: Weight) -> f32 {
        self.fonts
            .layout(text, size, weight, self.canvas.scale())
            .width
    }

    /// Text with its top-left at (`x`, `y`); returns its width.
    pub fn label(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        weight: Weight,
        color: Color,
    ) -> f32 {
        let mut layout = self.fonts.layout(text, size, weight, self.canvas.scale());
        self.canvas.text(self.fonts, &mut layout, x, y, color);
        layout.width
    }

    /// Text vertically centred on `cy`, cut off at `max_width` with "…".
    #[allow(clippy::too_many_arguments)]
    pub fn label_v(
        &mut self,
        text: &str,
        x: f32,
        cy: f32,
        max_width: f32,
        size: f32,
        weight: Weight,
        color: Color,
    ) -> f32 {
        let scale = self.canvas.scale();
        let mut layout = self.fonts.layout(text, size, weight, scale);
        if layout.width > max_width && max_width > 0.0 {
            let mut chars: Vec<char> = text.chars().collect();
            while !chars.is_empty() && layout.width > max_width {
                chars.pop();
                let cut: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
                layout = self.fonts.layout(&cut, size, weight, scale);
            }
        }
        let y = cy - layout.height / 2.0;
        self.canvas.text(self.fonts, &mut layout, x, y, color);
        layout.width
    }

    /// A symbolic icon centred on (`cx`, `cy`).
    pub fn icon(&mut self, name: &str, cx: f32, cy: f32, size: f32, color: Color) {
        self.canvas.icon(
            self.icons,
            name,
            cx - size / 2.0,
            cy - size / 2.0,
            size,
            color,
        );
    }

    /// An on/off switch; (`x`, `y`) is its top-left. `on` runs 0-1 so the
    /// knob can slide (see [`crate::anim`]).
    pub fn switch(&mut self, x: f32, y: f32, on: f32) -> Rect {
        let t = self.theme;
        let (w, h) = (t.switch_width, t.switch_height);
        let track = t.switch_off.mix(t.accent, on);
        self.canvas.rounded_rect(x, y, w, h, h / 2.0, track);
        let knob = h - 6.0;
        let kx = x + 3.0 + (w - 6.0 - knob) * on;
        self.canvas
            .circle(kx + knob / 2.0, y + h / 2.0, knob / 2.0, t.switch_knob);
        Rect::new(x, y, w, h)
    }

    /// A flat rounded button background; `hover` (0-1) brightens it.
    pub fn button(&mut self, rect: Rect, hover: f32) {
        let t = self.theme;
        let color = t.button_background.mix(t.button_hover, hover);
        self.canvas
            .rounded_rect(rect.x, rect.y, rect.w, rect.h, t.button_radius, color);
    }

    /// The width [`Painter::segmented`] will take.
    pub fn segmented_width(&mut self, labels: &[&str]) -> f32 {
        let size = self.theme.font_size;
        labels
            .iter()
            .map(|l| self.measure(l, size, Weight::Normal) + 24.0)
            .sum()
    }

    /// Linked toggle buttons, one per label; (`x`, `y`) is the top-left.
    /// `selected` is the lit button's index, fractional while the
    /// highlight slides between two; `hover(i)` is each button's hover
    /// (0-1). Returns each button's rect.
    pub fn segmented(
        &mut self,
        x: f32,
        y: f32,
        labels: &[&str],
        selected: Option<f32>,
        hover: impl Fn(usize) -> f32,
    ) -> Vec<Rect> {
        let t = self.theme;
        let h = t.button_height;
        let size = t.font_size;
        let width = self.segmented_width(labels);
        self.canvas
            .rounded_rect(x, y, width, h, t.button_radius, t.button_background);
        let mut rects = Vec::with_capacity(labels.len());
        let mut bx = x;
        for label in labels {
            let w = self.measure(label, size, Weight::Normal) + 24.0;
            rects.push(Rect::new(bx, y, w, h));
            bx += w;
        }
        for (i, r) in rects.iter().enumerate() {
            let amount = hover(i);
            if amount > 0.0 {
                let color = t.hover.with_alpha(t.hover.a * amount);
                self.canvas.rounded_rect(
                    r.x + 2.0,
                    r.y + 2.0,
                    r.w - 4.0,
                    h - 4.0,
                    t.button_radius - 2.0,
                    color,
                );
            }
        }
        // The highlight, between the two buttons it's sliding between.
        if let Some(pos) = selected.filter(|_| !rects.is_empty()) {
            let pos = pos.clamp(0.0, (rects.len() - 1) as f32);
            let (i, f) = (pos.floor() as usize, pos.fract());
            let a = rects[i];
            let b = rects[(i + 1).min(rects.len() - 1)];
            let hx = a.x + (b.x - a.x) * f;
            let hw = a.w + (b.w - a.w) * f;
            self.canvas.rounded_rect(
                hx + 2.0,
                y + 2.0,
                hw - 4.0,
                h - 4.0,
                t.button_radius - 2.0,
                t.accent,
            );
        }
        for (i, (label, r)) in labels.iter().zip(&rects).enumerate() {
            // Text turns to the on-accent colour as the highlight arrives.
            let lit = selected.map_or(0.0, |pos| (1.0 - (pos - i as f32).abs()).max(0.0));
            let color = t.window_text.mix(t.on_accent, lit);
            self.label_v(
                label,
                r.x + 12.0,
                y + h / 2.0,
                r.w,
                size,
                Weight::Normal,
                color,
            );
        }
        rects
    }

    /// A button showing the current choice and a down arrow; `rect` is the
    /// whole button.
    pub fn dropdown(&mut self, rect: Rect, label: &str, hover: f32) {
        let t = self.theme;
        self.button(rect, hover);
        let cy = rect.y + rect.h / 2.0;
        self.label_v(
            label,
            rect.x + 12.0,
            cy,
            rect.w - 40.0,
            t.font_size,
            Weight::Normal,
            t.window_text,
        );
        self.icon(
            "pan-down-symbolic",
            rect.right() - 18.0,
            cy,
            16.0,
            t.window_text,
        );
    }

    /// The width a dropdown needs for its widest choice.
    pub fn dropdown_width(&mut self, labels: &[&str]) -> f32 {
        let size = self.theme.font_size;
        labels
            .iter()
            .map(|l| self.measure(l, size, Weight::Normal))
            .fold(0.0, f32::max)
            + 44.0
    }

    /// The height [`Painter::popover`] takes for `count` choices.
    pub fn popover_height(count: usize) -> f32 {
        36.0 * count as f32 + 12.0
    }

    /// A list of choices floating over the window. `appear` (0-1) fades it
    /// in and slides it into place; `hover(i)` is each row's hover (0-1).
    /// Returns each row's rect.
    #[allow(clippy::too_many_arguments)]
    pub fn popover(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        labels: &[&str],
        selected: Option<usize>,
        hover: impl Fn(usize) -> f32,
        appear: f32,
    ) -> Vec<Rect> {
        let t = self.theme;
        let row = 36.0;
        let pad = 6.0;
        let h = Self::popover_height(labels.len());
        let y = y - 6.0 * (1.0 - appear);
        let fade = |c: Color| c.with_alpha(c.a * appear);
        // A soft shadow: a few widening, fading outlines.
        for i in 1..=4 {
            let grow = i as f32 * 2.0;
            self.canvas.rounded_rect(
                x - grow,
                y - grow + 2.0,
                width + grow * 2.0,
                h + grow * 2.0,
                t.popover_radius + grow,
                Color::rgba(0, 0, 0, 0.05 * appear),
            );
        }
        self.canvas
            .rounded_rect(x, y, width, h, t.popover_radius, fade(t.popover_background));
        self.canvas.stroke_rounded_rect(
            x,
            y,
            width,
            h,
            t.popover_radius,
            1.0,
            fade(t.card_separator),
        );
        let mut rects = Vec::with_capacity(labels.len());
        for (i, label) in labels.iter().enumerate() {
            let rect = Rect::new(x + pad, y + pad + row * i as f32, width - pad * 2.0, row);
            let amount = hover(i) * appear;
            if amount > 0.0 {
                self.canvas.rounded_rect(
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                    6.0,
                    t.hover.with_alpha(t.hover.a * amount),
                );
            }
            let cy = rect.y + row / 2.0;
            self.label_v(
                label,
                rect.x + 10.0,
                cy,
                rect.w - 40.0,
                t.font_size,
                Weight::Normal,
                fade(t.window_text),
            );
            if selected == Some(i) {
                self.icon(
                    "object-select-symbolic",
                    rect.right() - 16.0,
                    cy,
                    16.0,
                    fade(t.window_text),
                );
            }
            rects.push(rect);
        }
        rects
    }

    /// The GTK-style slider: a thin track filled to `value` (0-1) and a
    /// round knob. `rect` is the hit area; the track runs along its middle.
    pub fn scale(&mut self, rect: Rect, value: f32) {
        let t = self.theme;
        let value = value.clamp(0.0, 1.0);
        let knob = 20.0;
        let (x0, x1) = (rect.x + knob / 2.0, rect.right() - knob / 2.0);
        let cy = rect.y + rect.h / 2.0;
        let track = 6.0;
        self.canvas.rounded_rect(
            x0,
            cy - track / 2.0,
            x1 - x0,
            track,
            track / 2.0,
            t.switch_off,
        );
        let kx = x0 + (x1 - x0) * value;
        self.canvas
            .rounded_rect(x0, cy - track / 2.0, kx - x0, track, track / 2.0, t.accent);
        self.canvas
            .circle(kx, cy + 1.0, knob / 2.0 + 0.5, Color::rgba(0, 0, 0, 0.3));
        self.canvas.circle(kx, cy, knob / 2.0, t.switch_knob);
    }

    /// Where along a [`Painter::scale`] in `rect` the pointer at `x` is (0-1).
    pub fn scale_value(rect: Rect, x: f64) -> f32 {
        let knob = 20.0;
        let (x0, x1) = (rect.x + knob / 2.0, rect.right() - knob / 2.0);
        ((x as f32 - x0) / (x1 - x0)).clamp(0.0, 1.0)
    }

    /// A one-line text entry. `cursor` (a char index) is drawn while
    /// focused; long text scrolls to keep it in view.
    #[allow(clippy::too_many_arguments)]
    pub fn entry(
        &mut self,
        rect: Rect,
        text: &str,
        placeholder: &str,
        cursor: Option<usize>,
        error: bool,
    ) {
        let t = self.theme;
        let scale = self.canvas.scale();
        self.canvas.rounded_rect(
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            t.button_radius,
            t.entry_background,
        );
        if error {
            self.canvas.stroke_rounded_rect(
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                t.button_radius,
                2.0,
                t.error,
            );
        } else if cursor.is_some() {
            self.canvas.stroke_rounded_rect(
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                t.button_radius,
                2.0,
                t.focus_ring,
            );
        }
        let pad = 10.0;
        let inner = rect.w - pad * 2.0;
        let size = t.font_size;
        let cy = rect.y + rect.h / 2.0;
        if text.is_empty() {
            if !placeholder.is_empty() {
                self.label_v(
                    placeholder,
                    rect.x + pad,
                    cy,
                    inner,
                    size,
                    Weight::Normal,
                    t.window_dim,
                );
            }
            if cursor.is_some() {
                self.canvas
                    .rounded_rect(rect.x + pad, cy - 9.0, 1.5, 18.0, 0.0, t.window_text);
            }
            return;
        }
        let caret = cursor.map(|c| {
            let before: String = text.chars().take(c).collect();
            self.measure(&before, size, Weight::Normal)
        });
        // Scroll so the caret stays inside.
        let shift = caret.map_or(0.0, |c| (c - inner).max(0.0));
        let mut layout = self.fonts.layout(text, size, Weight::Normal, scale);
        let clip = (rect.x + pad - 1.0, rect.y, inner + 2.0, rect.h);
        let tx = rect.x + pad - shift;
        let ty = cy - layout.height / 2.0;
        self.canvas
            .text_clipped(self.fonts, &mut layout, tx, ty, t.window_text, clip);
        if let Some(c) = caret {
            self.canvas
                .rounded_rect(tx + c, cy - 9.0, 1.5, 18.0, 0.0, t.window_text);
        }
    }
}
