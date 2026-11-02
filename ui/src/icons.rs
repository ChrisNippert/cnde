//! Icons from the installed icon themes, rendered once per size.
//!
//! Symbolic icons (the configured theme, then cnde's own, then Adwaita) keep
//! only their alpha channel and are drawn tinted with any colour, the way
//! GTK draws them. App icons are full colour,
//! from SVG or PNG, looked up in the user's GTK icon theme, then hicolor
//! (where nearly every app installs its icon), then Adwaita.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use resvg::{tiny_skia, usvg};

/// Symbolic themes searched after the configured one; hicolor is the
/// freedesktop fallback.
const THEMES: &[&str] = &["cnde", "Adwaita", "hicolor"];

pub struct Icons {
    base_dirs: Vec<PathBuf>,
    /// Symbolic themes, in search order.
    themes: Vec<String>,
    /// (name, pixel size) -> coverage mask, or None if the theme lacks it.
    cache: HashMap<(String, u32), Option<Mask>>,
    /// Themes searched for app icons, in order.
    app_themes: Vec<String>,
    /// (name, pixel size) -> rendered app icon, and when it was last used.
    /// Bounded: the least recently used goes when full.
    app_cache: HashMap<(String, u32), (Option<tiny_skia::Pixmap>, u64)>,
    /// Counts app icon lookups, for `app_cache`'s last use.
    app_uses: u64,
}

/// App icons kept rendered (~12 MB of grid icons at 2x scale): two app
/// grid pages and the dock fit with room, so sliding pages never renders
/// an icon twice.
const APP_CACHE_LIMIT: usize = 256;

pub struct Mask {
    pub size: u32,
    pub alpha: Vec<u8>,
}

impl Icons {
    pub fn new() -> Self {
        // The user's, the system's, then the prefix cnde was installed to
        // (a ~/.local or /opt install isn't in XDG_DATA_DIRS).
        let mut base_dirs: Vec<PathBuf> = cnde_config::paths::search_dirs()
            .into_iter()
            .map(|dir| dir.join("icons"))
            .collect();
        // A build that isn't installed: cnde's icons from the source tree.
        let source = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../icons"));
        if source.is_dir() {
            base_dirs.push(source);
        }
        let mut app_themes: Vec<String> = gtk_icon_theme().into_iter().collect();
        // AdwaitaLegacy has the classic full-colour icons (preferences-system).
        for theme in ["hicolor", "Adwaita", "AdwaitaLegacy"] {
            if !app_themes.iter().any(|t| t == theme) {
                app_themes.push(theme.into());
            }
        }
        Self {
            base_dirs,
            themes: THEMES.iter().map(|t| t.to_string()).collect(),
            cache: HashMap::new(),
            app_themes,
            app_cache: HashMap::new(),
            app_uses: 0,
        }
    }

    /// A full-colour app icon (a desktop entry's `Icon=`: a theme name or an
    /// absolute path), `size` device pixels square.
    pub fn app(&mut self, name: &str, size: u32) -> Option<&tiny_skia::Pixmap> {
        let key = (name.to_string(), size);
        self.app_uses += 1;
        let now = self.app_uses;
        if let Some(entry) = self.app_cache.get_mut(&key) {
            entry.1 = now;
        } else {
            if self.app_cache.len() >= APP_CACHE_LIMIT
                && let Some(oldest) = self
                    .app_cache
                    .iter()
                    .min_by_key(|(_, (_, used))| *used)
                    .map(|(k, _)| k.clone())
            {
                self.app_cache.remove(&oldest);
            }
            let pixmap = self
                .find_app(name, size)
                .and_then(|path| render_color(&path, size));
            self.app_cache.insert(key.clone(), (pixmap, now));
        }
        self.app_cache.get(&key)?.0.as_ref()
    }

    /// Scalable first, then the largest PNG at least `size`, else the largest.
    fn find_app(&self, name: &str, size: u32) -> Option<PathBuf> {
        if name.starts_with('/') {
            return Path::new(name).is_file().then(|| name.into());
        }
        if name.is_empty() {
            return None;
        }
        let svg = format!("{name}.svg");
        let png = format!("{name}.png");
        for theme in &self.app_themes {
            for base in &self.base_dirs {
                let theme_dir = base.join(theme);
                let Ok(dirs) = std::fs::read_dir(&theme_dir) else {
                    continue;
                };
                // Layouts differ: hicolor is <size>/apps, Papirus apps/<size>.
                let mut candidates: Vec<(u32, PathBuf)> = Vec::new();
                for dir in dirs.flatten() {
                    let dir_name = dir.file_name().to_string_lossy().into_owned();
                    let dir_size = parse_size(&dir_name);
                    let sub_dirs = std::fs::read_dir(dir.path())
                        .into_iter()
                        .flatten()
                        .flatten();
                    for sub in sub_dirs {
                        let sub_size = parse_size(&sub.file_name().to_string_lossy());
                        let icon_size = dir_size.or(sub_size);
                        for (file, is_svg) in [(&svg, true), (&png, false)] {
                            let path = sub.path().join(file);
                            if path.is_file() {
                                // Scalable (no size, or an SVG) beats any bitmap.
                                let rank = if is_svg || icon_size.is_none() {
                                    u32::MAX
                                } else {
                                    icon_size.unwrap_or(0)
                                };
                                candidates.push((rank, path));
                            }
                        }
                    }
                }
                let best = candidates
                    .iter()
                    .filter(|(rank, _)| *rank >= size)
                    .min_by_key(|(rank, _)| *rank)
                    .or_else(|| candidates.iter().max_by_key(|(rank, _)| *rank));
                if let Some((_, path)) = best {
                    return Some(path.clone());
                }
            }
        }
        for dir in cnde_config::paths::search_dirs() {
            for ext in ["svg", "png"] {
                let path = dir.join(format!("pixmaps/{name}.{ext}"));
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }

    /// The icon's coverage at `size` device pixels, rendering on first use.
    pub fn get(&mut self, name: &str, size: u32) -> Option<&Mask> {
        let key = (name.to_string(), size);
        if !self.cache.contains_key(&key) {
            let mask = self.find(name).and_then(|path| render(&path, size));
            self.cache.insert(key.clone(), mask);
        }
        self.cache.get(&key)?.as_ref()
    }

    /// Prefer `theme` for symbolic icons (`appearance.icon_theme`); empty
    /// or "cnde" means cnde's own. Missing icons come from cnde's, then
    /// Adwaita.
    pub fn set_theme(&mut self, theme: &str) {
        let mut themes: Vec<String> = Vec::new();
        if !theme.is_empty() {
            themes.push(theme.to_string());
        }
        for fallback in THEMES {
            if !themes.iter().any(|t| t.eq_ignore_ascii_case(fallback)) {
                themes.push(fallback.to_string());
            }
        }
        if themes != self.themes {
            self.themes = themes;
            self.cache.clear();
        }
    }

    /// `<base>/<theme>/{symbolic,scalable}/<context>/<name>.svg`; themes
    /// first, so a theme anywhere beats the next one.
    fn find(&self, name: &str) -> Option<PathBuf> {
        let file = format!("{name}.svg");
        for theme in &self.themes {
            for base in &self.base_dirs {
                for kind in ["symbolic", "scalable"] {
                    let Ok(contexts) = std::fs::read_dir(base.join(theme).join(kind)) else {
                        continue;
                    };
                    for context in contexts.flatten() {
                        let path = context.path().join(&file);
                        if path.is_file() {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }
}

impl Default for Icons {
    fn default() -> Self {
        Self::new()
    }
}

fn render(path: &PathBuf, size: u32) -> Option<Mask> {
    let data = std::fs::read(path).ok()?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let svg = tree.size();
    let transform =
        tiny_skia::Transform::from_scale(size as f32 / svg.width(), size as f32 / svg.height());
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(Mask {
        size,
        alpha: pixmap.pixels().iter().map(|p| p.alpha()).collect(),
    })
}

/// "48x48" or "48x48@2" or "48" -> 48; "scalable", "symbolic" -> None.
fn parse_size(dir: &str) -> Option<u32> {
    let base = dir.split('@').next()?;
    let first = base.split('x').next()?;
    first.parse().ok()
}

/// The icon theme GTK apps use, from GTK's settings.ini (GNOME's gsettings
/// value isn't readable without dconf).
fn gtk_icon_theme() -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    ["gtk-4.0", "gtk-3.0"].iter().find_map(|version| {
        let text = std::fs::read_to_string(config.join(version).join("settings.ini")).ok()?;
        text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "gtk-icon-theme-name").then(|| value.trim().to_string())
        })
    })
}

/// An SVG or PNG scaled into a `size` square, aspect kept, centred.
fn render_color(path: &Path, size: u32) -> Option<tiny_skia::Pixmap> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let is_svg = path.extension().is_some_and(|e| e == "svg" || e == "svgz");
    if is_svg {
        let data = std::fs::read(path).ok()?;
        let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).ok()?;
        let svg = tree.size();
        let scale = size as f32 / svg.width().max(svg.height());
        let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(
            (size as f32 - svg.width() * scale) / 2.0,
            (size as f32 - svg.height() * scale) / 2.0,
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
    } else {
        let mut image = tiny_skia::Pixmap::load_png(path).ok()?;
        // Bicubic reads a 4x4 neighbourhood, so shrinking a 512 px icon to
        // 32 in one go skips most pixels and aliases. Halve first.
        while image.width().min(image.height()) >= 2 * size {
            image = halve(&image)?;
        }
        let (w, h) = (image.width() as f32, image.height() as f32);
        let scale = size as f32 / w.max(h);
        let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(
            (size as f32 - w * scale) / 2.0,
            (size as f32 - h * scale) / 2.0,
        );
        let paint = tiny_skia::PixmapPaint {
            quality: tiny_skia::FilterQuality::Bicubic,
            ..Default::default()
        };
        pixmap.draw_pixmap(0, 0, image.as_ref(), &paint, transform, None);
    }
    Some(pixmap)
}

/// Half the size, each pixel the average of four (premultiplied, so
/// averaging is exact).
fn halve(image: &tiny_skia::Pixmap) -> Option<tiny_skia::Pixmap> {
    let (w, h) = (image.width() / 2, image.height() / 2);
    let mut out = tiny_skia::Pixmap::new(w.max(1), h.max(1))?;
    let src = image.data();
    let stride = image.width() as usize * 4;
    let dst = out.data_mut();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = 2 * y * stride + 8 * x;
            for c in 0..4 {
                let sum = src[i + c] as u32
                    + src[i + 4 + c] as u32
                    + src[i + stride + c] as u32
                    + src[i + stride + 4 + c] as u32;
                dst[(y * w as usize + x) * 4 + c] = ((sum + 2) / 4) as u8;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_averages_each_square_of_four() {
        // A 2x2 checkerboard of opaque white and transparent pixels.
        let mut image = tiny_skia::Pixmap::new(2, 2).unwrap();
        for (i, px) in image.data_mut().chunks_mut(4).enumerate() {
            if i == 0 || i == 3 {
                px.copy_from_slice(&[255; 4]);
            }
        }
        let half = halve(&image).unwrap();
        assert_eq!((half.width(), half.height()), (1, 1));
        assert_eq!(half.data(), &[128, 128, 128, 128]);
    }
}
