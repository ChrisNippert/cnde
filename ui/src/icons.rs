//! Symbolic icons from the installed icon theme (Adwaita first), rendered
//! once per size with resvg and drawn tinted with any colour, the way GTK
//! draws symbolic icons. Only the alpha channel is kept.

use std::{collections::HashMap, path::PathBuf};

use resvg::{tiny_skia, usvg};

/// Themes searched in order; hicolor is the freedesktop fallback.
const THEMES: &[&str] = &["Adwaita", "hicolor"];

pub struct Icons {
    base_dirs: Vec<PathBuf>,
    /// (name, pixel size) -> coverage mask, or None if the theme lacks it.
    cache: HashMap<(String, u32), Option<Mask>>,
}

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
        Self {
            base_dirs,
            cache: HashMap::new(),
        }
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
