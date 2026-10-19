//! The window side of the sidebar's custom shortcuts: loading them,
//! changing the bookmarks file, dragging them into order and the rename.

use std::path::{Path, PathBuf};

use crate::{
    App, Event,
    bookmarks::{self, Bookmarks, Entry},
    folder::Location,
    ops::Rename,
    view::{self, Mark, SideSlot, Target},
};

/// A press on a bookmark that may turn into a drag to reorder.
#[derive(Debug, Clone, Copy)]
pub struct Drag {
    pub index: usize,
    pub at: (f64, f64),
    /// Moved far enough to be a drag, not a click.
    pub active: bool,
}

const DRAG_DISTANCE: f64 = 6.0;

fn config_home() -> PathBuf {
    crate::apps::Env::from_env().config_home
}

/// The folders listed in the bookmarks file that exist.
fn read_entries(config: &Path, home: &Path) -> Vec<Entry> {
    bookmarks::load(config, home)
        .entries()
        .into_iter()
        .filter(|e| e.path.is_dir())
        .collect()
}

/// Where in the file the bookmark on `path` is.
fn line_of(bm: &Bookmarks, path: &Path) -> Option<usize> {
    bm.entries().iter().find(|e| e.path == path).map(|e| e.line)
}

/// Read the bookmarks file on a thread and report it.
pub fn start(
    tx: smithay_client_toolkit::reexports::calloop::channel::Sender<Event>,
    home: PathBuf,
) {
    let _ = std::thread::Builder::new()
        .name("bookmarks".into())
        .spawn(move || {
            let _ = tx.send(Event::Bookmarks(read_entries(&config_home(), &home)));
        });
}

impl App {
    /// Read the bookmarks file again, on a thread.
    pub(crate) fn reload_bookmarks(&self) {
        start(self.events.clone(), self.ui.home.clone());
    }

    /// Show `entries`, less what the standard places already are.
    pub(crate) fn set_bookmarks(&mut self, entries: Vec<Entry>) {
        self.ui.x.bookmark_entries = entries;
        self.apply_bookmarks();
    }

    pub(crate) fn apply_bookmarks(&mut self) {
        let mut seen: Vec<&Path> = self.ui.places.iter().map(|p| p.path.as_path()).collect();
        let trash = crate::model::trash_dir();
        seen.push(&trash);
        let mut marks = Vec::new();
        for e in &self.ui.x.bookmark_entries {
            if seen.contains(&e.path.as_path()) {
                continue;
            }
            seen.push(&e.path);
            marks.push(Mark {
                name: e.name(),
                path: e.path.clone(),
            });
        }
        self.ui.bookmarks = marks;
    }

    /// Change the bookmarks file on a thread, then show the result.
    pub(crate) fn edit_bookmarks(&self, change: impl FnOnce(&mut Bookmarks) + Send + 'static) {
        let (tx, home) = (self.events.clone(), self.ui.home.clone());
        let _ = std::thread::Builder::new()
            .name("bookmarks".into())
            .spawn(move || {
                let config = config_home();
                if let Err(err) = bookmarks::update(&config, &home, change) {
                    let _ = tx.send(Event::Error(format!("Can't save the bookmarks: {err}")));
                }
                let _ = tx.send(Event::Bookmarks(read_entries(&config, &home)));
            });
    }

    /// Add folders to the sidebar, before the bookmark at slot `at` (the
    /// end if none).
    pub(crate) fn add_bookmarks(&mut self, dirs: Vec<PathBuf>, at: Option<usize>) {
        let before = at
            .and_then(|k| self.ui.bookmarks.get(k))
            .map(|m| m.path.clone());
        let standard: Vec<PathBuf> = self.ui.places.iter().map(|p| p.path.clone()).collect();
        let dirs: Vec<PathBuf> = dirs
            .into_iter()
            .filter(|d| d.is_dir() && !standard.contains(d))
            .collect();
        if dirs.is_empty() {
            return;
        }
        self.edit_bookmarks(move |bm| {
            let at = before.as_deref().and_then(|b| line_of(bm, b));
            let mut added = 0;
            for d in &dirs {
                // Each lands just after the one before it.
                if bm.add(d, at.map(|a| a + added)) {
                    added += 1;
                }
            }
        });
    }

    pub(crate) fn remove_bookmark(&mut self, i: usize) {
        let Some(path) = self.ui.bookmarks.get(i).map(|m| m.path.clone()) else {
            return;
        };
        self.edit_bookmarks(move |bm| {
            if let Some(line) = line_of(bm, &path) {
                bm.remove(line);
            }
        });
    }

    /// The selected folders of the open folder, for "Add to Sidebar".
    pub(crate) fn selected_dirs(&self) -> Vec<PathBuf> {
        let f = &self.ui.folder;
        f.visible
            .iter()
            .map(|&i| &f.entries[i])
            .filter(|e| e.is_dir && f.selected.contains(&e.name))
            .map(|e| e.path.clone())
            .collect()
    }

    pub(crate) fn open_bookmark(&mut self, i: usize) {
        if let Some(m) = self.ui.bookmarks.get(i) {
            let path = m.path.clone();
            self.go(Location::Dir(path));
        }
    }

    pub(crate) fn bookmark_terminal(&mut self, i: usize) {
        if let Some(m) = self.ui.bookmarks.get(i) {
            let dir = m.path.clone();
            self.terminal_in(dir);
        }
    }

    pub(crate) fn start_bookmark_rename(&mut self, i: usize) {
        let Some(m) = self.ui.bookmarks.get(i) else {
            return;
        };
        self.ui.x.rename = Some(Rename {
            cursor: m.name.chars().count(),
            text: m.name.clone(),
            name: m.name.clone(),
            fresh: true,
            bookmark: Some(i),
        });
    }

    /// Enter on a bookmark's rename: store the label (the folder's own
    /// name means no label).
    pub(crate) fn commit_bookmark_rename(&mut self, i: usize, text: String) {
        let Some(m) = self.ui.bookmarks.get(i) else {
            return;
        };
        let text = text.trim().to_string();
        let own = m
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = if text == own { String::new() } else { text };
        let path = m.path.clone();
        self.edit_bookmarks(move |bm| {
            if let Some(line) = line_of(bm, &path) {
                bm.rename(line, &label);
            }
        });
    }

    /// Right-click on a bookmark of the sidebar: true if it was one.
    pub(crate) fn bookmark_context(&mut self, position: (f64, f64)) -> bool {
        let Some(Target::Bookmark(i)) = self.painted.hit(position) else {
            return false;
        };
        self.ui.x.menu = Some(crate::ops::Menu {
            bookmark: Some(i),
            ..crate::ops::Menu::at(position.0 as f32, position.1 as f32)
        });
        self.redraw();
        true
    }

    // ---- reordering by dragging ----

    pub(crate) fn bookmark_arm(&mut self, index: usize, at: (f64, f64)) {
        self.ui.x.bm_drag = Some(Drag {
            index,
            at,
            active: false,
        });
    }

    /// Pointer motion while a bookmark is held; true if it took it.
    pub(crate) fn bookmark_motion(&mut self, to: (f64, f64)) -> bool {
        let Some(mut drag) = self.ui.x.bm_drag else {
            return false;
        };
        if !drag.active && (to.0 - drag.at.0).hypot(to.1 - drag.at.1) >= DRAG_DISTANCE {
            drag.active = true;
            self.ui.x.bm_drag = Some(drag);
        }
        if drag.active {
            let slot = view::side_slot(&self.ui, self.size.1 as f32, to, true);
            self.ui.x.bm_insert = match slot {
                Some(SideSlot::Insert(k)) => Some(k),
                _ => None,
            };
            self.ui.hover = None;
            self.redraw();
        }
        true
    }

    /// The button went up: open the bookmark that was clicked, or put the
    /// dragged one in its new place.
    pub(crate) fn bookmark_release(&mut self) {
        let Some(drag) = self.ui.x.bm_drag.take() else {
            return;
        };
        let slot = self.ui.x.bm_insert.take();
        if !drag.active {
            self.open_bookmark(drag.index);
        } else if let Some(k) = slot
            && k != drag.index
            && k != drag.index + 1
            && let Some(path) = self.ui.bookmarks.get(drag.index).map(|m| m.path.clone())
        {
            let before = self.ui.bookmarks.get(k).map(|m| m.path.clone());
            self.edit_bookmarks(move |bm| {
                if let Some(line) = line_of(bm, &path) {
                    let at = before.as_deref().and_then(|b| line_of(bm, b));
                    bm.move_entry(line, at);
                }
            });
        }
        self.redraw();
    }
}
