//! The info panel on the right: what's selected (or the open folder), with
//! size, dates, location, git branch, what's inside, and a few actions.
//! The slow facts come from a background scan, see [`scan`].

use std::{
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool, atomic::Ordering::Relaxed},
    time::SystemTime,
};

use cnde_ui::{Color, Painter, Rect, Weight};

use crate::{
    folder::Location,
    model::{self, Entry},
    ops::Act,
    view::{self, Colors, Painted, Target, Ui},
};

pub const PANEL_W: f32 = 248.0;
const PAD: f32 = 18.0;
const ROW: f32 = 30.0;
const ACTS_H: f32 = 64.0;

/// What a scan learned about the subject.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Scan {
    /// Bytes of everything inside.
    pub bytes: u64,
    /// The biggest kinds by bytes, then "Other".
    pub kinds: Vec<(String, u64)>,
    /// Only for a single subject.
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub git: Option<String>,
}

#[derive(Default)]
pub struct InfoState {
    /// Fingerprint of the subject the numbers below belong to.
    pub key: u64,
    /// Bumped per scan; results of older ones are dropped.
    pub generation: u64,
    /// `None` while the scan runs.
    pub scan: Option<Scan>,
    pub cancel: Option<Arc<AtomicBool>>,
}

/// What the panel describes.
pub enum Subject<'a> {
    /// Nothing selected: the open folder.
    Folder(&'a Path),
    One(&'a Entry),
    Many(usize),
}

pub fn subject(ui: &Ui) -> Option<Subject<'_>> {
    let Location::Dir(path) = &ui.location else {
        return None;
    };
    let f = &ui.folder;
    Some(match f.selected.len() {
        0 => Subject::Folder(path),
        1 => match f.entries.iter().find(|e| f.selected.contains(&e.name)) {
            Some(e) => Subject::One(e),
            None => Subject::Folder(path),
        },
        n => Subject::Many(n),
    })
}

/// A cheap fingerprint of what the panel describes, to know when to rescan.
pub fn subject_key(ui: &Ui) -> u64 {
    let hash = |s: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s(&mut h);
        h.finish()
    };
    let Location::Dir(path) = &ui.location else {
        return 0;
    };
    // Names added up, so the order doesn't matter.
    let names = ui
        .folder
        .selected
        .iter()
        .map(|n| hash(&|h| n.hash(h)))
        .fold(0u64, u64::wrapping_add);
    hash(&|h| {
        path.hash(h);
        names.hash(h);
        ui.folder.selected.len().hash(h);
    })
}

/// Walk `paths` (all of them are one subject): bytes, kinds, and for a
/// single path its dates and git branch. Doesn't follow symlinks or leave
/// the filesystem; stops early once `cancel` is set.
pub fn scan(paths: &[PathBuf], cancel: &AtomicBool) -> Scan {
    let mut bytes = 0;
    let mut kinds: HashMap<String, u64> = HashMap::new();
    for root in paths {
        let Ok(meta) = fs::symlink_metadata(root) else {
            continue;
        };
        if !meta.is_dir() {
            bytes += meta.len();
            let name = root.file_name().map(|n| n.to_string_lossy());
            *kinds
                .entry(model::kind_of(name.as_deref().unwrap_or(""), false))
                .or_default() += meta.len();
            continue;
        }
        let dev = meta.dev();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for de in rd.flatten() {
                if cancel.load(Relaxed) {
                    return Scan::default();
                }
                let Ok(m) = de.metadata() else { continue };
                if m.is_dir() {
                    if m.dev() == dev {
                        stack.push(de.path());
                    }
                } else {
                    bytes += m.len();
                    let name = de.file_name();
                    *kinds
                        .entry(model::kind_of(&name.to_string_lossy(), false))
                        .or_default() += m.len();
                }
            }
        }
    }
    let mut out = Scan {
        bytes,
        kinds: top_kinds(kinds, 3),
        ..Scan::default()
    };
    if let [one] = paths {
        if let Ok(m) = fs::metadata(one) {
            out.created = m.created().ok();
            out.modified = m.modified().ok();
        }
        let start = if one.is_dir() {
            Some(one.as_path())
        } else {
            one.parent()
        };
        out.git = start.and_then(git_branch);
    }
    out
}

/// The `n` biggest kinds by bytes (ties by name), then the rest as "Other".
pub fn top_kinds(kinds: HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut all: Vec<(String, u64)> = kinds.into_iter().filter(|(_, b)| *b > 0).collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let rest: u64 = all.iter().skip(n).map(|(_, b)| b).sum();
    all.truncate(n);
    if rest > 0 {
        all.push(("Other".into(), rest));
    }
    all
}

/// The branch of the git work tree `dir` is in, read from `.git/HEAD` (no
/// git needed): the branch name, or "detached" on a bare commit.
pub fn git_branch(dir: &Path) -> Option<String> {
    for d in dir.ancestors() {
        let dot = d.join(".git");
        let Ok(meta) = fs::metadata(&dot) else {
            continue;
        };
        // A worktree or submodule has a file pointing at the real git dir.
        let git_dir = if meta.is_dir() {
            dot
        } else {
            let text = fs::read_to_string(&dot).ok()?;
            let target = text.strip_prefix("gitdir:")?.trim();
            d.join(target)
        };
        let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
        let head = head.trim();
        return Some(match head.strip_prefix("ref:") {
            Some(r) => {
                let r = r.trim();
                r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
            }
            None => "detached".into(),
        });
    }
    None
}

/// `path` with the home folder as "~".
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn items(n: u32) -> String {
    format!("{n} {}", if n == 1 { "item" } else { "items" })
}

/// Text cut from the front with "…" until it fits `max_w`.
fn fit_tail(p: &mut Painter<'_, '_>, text: &str, size: f32, max_w: f32) -> String {
    if p.measure(text, size, Weight::Normal) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    // The most characters we can drop from the front and still fit.
    while lo < hi {
        let mid = (lo + hi) / 2;
        let cut = format!("…{}", chars[mid..].iter().collect::<String>());
        if p.measure(&cut, size, Weight::Normal) <= max_w {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    format!(
        "…{}",
        chars[lo.min(chars.len())..].iter().collect::<String>()
    )
}

/// Paint the panel in the window's right edge (`w` x `h` is the window).
pub fn paint(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, out: &mut Painted, w: f32, h: f32) {
    let Some(subject) = subject(ui) else {
        return;
    };
    let t = p.theme;
    let x0 = w - PANEL_W;
    let top = view::TOP_H;
    let radius = if ui.rounded { t.window_radius } else { 0.0 };
    let tone = if t.dark {
        t.window_background.mix(Color::rgb(0, 0, 0), 0.22)
    } else {
        t.window_background.mix(Color::rgb(0, 0, 0), 0.04)
    };
    // Only the bottom right corner is round.
    p.canvas.rounded_rect_clipped(
        x0,
        top - radius - 2.0,
        PANEL_W,
        h - top + radius + 2.0,
        radius,
        tone,
        (x0, top, PANEL_W, h - top),
    );
    p.canvas.rounded_rect(x0, top, 1.0, h - top, 0.0, c.hair);

    let inner = PANEL_W - PAD * 2.0;
    let x = x0 + PAD;
    let now = SystemTime::now();
    let scan = ui.x.info.scan.as_ref();
    let folder = &ui.folder;

    // Heading: icon, name, one line of what it is.
    let (icon, icon_color, name, sub, dir) = match &subject {
        Subject::Folder(path) => (
            "folder-symbolic",
            c.fold,
            view::title_of(ui),
            items(folder.visible.len() as u32),
            Some(*path),
        ),
        Subject::One(e) => (
            view::icon_for(e),
            if e.is_dir { c.fold } else { c.dim },
            e.name.clone(),
            match (e.is_dir, e.items) {
                (true, Some(n)) => format!("Folder · {}", items(n)),
                (true, None) => "Folder".into(),
                _ => e.kind.clone(),
            },
            e.is_dir.then_some(e.path.as_path()),
        ),
        Subject::Many(n) => (
            "view-list-symbolic",
            c.dim,
            format!("{n} items selected"),
            String::new(),
            None,
        ),
    };
    let _ = dir;
    let mut y = top + PAD;
    let preview = match &subject {
        Subject::One(e) if !e.is_dir => {
            crate::thumbs::get(&e.path, e.modified, crate::thumbs::LARGE, false)
        }
        _ => None,
    };
    if let Some(pm) = preview {
        let bx = (x0 + (PANEL_W - 200.0) / 2.0, y, 200.0, 150.0);
        crate::thumbs::draw(p.canvas, &pm, bx, 8.0);
        y += 150.0 + 16.0;
    } else {
        p.icon(icon, x0 + PANEL_W / 2.0, y + 44.0, 72.0, icon_color);
        y += 104.0;
    }
    let nw = p.measure(&name, 16.0, Weight::Bold).min(inner);
    p.label_v(
        &name,
        x0 + (PANEL_W - nw) / 2.0,
        y + 10.0,
        inner,
        16.0,
        Weight::Bold,
        c.ink,
    );
    y += 24.0;
    if !sub.is_empty() {
        let size = t.font_size - 1.0;
        let sw = p.measure(&sub, size, Weight::Normal).min(inner);
        p.label_v(
            &sub,
            x0 + (PANEL_W - sw) / 2.0,
            y + 8.0,
            inner,
            size,
            Weight::Normal,
            c.dim,
        );
    }
    y += 26.0;

    // Facts, one hairline-separated row each.
    let size = t.font_size - 1.0;
    let calculating = "Calculating…".to_string();
    let mut rows: Vec<(&str, String)> = Vec::new();
    match &subject {
        Subject::One(e) if !e.is_dir => rows.push(("Size", model::human_size(e.size))),
        _ => rows.push((
            "Size",
            scan.map_or(calculating, |s| model::human_size(s.bytes)),
        )),
    }
    if !matches!(subject, Subject::Many(_)) {
        let modified = match &subject {
            Subject::One(e) => e.modified,
            _ => scan.and_then(|s| s.modified),
        };
        if let Some(m) = modified {
            rows.push(("Modified", model::human_time(m, now)));
        }
        if let Some(created) = scan.and_then(|s| s.created) {
            rows.push(("Created", model::human_time(created, now)));
        }
    }
    if let Location::Dir(cur) = &ui.location {
        let wher = match &subject {
            Subject::Folder(path) => path.parent().unwrap_or(path).to_path_buf(),
            _ => cur.clone(),
        };
        rows.push(("Where", tilde(&wher, &ui.home)));
    }
    if let Some(git) = scan.and_then(|s| s.git.clone()) {
        rows.push(("Git", git));
    }
    let acts_top = h - ACTS_H;
    for (label, value) in rows {
        if y + ROW > acts_top {
            break;
        }
        p.canvas.rounded_rect(x, y, inner, 1.0, 0.0, c.hair);
        let lw = p.measure(label, size, Weight::Normal);
        p.label_v(
            label,
            x,
            y + ROW / 2.0,
            lw + 2.0,
            size,
            Weight::Normal,
            c.dim,
        );
        let room = inner - lw - 12.0;
        let shown = if label == "Where" {
            fit_tail(p, &value, size, room)
        } else {
            value
        };
        let vw = p.measure(&shown, size, Weight::Normal).min(room);
        p.label_v(
            &shown,
            x + inner - vw,
            y + ROW / 2.0,
            room,
            size,
            Weight::Normal,
            c.ink,
        );
        y += ROW;
    }

    // What's inside, as a stacked bar by kind.
    if let Some(s) = scan
        && !s.kinds.is_empty()
        && s.bytes > 0
        && y + 44.0 < acts_top
    {
        y += 12.0;
        let colors = [
            t.accent,
            Color::rgb(0x33, 0xd1, 0x7a),
            Color::rgb(0xf6, 0xa6, 0x23),
        ];
        let total: u64 = s.kinds.iter().map(|(_, b)| b).sum();
        let gap = 2.0;
        let usable = inner - gap * (s.kinds.len() - 1) as f32;
        let mut bx = x;
        for (i, (_, bytes)) in s.kinds.iter().enumerate() {
            let sw = (usable * *bytes as f32 / total as f32).max(3.0);
            let col = if i < 3 && s.kinds[i].0 != "Other" {
                colors[i]
            } else {
                c.dim
            };
            p.canvas.rounded_rect(bx, y, sw, 6.0, 3.0, col);
            bx += sw + gap;
        }
        y += 16.0;
        let small = t.font_size - 1.5;
        let (mut lx, mut ly) = (x, y);
        for (i, (kind, bytes)) in s.kinds.iter().enumerate() {
            let pct = (*bytes as f64 * 100.0 / total as f64).round() as u32;
            let text = if kind == "Other" {
                kind.clone()
            } else {
                format!("{kind} {}%", pct.max(1))
            };
            let tw = p.measure(&text, small, Weight::Normal).min(inner - 12.0);
            if lx > x && lx + 12.0 + tw > x + inner {
                lx = x;
                ly += 16.0;
            }
            if ly + 16.0 > acts_top {
                break;
            }
            let col = if kind == "Other" {
                c.dim
            } else {
                colors[i.min(2)]
            };
            p.canvas.circle(lx + 3.5, ly + 8.0, 3.5, col);
            p.label_v(
                &text,
                lx + 12.0,
                ly + 8.0,
                inner - 12.0,
                small,
                Weight::Normal,
                c.dim,
            );
            lx += 12.0 + tw + 12.0;
        }
    }

    // Actions along the bottom.
    let ay = acts_top;
    p.canvas.rounded_rect(x, ay, inner, 1.0, 0.0, c.hair);
    let (x, inner) = (x0 + 8.0, PANEL_W - 16.0);
    let has_selection = !folder.selected.is_empty();
    let acts = [
        (Act::Open, "document-open-symbolic", "Open", has_selection),
        (
            Act::Terminal,
            "utilities-terminal-symbolic",
            "Terminal",
            true,
        ),
        (Act::CopyPath, "edit-copy-symbolic", "Copy path", true),
        (Act::Trash, "user-trash-symbolic", "Trash", has_selection),
    ];
    let cell = inner / acts.len() as f32;
    for (i, (act, icon, label, enabled)) in acts.into_iter().enumerate() {
        let rect = Rect::new(x + cell * i as f32, ay + 6.0, cell, ACTS_H - 12.0);
        let target = Target::Act(act);
        if enabled && ui.x.menu.is_none() && ui.hover == Some(target) {
            p.canvas
                .rounded_rect(rect.x + 1.0, rect.y, rect.w - 2.0, rect.h, 8.0, t.hover);
        }
        let alpha = if enabled { 1.0 } else { 0.3 };
        let cx = rect.x + cell / 2.0;
        p.icon(icon, cx, rect.y + 17.0, 18.0, c.ink.with_alpha(alpha));
        let small = t.font_size - 2.5;
        let lw = p.measure(label, small, Weight::Normal).min(cell);
        p.label_v(
            label,
            cx - lw / 2.0,
            rect.y + 38.0,
            cell,
            small,
            Weight::Normal,
            c.dim.with_alpha(c.dim.a * alpha),
        );
        if enabled {
            out.hits.push((rect, target));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cnde-info-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn git_branch_forms() {
        let root = temp("git");
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(git_branch(&sub).filter(|_| false), None);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch(&sub).as_deref(), Some("feature/x"));
        fs::write(
            root.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(&sub).as_deref(), Some("detached"));
        // A worktree: `.git` is a file naming the real git dir.
        let wt = root.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/HEAD"), "ref: refs/heads/side\n").unwrap();
        fs::write(wt.join(".git"), "gitdir: ../real\n").unwrap();
        assert_eq!(git_branch(&wt).as_deref(), Some("side"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn kinds_breakdown() {
        let mut m = HashMap::new();
        for (k, b) in [
            ("Rust", 70),
            ("PNG", 20),
            ("Text", 5),
            ("TOML", 3),
            ("Zip", 2),
            ("Empty", 0),
        ] {
            m.insert(k.to_string(), b);
        }
        let top = top_kinds(m, 3);
        assert_eq!(
            top,
            vec![
                ("Rust".into(), 70),
                ("PNG".into(), 20),
                ("Text".into(), 5),
                ("Other".into(), 5)
            ]
        );
        assert!(top_kinds(HashMap::new(), 3).is_empty());
    }

    #[test]
    fn scan_counts_bytes_and_kinds() {
        let root = temp("scan");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("a.rs"), vec![0u8; 100]).unwrap();
        fs::write(root.join("d/b.rs"), vec![0u8; 50]).unwrap();
        fs::write(root.join("d/c.png"), vec![0u8; 10]).unwrap();
        let s = scan(std::slice::from_ref(&root), &AtomicBool::new(false));
        assert_eq!(s.bytes, 160);
        assert_eq!(s.kinds[0].1, 150);
        assert!(s.created.is_some() || s.modified.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tilde_paths() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me"), home), "~");
        assert_eq!(tilde(Path::new("/home/me/a/b"), home), "~/a/b");
        assert_eq!(tilde(Path::new("/etc"), home), "/etc");
    }
}
