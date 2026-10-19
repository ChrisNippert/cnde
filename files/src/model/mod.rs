//! Stand-in for the data layer; replaced at merge.
#![allow(dead_code)]

use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    time::SystemTime,
};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub hidden: bool,
    pub size: u64,
    pub items: Option<u32>,
    pub modified: Option<SystemTime>,
    pub kind: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Kind,
}

/// Folders first, then by `key`.
pub fn sort(entries: &mut [Entry], key: SortKey, descending: bool) {
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let o = match key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size).then(a.items.cmp(&b.items)),
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Kind => a.kind.cmp(&b.kind),
            };
            let o = if o == Ordering::Equal {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            } else {
                o
            };
            if descending { o.reverse() } else { o }
        })
    });
}

pub fn list_dir(
    path: &Path,
    cancel: &AtomicBool,
    mut batch: impl FnMut(Vec<Entry>),
) -> std::io::Result<()> {
    let mut chunk = Vec::new();
    for item in std::fs::read_dir(path)? {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Ok(());
        }
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let meta = item.metadata().ok();
        let is_symlink = item.file_type().is_ok_and(|t| t.is_symlink());
        let is_dir = item.path().is_dir();
        let items = if is_dir {
            std::fs::read_dir(item.path())
                .ok()
                .map(|d| d.count() as u32)
        } else {
            None
        };
        chunk.push(Entry {
            hidden: name.starts_with('.'),
            kind: kind_of(&name, is_dir),
            path: item.path(),
            is_dir,
            is_symlink,
            size: meta.as_ref().map_or(0, |m| m.len()),
            items,
            modified: meta.and_then(|m| m.modified().ok()),
            name,
        });
        if chunk.len() >= 500 {
            batch(std::mem::take(&mut chunk));
        }
    }
    if !chunk.is_empty() {
        batch(chunk);
    }
    Ok(())
}

pub fn kind_of(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".into();
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => format!("{} file", ext.to_uppercase()),
        _ => "File".into(),
    }
}

pub fn dir_size(path: &Path, cancel: &AtomicBool) -> u64 {
    let mut total = 0;
    let Ok(dir) = std::fs::read_dir(path) else {
        return 0;
    };
    for item in dir.flatten() {
        if cancel.load(AtomicOrdering::Relaxed) {
            break;
        }
        match item.metadata() {
            Ok(m) if m.is_dir() => total += dir_size(&item.path(), cancel),
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

#[derive(Debug, Clone)]
pub struct Drive {
    pub name: String,
    pub mount: PathBuf,
    pub device: String,
    pub fs_type: String,
    pub total: u64,
    pub free: u64,
    pub removable: bool,
}

/// Real disks from /proc/mounts, sized with `df`.
pub fn drives() -> Vec<Drive> {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(device), Some(mount), Some(fs_type)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if !device.starts_with("/dev/") || device.contains("loop") {
            continue;
        }
        let mount = mount.replace("\\040", " ");
        let Ok(df) = std::process::Command::new("df")
            .args(["-B1", "--output=size,avail", &mount])
            .output()
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&df.stdout).into_owned();
        let mut n = text.lines().nth(1).unwrap_or("").split_whitespace();
        let total: u64 = n.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let free: u64 = n.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let name = if mount == "/" {
            "System".to_string()
        } else {
            mount.rsplit('/').next().unwrap_or(&mount).to_string()
        };
        out.push(Drive {
            name,
            removable: mount.starts_with("/run/media") || mount.starts_with("/media"),
            mount: mount.into(),
            device: device.into(),
            fs_type: fs_type.into(),
            total,
            free,
        });
    }
    out
}

pub fn eject(drive: &Drive) -> std::io::Result<()> {
    let status = std::process::Command::new("umount")
        .arg(&drive.mount)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("couldn't unmount"))
    }
}

pub struct Recent {
    pub path: PathBuf,
    pub when: SystemTime,
}

pub fn recent(_limit: usize) -> Vec<Recent> {
    Vec::new()
}

pub fn trash(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("not implemented"))
}

pub fn trash_size() -> u64 {
    dir_size(&trash_dir(), &AtomicBool::new(false))
}

pub fn trash_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_default();
    base.join("Trash/files")
}

pub fn fuzzy_score(query: &str, name: &str) -> Option<i32> {
    name.to_lowercase()
        .contains(&query.to_lowercase())
        .then_some(0)
}

pub fn human_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", units[i])
    } else {
        format!("{v:.1} {}", units[i])
    }
}

pub fn human_time(t: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(t).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "Just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}
