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
        let mut system = FontSystem::new();
        // Without fontconfig only /usr/share/fonts and the home font
        // folders are read; NixOS (and other prefixes) keep fonts under the
        // data dirs instead.
        for dir in cnde_config::paths::data_dirs() {
            if dir.starts_with("/usr/share") || dir.starts_with("/usr/local/share") {
                continue;
            }
            for fonts in [dir.join("fonts"), dir.join("X11/fonts")] {
                if fonts.is_dir() {
                    system.db_mut().load_fonts_dir(fonts);
                }
            }
        }
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

/// Ctrl+Backspace in a text field: deletes the word before `cursor` (a char
/// index) along with the spaces between it and the cursor. Returns the new
/// cursor, which is where the deleted text started.
pub fn delete_word_back(text: &mut String, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    *text = chars[..start].iter().chain(&chars[cursor..]).collect();
    start
}

/// Ctrl+Delete: deletes from `cursor` (a char index) through the spaces and
/// the word after it.
pub fn delete_word_forward(text: &mut String, cursor: usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut end = cursor;
    while end < chars.len() && chars[end].is_whitespace() {
        end += 1;
    }
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    *text = chars[..cursor].iter().chain(&chars[end..]).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_backspace_takes_the_word_and_its_spaces() {
        let mut text = String::from("hello big world");
        assert_eq!(delete_word_back(&mut text, 15), 10);
        assert_eq!(text, "hello big ");
        assert_eq!(delete_word_back(&mut text, 10), 6);
        assert_eq!(text, "hello ");
        // Trailing spaces go first, then the word before them.
        let mut text = String::from("a bc   ");
        assert_eq!(delete_word_back(&mut text, 7), 2);
        assert_eq!(text, "a ");
        // Mid-text: only what's before the cursor goes.
        let mut text = String::from("one two");
        assert_eq!(delete_word_back(&mut text, 5), 4);
        assert_eq!(text, "one wo");
        // Nothing before the cursor, and a cursor past the end.
        let mut text = String::from("abc");
        assert_eq!(delete_word_back(&mut text, 0), 0);
        assert_eq!(text, "abc");
        assert_eq!(delete_word_back(&mut text, 99), 0);
        assert_eq!(text, "");
    }

    #[test]
    fn ctrl_backspace_counts_chars_not_bytes() {
        let mut text = String::from("héllo wörld");
        assert_eq!(delete_word_back(&mut text, 11), 6);
        assert_eq!(text, "héllo ");
    }

    #[test]
    fn ctrl_delete_takes_the_spaces_and_the_word_after() {
        let mut text = String::from("hello big world");
        delete_word_forward(&mut text, 5);
        assert_eq!(text, "hello world");
        delete_word_forward(&mut text, 6);
        assert_eq!(text, "hello ");
        let mut text = String::from("abc");
        delete_word_forward(&mut text, 3);
        assert_eq!(text, "abc");
    }
}
