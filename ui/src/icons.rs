//! Icons from the installed icon themes, rendered once per size.
//!
//! Symbolic icons (Adwaita first) keep only their alpha channel and are drawn
//! tinted with any colour, the way GTK draws them. App icons are full colour,
//! from SVG or PNG, looked up in the user's GTK icon theme, then hicolor
//! (where nearly every app installs its icon), then Adwaita.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use resvg::{tiny_skia, usvg};

/// Themes searched in order; hicolor is the freedesktop fallback.
const THEMES: &[&str] = &["Adwaita", "hicolor"];

pub struct Icons {
    base_dirs: Vec<PathBuf>,
    /// (name, pixel size) -> coverage mask, or None if the theme lacks it.
    cache: HashMap<(String, u32), Option<Mask>>,
    /// Themes searched for app icons, in order.
    app_themes: Vec<String>,
    /// (name, pixel size) -> rendered app icon. Bounded: cleared when full.
    app_cache: HashMap<(String, u32), Option<tiny_skia::Pixmap>>,
}

/// More app icons than this in the cache clears it (~4 MB at 2x scale).
const APP_CACHE_LIMIT: usize = 96;

pub struct Mask {
    pub size: u32,
    pub alpha: Vec<u8>,
}

impl Icons {
    pub fn new() -> Self {
        let mut base_dirs = Vec::new();
        if let Some(home) = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        {
            base_dirs.push(home.join("icons"));
        }
        let data_dirs =
            std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        base_dirs.extend(
            data_dirs
                .split(':')
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d).join("icons")),
        );
        let mut app_themes: Vec<String> = gtk_icon_theme().into_iter().collect();
        for theme in ["hicolor", "Adwaita"] {
            if !app_themes.iter().any(|t| t == theme) {
                app_themes.push(theme.into());
            }
        }
        Self {
            base_dirs,
            cache: HashMap::new(),
            app_themes,
            app_cache: HashMap::new(),
        }
    }

    /// A full-colour app icon (a desktop entry's `Icon=`: a theme name or an
    /// absolute path), `size` device pixels square.
    pub fn app(&mut self, name: &str, size: u32) -> Option<&tiny_skia::Pixmap> {
        let key = (name.to_string(), size);
        if !self.app_cache.contains_key(&key) {
            if self.app_cache.len() >= APP_CACHE_LIMIT {
                self.app_cache.clear();
            }
            let pixmap = self
                .find_app(name, size)
                .and_then(|path| render_color(&path, size));
            self.app_cache.insert(key.clone(), pixmap);
        }
        self.app_cache.get(&key)?.as_ref()
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
        for ext in ["svg", "png"] {
            let path = PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"));
            if path.is_file() {
                return Some(path);
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

    /// `<base>/<theme>/{symbolic,scalable}/<context>/<name>.svg`.
    fn find(&self, name: &str) -> Option<PathBuf> {
        let file = format!("{name}.svg");
        for base in &self.base_dirs {
            for theme in THEMES {
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
        let image = tiny_skia::Pixmap::load_png(path).ok()?;
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
