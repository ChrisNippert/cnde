//! Doing things to files: trash, rename, new folder, a terminal, copying a
//! path, the right-click menu, and following the open folder on disk. The
//! slow parts run on threads and report back as events.

use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicBool, atomic::Ordering},
    time::Duration,
};

use cnde_ui::{Painter, Rect, Weight};
use rustix::{
    fs::inotify::{self, CreateFlags, WatchFlags},
    io::Errno,
};
use smithay_client_toolkit::{
    reexports::calloop::{
        Interest, Mode, PostAction, RegistrationToken,
        generic::Generic,
        timer::{TimeoutAction, Timer},
    },
    seat::keyboard::{KeyEvent, Keysym},
};
use tracing::debug;

use crate::{
    App, Event,
    archive::Format,
    folder::Location,
    info,
    model::{self, Entry},
    transfer::Msg,
    undo::Action,
    view::{self, Colors, Painted, Target, Ui},
};

/// Waits this long after the first change before listing again.
const REFRESH_DELAY: Duration = Duration::from_millis(150);
/// Tried in order when `$TERMINAL` is unset (the compositor's list).
const TERMINALS: &[&str] = &[
    "xdg-terminal-exec",
    "alacritty",
    "foot",
    "kitty",
    "gnome-terminal",
    "konsole",
    "weston-terminal",
];
const MENU_W: f32 = 200.0;

/// Something a button or a menu row does to the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Open,
    Rename,
    Trash,
    CopyPath,
    Terminal,
    /// Open With…: the app chooser sheet.
    OpenWith,
    /// A folder to the sidebar.
    AddBookmark,
    /// The sidebar bookmark's menu.
    MarkOpen,
    MarkTerminal,
    MarkRename,
    MarkRemove,
    /// A recent row: show it in its folder.
    OpenFolder,
    RemoveRecent,
    /// Show or hide the New File list.
    NewFileMenu,
    NewFile,
    /// New file from this template (an index into the menu's list).
    NewTemplate(usize),
    Copy,
    Cut,
    Paste,
    NewFolder,
    SelectAll,
    Refresh,
    ToggleHidden,
    /// The × on the progress strip.
    CancelTransfer,
    /// Show or hide the list of archive formats.
    CompressMenu,
    /// Compress the selection into an archive of this format.
    Compress(Format),
    /// Unpack the selected archive beside itself.
    ExtractHere,
}

const MENU: [(Act, &str); 9] = [
    (Act::Open, "Open"),
    (Act::OpenWith, "Open With…"),
    (Act::Rename, "Rename"),
    (Act::Copy, "Copy"),
    (Act::Cut, "Cut"),
    (Act::Paste, "Paste"),
    (Act::Trash, "Move to Trash"),
    (Act::CopyPath, "Copy path"),
    (Act::Terminal, "Terminal here"),
];
/// The menu over a row of the recent list.
const MENU_RECENT: [(Act, &str); 5] = [
    (Act::Open, "Open"),
    (Act::OpenWith, "Open With…"),
    (Act::OpenFolder, "Open Folder"),
    (Act::CopyPath, "Copy path"),
    (Act::RemoveRecent, "Remove from Recent"),
];
/// The menu over a single folder.
const MENU_DIR: [(Act, &str); 10] = [
    (Act::Open, "Open"),
    (Act::AddBookmark, "Add to Sidebar"),
    (Act::Rename, "Rename"),
    (Act::Copy, "Copy"),
    (Act::Cut, "Cut"),
    (Act::Paste, "Paste"),
    (Act::Trash, "Move to Trash"),
    (Act::CopyPath, "Copy path"),
    (Act::Terminal, "Terminal here"),
    (Act::OpenWith, "Open With…"),
];
/// The menu over a sidebar bookmark.
const MENU_MARK: [(Act, &str); 4] = [
    (Act::MarkOpen, "Open"),
    (Act::MarkTerminal, "Open in Terminal"),
    (Act::MarkRename, "Rename"),
    (Act::MarkRemove, "Remove from Sidebar"),
];
/// The menu over empty space in a folder.
const MENU_EMPTY: [(Act, &str); 5] = [
    (Act::NewFolder, "New Folder"),
    (Act::NewFileMenu, "New File  ›"),
    (Act::Paste, "Paste"),
    (Act::Terminal, "Open Terminal Here"),
    (Act::SelectAll, "Select All"),
];

impl Act {
    /// Rows that open a list or tick a box leave the menu up.
    fn keeps_menu(self) -> bool {
        matches!(self, Act::NewFileMenu | Act::CompressMenu)
    }
}

/// A name being edited in place.
pub struct Rename {
    /// The entry's current name.
    pub name: String,
    pub text: String,
    /// In characters.
    pub cursor: usize,
    /// Everything is selected: typing replaces it.
    pub fresh: bool,
    /// Renaming this sidebar bookmark, not a file.
    pub bookmark: Option<usize>,
}

/// The list beside the right-click menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    New,
    Compress,
}

/// The right-click menu, at a window position.
#[derive(Default)]
pub struct Menu {
    pub x: f32,
    pub y: f32,
    /// Over empty space, not a row.
    pub empty: bool,
    /// Over a row of the recent list: which.
    pub recent: Option<usize>,
    pub sub: Option<Sub>,
    /// Over a single folder: it can go to the sidebar.
    pub dir: bool,
    /// Over a bookmark of the sidebar: which.
    pub bookmark: Option<usize>,
    pub templates: Vec<(String, PathBuf)>,
    /// Over files that can be compressed (and bsdtar is there).
    pub compress: bool,
    /// Over a single archive that can be unpacked.
    pub extract: bool,
}

impl Menu {
    pub fn at(x: f32, y: f32) -> Self {
        Menu {
            x,
            y,
            ..Menu::default()
        }
    }

    fn rows(&self) -> Vec<(Act, &'static str)> {
        let base: &[(Act, &str)] = if self.bookmark.is_some() {
            &MENU_MARK
        } else if self.dir {
            &MENU_DIR
        } else if self.empty {
            &MENU_EMPTY
        } else if self.recent.is_some() {
            &MENU_RECENT
        } else {
            &MENU
        };
        let mut rows = base.to_vec();
        // The archive rows go before Move to Trash.
        if let Some(at) = rows.iter().position(|(a, _)| *a == Act::Trash) {
            if self.compress {
                rows.insert(at, (Act::CompressMenu, "Compress  ›"));
            }
            if self.extract {
                rows.insert(at, (Act::ExtractHere, "Extract Here"));
            }
        }
        rows
    }

    /// The submenu's rows: label, what it does, and the ticked one. At
    /// most `fit` apps show at once.
    fn sub_rows(&self, fit: usize) -> (Vec<String>, Vec<Act>, Option<usize>) {
        let (mut labels, mut acts, tick) = (Vec::new(), Vec::new(), None);
        match self.sub {
            Some(Sub::New) => {
                labels.push("Empty File".into());
                acts.push(Act::NewFile);
                for (i, (name, _)) in self.templates.iter().enumerate().take(fit.max(2) - 1) {
                    labels.push(name.clone());
                    acts.push(Act::NewTemplate(i));
                }
            }
            Some(Sub::Compress) => {
                for f in Format::ALL {
                    labels.push(f.label().into());
                    acts.push(Act::Compress(f));
                }
            }
            None => {}
        }
        (labels, acts, tick)
    }
}

/// The window state these operations keep.
#[derive(Default)]
pub struct Extra {
    pub info: info::InfoState,
    pub rename: Option<Rename>,
    pub menu: Option<Menu>,
    /// The ⋮ menu in the top bar.
    pub main_menu: Option<crate::mainmenu::MainMenu>,
    pub(crate) save_timer: Option<RegistrationToken>,
    /// A new folder to start renaming once the listing has it.
    pub pending_rename: Option<String>,
    pub xfer: crate::transfer::State,
    pub dnd: crate::dnd::State,
    /// A scrollbar thumb being dragged.
    pub bar_drag: Option<crate::scrollbar::Drag>,
    /// A bookmark being dragged to a new place in the list.
    pub bm_drag: Option<crate::marks::Drag>,
    /// Where the sidebar draws the insertion line of a drag.
    pub bm_insert: Option<usize>,
    /// The bookmarks file's folders that exist, before the standard places
    /// are taken out.
    pub bookmark_entries: Vec<crate::bookmarks::Entry>,
    /// The Open With sheet.
    pub sheet: Option<crate::openwith::Sheet>,
    pub refresh_seq: u64,
    refresh_timer: Option<RegistrationToken>,
    watch: Option<RegistrationToken>,
}

/// The rename entry, drawn in `rect`.
pub fn paint_rename(ui: &Ui, p: &mut Painter<'_, '_>, c: &Colors, rect: Rect) {
    let Some(r) = &ui.x.rename else {
        return;
    };
    p.entry(rect, &r.text, "", Some(r.cursor), false);
    if r.fresh && !r.text.is_empty() {
        let size = p.theme.font_size;
        let tw = p.measure(&r.text, size, Weight::Normal).min(rect.w - 20.0);
        p.canvas.rounded_rect(
            rect.x + 10.0,
            rect.y + rect.h / 2.0 - 9.0,
            tw,
            18.0,
            2.0,
            c.sel,
        );
    }
}

/// The right-click menu over everything; its rows are the last hits.
pub fn paint_menu(ui: &Ui, p: &mut Painter<'_, '_>, out: &mut Painted, w: f32, h: f32) {
    let Some(menu) = &ui.x.menu else {
        return;
    };
    let items = menu.rows();
    let labels: Vec<&str> = items.iter().map(|(_, l)| *l).collect();
    let mh = Painter::popover_height(labels.len());
    let x = menu.x.min(w - MENU_W - 8.0).max(8.0);
    let y = menu.y.min(h - mh - 8.0).max(8.0);
    let hover = |i: usize| (ui.hover == Some(Target::Act(items[i].0))) as u8 as f32;
    let rects = p.popover(x, y, MENU_W, &labels, None, hover, 1.0);
    let mut live = rects.len();
    let opener = menu.sub.map(|s| match s {
        Sub::New => Act::NewFileMenu,
        Sub::Compress => Act::CompressMenu,
    });
    let mut row_y = y;
    for (rect, (act, _)) in rects.into_iter().zip(items) {
        if Some(act) == opener {
            row_y = rect.y;
        }
        out.hits.push((rect, Target::Act(act)));
    }
    if menu.sub.is_some() {
        let fit = ((h - 28.0) / 36.0) as usize;
        let (labels, acts, tick) = menu.sub_rows(fit.max(3));
        let strs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let sw = (p.dropdown_width(&strs) + 16.0).clamp(190.0, 380.0);
        let sx = if x + MENU_W + 4.0 + sw <= w - 8.0 {
            x + MENU_W + 4.0
        } else {
            (x - sw - 4.0).max(8.0)
        };
        let sh = Painter::popover_height(strs.len());
        let sy = (row_y - 6.0).min(h - sh - 8.0).max(8.0);
        let hover = |i: usize| (ui.hover == Some(Target::Act(acts[i]))) as u8 as f32;
        let rects = p.popover(sx, sy, sw, &strs, tick, hover, 1.0);
        live += rects.len();
        for (rect, act) in rects.into_iter().zip(&acts) {
            out.hits.push((rect, Target::Act(*act)));
        }
    }
    out.menu_rows = live;
}

/// The first of `names` that is an executable on `$PATH`.
pub(crate) fn on_path(name: &str) -> bool {
    if name.contains('/') {
        return Path::new(name).is_file();
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|d| {
            fs::metadata(d.join(name)).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
    })
}

/// The terminal to use: `$TERMINAL`, else the first of the usual ones that
/// is installed.
pub(crate) fn terminal_argv() -> Option<Vec<String>> {
    let mut candidates: Vec<Vec<String>> = Vec::new();
    candidates.extend(configured_terminal());
    if let Ok(term) = std::env::var("TERMINAL") {
        candidates.push(term.split_whitespace().map(String::from).collect());
    }
    candidates.extend(TERMINALS.iter().map(|t| vec![t.to_string()]));
    candidates
        .into_iter()
        .find(|a| a.first().is_some_and(|c| on_path(c)))
}

/// The command of the terminal chosen in Settings (`apps.terminal`).
fn configured_terminal() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(cnde_config::default_path()?).ok()?;
    let id = cnde_config::Config::parse(&text).ok()?.apps.terminal;
    let env = cnde_apps::Env::from_env();
    let index = cnde_apps::Index::build(env);
    let app = index.apps.iter().find(|a| a.id == id)?;
    let argv: Vec<String> = cnde_apps::split_exec(&app.exec)
        .into_iter()
        .filter(|w| !w.starts_with('%'))
        .collect();
    (!argv.is_empty()).then_some(argv)
}

/// `command` run inside the terminal `term`.
pub(crate) fn in_terminal(mut term: Vec<String>, command: Vec<String>) -> Vec<String> {
    match term.first().map(|t| t.rsplit('/').next().unwrap_or(t)) {
        Some("xdg-terminal-exec") => {}
        Some("gnome-terminal") => term.push("--".into()),
        _ => term.push("-e".into()),
    }
    term.extend(command);
    term
}

/// Run `cmd` detached, reaping it on a thread.
pub(crate) fn run_detached(mut cmd: Command) -> std::io::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

impl App {
    /// Items per row and the height of a row, for the current view.
    pub(crate) fn cells(&self) -> (usize, f32) {
        view::cells(&self.ui, view::list_w(&self.ui, self.size.0 as f32))
    }

    /// The selected entries (cloned), in listing order.
    fn selected_entries(&self) -> Vec<Entry> {
        let f = &self.ui.folder;
        f.visible
            .iter()
            .map(|&i| &f.entries[i])
            .filter(|e| f.selected.contains(&e.name))
            .cloned()
            .collect()
    }

    /// The selected entries' paths, in listing order.
    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected_entries()
            .into_iter()
            .map(|e| e.path)
            .collect()
    }

    /// What an action applies to: the selection, else the open folder.
    fn target_paths(&self) -> Vec<PathBuf> {
        let sel = self.selected_entries();
        if !sel.is_empty() {
            return sel.into_iter().map(|e| e.path).collect();
        }
        match &self.ui.location {
            Location::Dir(p) => vec![p.clone()],
            _ => Vec::new(),
        }
    }

    pub(crate) fn act(&mut self, act: Act) {
        let menu = self.ui.x.menu.take();
        self.act_in(act, menu);
    }

    /// Do `act`; `menu` is the right-click menu it came from, if any.
    fn act_in(&mut self, act: Act, menu: Option<Menu>) {
        debug!(?act, "menu action");
        let recent = menu.as_ref().and_then(|m| m.recent);
        match act {
            Act::Open => {
                if let Some(i) = recent {
                    self.open_recent(i);
                } else {
                    let f = &self.ui.folder;
                    let pos = f
                        .cursor
                        .filter(|&c| f.is_selected(c))
                        .or_else(|| (0..f.visible.len()).find(|&p| f.is_selected(p)));
                    if let Some(pos) = pos {
                        self.open_row(pos);
                    }
                }
            }
            Act::Rename => self.start_rename(),
            Act::Trash => self.trash_selection(),
            Act::CopyPath => match recent.and_then(|i| self.recent_path(i)) {
                Some(path) => {
                    let text = path.display().to_string();
                    self.set_clipboard(crate::clipboard::Payload::Text(text));
                }
                None => self.copy_path(),
            },
            Act::Terminal => self.terminal_here(),
            Act::Copy => self.copy_files(false),
            Act::Cut => self.copy_files(true),
            Act::Paste => self.paste(),
            Act::NewFolder => self.new_folder(),
            Act::NewFile => self.new_file("Untitled".into(), None),
            Act::NewTemplate(i) => {
                if let Some((name, path)) = menu.as_ref().and_then(|m| m.templates.get(i)) {
                    self.new_file(name.clone(), Some(path.clone()));
                }
            }
            Act::OpenFolder => {
                if let Some(i) = recent {
                    self.open_recent_folder(i);
                }
            }
            Act::RemoveRecent => {
                if let Some(i) = recent {
                    self.remove_recent(i);
                }
            }
            Act::OpenWith => self.open_with_sheet(recent),
            Act::AddBookmark => {
                let dirs = self.selected_dirs();
                self.add_bookmarks(dirs, None);
            }
            Act::MarkOpen => {
                if let Some(i) = menu.as_ref().and_then(|m| m.bookmark) {
                    self.open_bookmark(i);
                }
            }
            Act::MarkTerminal => {
                if let Some(i) = menu.as_ref().and_then(|m| m.bookmark) {
                    self.bookmark_terminal(i);
                }
            }
            Act::MarkRename => {
                if let Some(i) = menu.as_ref().and_then(|m| m.bookmark) {
                    self.start_bookmark_rename(i);
                }
            }
            Act::MarkRemove => {
                if let Some(i) = menu.as_ref().and_then(|m| m.bookmark) {
                    self.remove_bookmark(i);
                }
            }
            Act::NewFileMenu | Act::CompressMenu => {}
            Act::Compress(format) => self.compress_selection(format),
            Act::ExtractHere => self.extract_selection(),
            Act::SelectAll => self.ui.folder.select_all(),
            Act::Refresh => self.reload_folder(),
            Act::ToggleHidden => self.toggle_hidden(),
            Act::CancelTransfer => self.cancel_transfer(),
        }
        self.redraw();
    }

    /// A press while the menu is open: pick a row, or just close it.
    pub(crate) fn menu_press(&mut self, position: (f64, f64)) -> bool {
        if self.ui.x.menu.is_none() {
            return self.main_menu_press(position);
        }
        let hit = self.painted.hit(position);
        if let Some(Target::Act(act)) = hit
            && act.keeps_menu()
        {
            self.menu_toggle(act);
            self.redraw();
            return true;
        }
        let menu = self.ui.x.menu.take();
        if let Some(Target::Act(act)) = hit {
            self.act_in(act, menu);
        }
        self.redraw();
        true
    }

    /// A row that opens a list beside the menu, or ticks its box.
    fn menu_toggle(&mut self, act: Act) {
        let Some(menu) = self.ui.x.menu.as_mut() else {
            return;
        };
        match act {
            Act::CompressMenu if menu.sub == Some(Sub::Compress) => menu.sub = None,
            Act::CompressMenu => menu.sub = Some(Sub::Compress),
            Act::NewFileMenu if menu.sub == Some(Sub::New) => menu.sub = None,
            Act::NewFileMenu => {
                let env = crate::apps::Env::from_env();
                menu.templates = crate::newfile::templates_dir(&env.config_home, &self.ui.home)
                    .map(|d| crate::newfile::list_templates(&d))
                    .unwrap_or_default();
                menu.sub = Some(Sub::New);
            }
            _ => {}
        }
    }

    /// What Open With applies to: the recent row, else the selection.
    pub(crate) fn menu_files(&self, recent: Option<usize>) -> Vec<(PathBuf, bool)> {
        match recent {
            Some(i) => self
                .ui
                .pages
                .recent
                .as_ref()
                .and_then(|r| r.get(i))
                .map(|r| vec![(r.path.clone(), r.is_dir)])
                .unwrap_or_default(),
            None => self
                .selected_entries()
                .into_iter()
                .map(|e| (e.path, e.is_dir))
                .collect(),
        }
    }

    // ---- recent rows ----

    fn recent_path(&self, i: usize) -> Option<PathBuf> {
        self.ui
            .pages
            .recent
            .as_ref()
            .and_then(|r| r.get(i))
            .map(|r| r.path.clone())
    }

    pub(crate) fn open_recent(&mut self, i: usize) {
        let row = self.ui.pages.recent.as_ref().and_then(|r| r.get(i));
        if let Some((path, is_dir)) = row.map(|r| (r.path.clone(), r.is_dir)) {
            self.open_path(path, is_dir);
        }
    }

    /// Show the recent file in its folder, selected.
    pub(crate) fn open_recent_folder(&mut self, i: usize) {
        let row = self.ui.pages.recent.as_ref().and_then(|r| r.get(i));
        if let Some((dir, name)) =
            row.and_then(|r| Some((r.path.parent()?.to_path_buf(), r.name.clone())))
        {
            self.reveal = Some(name);
            self.go(Location::Dir(dir));
        }
    }

    /// Drop a row from the list and from recently-used.xbel.
    fn remove_recent(&mut self, i: usize) {
        let Some(rows) = self.ui.pages.recent.as_mut().filter(|r| i < r.len()) else {
            return;
        };
        let path = rows.remove(i).path;
        self.ui.pages.version += 1;
        let tx = self.events.clone();
        std::thread::spawn(move || {
            if let Err(err) = model::forget(&path) {
                let _ = tx.send(Event::Error(format!(
                    "Can't update the recent files: {err}"
                )));
            }
        });
    }

    /// Right-click: select the row under the pointer and show the menu.
    pub(crate) fn context_click(&mut self, position: (f64, f64)) {
        if self.ui.picker.is_some() {
            return;
        }
        if self.ui.x.sheet.is_some() {
            return;
        }
        self.ui.x.main_menu = None;
        self.ui.x.rename = None;
        if self.bookmark_context(position) {
            return;
        }
        let (x, y) = (position.0 as f32, position.1 as f32);
        if !matches!(self.ui.location, Location::Dir(_)) {
            // The Overview and Recent pages: a menu on a recent row.
            let was_open = self.ui.x.menu.take().is_some();
            if let (false, Some(Target::RecentFile(i) | Target::RecentFolder(i))) =
                (was_open, self.painted.hit(position))
            {
                self.ui.x.menu = Some(Menu {
                    recent: Some(i),
                    ..Menu::at(x, y)
                });
            }
            self.redraw();
            return;
        }
        let Some(pos) = self.row_at(position) else {
            // Empty space in a folder: make something, or paste into it.
            let in_list = self.painted.list.contains(position)
                && self.painted.hit(position).is_none()
                && matches!(self.ui.location, Location::Dir(_));
            if in_list {
                self.ui.folder.clear_selection();
            }
            self.ui.x.menu = in_list.then(|| Menu {
                empty: true,
                ..Menu::at(x, y)
            });
            self.redraw();
            return;
        };
        if !self.ui.folder.is_selected(pos) {
            self.ui.folder.select_only(pos);
        }
        self.ui.folder.cursor = Some(pos);
        let dir = self.selected_dirs().len() == 1 && self.ui.folder.selected.len() == 1;
        let have = crate::archive::available();
        let extract = have
            && matches!(self.selected_entries().as_slice(),
                [e] if !e.is_dir && crate::archive::is_archive(&e.name));
        self.ui.x.menu = Some(Menu {
            dir,
            compress: have,
            extract,
            ..Menu::at(x, y)
        });
        self.redraw();
    }

    // ---- trash, rename, new folder ----

    pub(crate) fn trash_selection(&mut self) {
        let paths: Vec<PathBuf> = self
            .selected_entries()
            .into_iter()
            .map(|e| e.path)
            .collect();
        if paths.is_empty() {
            return;
        }
        let tx = self.events.clone();
        std::thread::spawn(move || {
            for path in paths {
                if let Err(err) = model::trash(&path) {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let _ = tx.send(Event::Error(format!(
                        "Can't move “{name}” to the Trash: {err}"
                    )));
                } else {
                    let _ = tx.send(Event::Xfer(Msg::Record(Action::Trashed(path))));
                }
            }
            let _ = tx.send(Event::Refresh);
        });
    }

    /// Rename the cursor row (or the only selected one) in place.
    pub(crate) fn start_rename(&mut self) {
        let f = &self.ui.folder;
        let pos = f
            .cursor
            .filter(|&c| f.is_selected(c))
            .or_else(|| (0..f.visible.len()).find(|&p| f.is_selected(p)));
        let Some(pos) = pos else {
            return;
        };
        let Some(name) = f.entry(pos).map(|e| e.name.clone()) else {
            return;
        };
        self.ui.x.rename = Some(Rename {
            cursor: name.chars().count(),
            text: name.clone(),
            name,
            fresh: true,
            bookmark: None,
        });
        self.ensure_visible(pos);
    }

    fn commit_rename(&mut self) {
        let Some(r) = self.ui.x.rename.take() else {
            return;
        };
        if let Some(i) = r.bookmark {
            self.commit_bookmark_rename(i, r.text);
            return;
        }
        let new = r.text;
        if new.is_empty() || new == r.name {
            return;
        }
        if new.contains('/') || new.contains('\0') || new == "." || new == ".." {
            self.show_error("A name can't contain “/”".into());
            return;
        }
        let Location::Dir(dir) = self.ui.location.clone() else {
            return;
        };
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let (from, to) = (dir.join(&r.name), dir.join(&new));
            if fs::symlink_metadata(&to).is_ok() {
                let _ = tx.send(Event::Error(format!("“{new}” already exists here")));
            } else if let Err(err) = fs::rename(&from, &to) {
                let _ = tx.send(Event::Error(format!("Can't rename “{}”: {err}", r.name)));
            } else {
                let _ = tx.send(Event::Xfer(Msg::Record(Action::Rename {
                    dir,
                    from: r.name.clone(),
                    to: new.clone(),
                })));
                let _ = tx.send(Event::Renamed(new));
            }
        });
    }

    pub(crate) fn new_folder(&mut self) {
        let Location::Dir(dir) = self.ui.location.clone() else {
            return;
        };
        let tx = self.events.clone();
        std::thread::spawn(move || {
            for n in 1..1000 {
                let name = if n == 1 {
                    "New Folder".to_string()
                } else {
                    format!("New Folder {n}")
                };
                match fs::create_dir(dir.join(&name)) {
                    Ok(()) => {
                        let _ =
                            tx.send(Event::Xfer(Msg::Record(Action::NewFolder(dir.join(&name)))));
                        let _ = tx.send(Event::Made(name));
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => {
                        let _ = tx.send(Event::Error(format!("Can't create a folder: {e}")));
                        return;
                    }
                }
            }
        });
    }

    /// Make a file here (empty, or a copy of `template`) and rename it.
    pub(crate) fn new_file(&mut self, base: String, template: Option<PathBuf>) {
        let Location::Dir(dir) = self.ui.location.clone() else {
            return;
        };
        let tx = self.events.clone();
        std::thread::spawn(move || {
            match crate::newfile::create(&dir, &base, template.as_deref()) {
                Ok(name) => {
                    let _ = tx.send(Event::Xfer(Msg::Record(Action::NewFile(dir.join(&name)))));
                    let _ = tx.send(Event::Made(name));
                }
                Err(e) => {
                    let _ = tx.send(Event::Error(format!("Can't create a file: {e}")));
                }
            }
        });
    }

    /// Keys while a rename or the menu is up. True if it took the key.
    pub(crate) fn pre_key(&mut self, event: &KeyEvent) -> bool {
        if self.sheet_key(event) {
            return true;
        }
        if self.ui.x.menu.is_some() || self.ui.x.main_menu.is_some() {
            self.ui.x.menu = None;
            self.ui.x.main_menu = None;
            self.redraw();
            return event.keysym == Keysym::Escape;
        }
        let ctrl = self.modifiers.ctrl;
        let Some(edit) = &mut self.ui.x.rename else {
            return false;
        };
        let len = edit.text.chars().count();
        let byte = |text: &str, chars: usize| {
            text.char_indices()
                .nth(chars)
                .map_or(text.len(), |(i, _)| i)
        };
        match event.keysym {
            Keysym::Return | Keysym::KP_Enter => self.commit_rename(),
            Keysym::Escape => edit_cancel(self),
            Keysym::BackSpace if edit.fresh => {
                edit.text.clear();
                edit.cursor = 0;
                edit.fresh = false;
            }
            Keysym::BackSpace if edit.cursor > 0 => {
                let at = byte(&edit.text, edit.cursor - 1);
                edit.text.remove(at);
                edit.cursor -= 1;
            }
            Keysym::Delete if edit.fresh => {
                edit.text.clear();
                edit.cursor = 0;
                edit.fresh = false;
            }
            Keysym::Delete if edit.cursor < len => {
                let at = byte(&edit.text, edit.cursor);
                edit.text.remove(at);
            }
            Keysym::Left => {
                edit.cursor = if edit.fresh {
                    0
                } else {
                    edit.cursor.saturating_sub(1)
                };
                edit.fresh = false;
            }
            Keysym::Right => {
                edit.cursor = if edit.fresh {
                    len
                } else {
                    (edit.cursor + 1).min(len)
                };
                edit.fresh = false;
            }
            Keysym::Home => {
                edit.cursor = 0;
                edit.fresh = false;
            }
            Keysym::End => {
                edit.cursor = len;
                edit.fresh = false;
            }
            _ => {
                if let Some(text) = event
                    .utf8
                    .clone()
                    .filter(|t| !ctrl && !t.chars().any(char::is_control))
                {
                    if edit.fresh {
                        edit.text.clear();
                        edit.cursor = 0;
                        edit.fresh = false;
                    }
                    let at = byte(&edit.text, edit.cursor);
                    edit.text.insert_str(at, &text);
                    edit.cursor += text.chars().count();
                }
            }
        }
        self.redraw();
        true
    }

    // ---- clipboard and terminal ----

    pub(crate) fn copy_path(&mut self) {
        let text = self
            .target_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }
        self.set_clipboard(crate::clipboard::Payload::Text(text));
    }

    pub(crate) fn terminal_here(&mut self) {
        let sel = self.selected_entries();
        let dir = match (sel.as_slice(), &self.ui.location) {
            ([e], _) if e.is_dir => e.path.clone(),
            ([e], _) => e.path.parent().map(Path::to_path_buf).unwrap_or_default(),
            (_, Location::Dir(p)) => p.clone(),
            _ => return,
        };
        self.terminal_in(dir);
    }

    /// A terminal in `dir`.
    pub(crate) fn terminal_in(&mut self, dir: PathBuf) {
        let tx = self.events.clone();
        std::thread::spawn(move || {
            let Some(argv) = terminal_argv() else {
                let _ = tx.send(Event::Error("No terminal found (set $TERMINAL)".into()));
                return;
            };
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(&dir);
            if let Err(err) = run_detached(cmd) {
                let _ = tx.send(Event::Error(format!("Can't run {}: {err}", argv[0])));
            }
        });
    }

    // ---- the info panel's scan ----

    /// Start a scan when what the panel describes has changed.
    pub(crate) fn sync_info(&mut self) {
        if !self.ui.info_panel {
            return;
        }
        let Some(subject) = info::subject(&self.ui) else {
            return;
        };
        let key = info::subject_key(&self.ui);
        if key == self.ui.x.info.key {
            return;
        }
        let paths: Vec<PathBuf> = match subject {
            info::Subject::Folder(p) => vec![p.to_path_buf()],
            info::Subject::One(e) => vec![e.path.clone()],
            info::Subject::Many(_) => self
                .selected_entries()
                .into_iter()
                .map(|e| e.path)
                .collect(),
        };
        let state = &mut self.ui.x.info;
        if let Some(old) = state.cancel.take() {
            old.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        state.cancel = Some(cancel.clone());
        state.generation += 1;
        state.key = key;
        state.scan = None;
        let (generation, tx) = (state.generation, self.events.clone());
        std::thread::spawn(move || {
            let scan = info::scan(&paths, &cancel);
            if !cancel.load(Ordering::Relaxed) {
                let _ = tx.send(Event::Info(generation, scan));
            }
        });
    }

    pub(crate) fn info_done(&mut self, generation: u64, scan: info::Scan) {
        if generation == self.ui.x.info.generation {
            self.ui.x.info.scan = Some(scan);
            self.redraw();
        }
    }

    // ---- following the folder on disk ----

    /// Watch `path` for changes; whatever was watched stops.
    pub(crate) fn watch_dir(&mut self, path: &Path) {
        self.unwatch();
        let setup = || -> rustix::io::Result<std::os::fd::OwnedFd> {
            let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
            inotify::add_watch(
                &fd,
                path,
                WatchFlags::CREATE
                    | WatchFlags::DELETE
                    | WatchFlags::MOVED_FROM
                    | WatchFlags::MOVED_TO
                    | WatchFlags::CLOSE_WRITE
                    | WatchFlags::ATTRIB,
            )?;
            Ok(fd)
        };
        let Ok(fd) = setup() else {
            debug!(path = %path.display(), "can't watch the folder");
            return;
        };
        self.ui.x.watch = self
            .loop_handle
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                |_, fd, app| {
                    let mut buf = [0u8; 4096];
                    loop {
                        match rustix::io::read(&*fd, &mut buf) {
                            Ok(0) | Err(Errno::AGAIN) => break,
                            Ok(_) | Err(Errno::INTR) => continue,
                            Err(_) => break,
                        }
                    }
                    app.schedule_refresh();
                    Ok(PostAction::Continue)
                },
            )
            .ok();
    }

    pub(crate) fn unwatch(&mut self) {
        if let Some(token) = self.ui.x.watch.take() {
            self.loop_handle.remove(token);
        }
        if let Some(token) = self.ui.x.refresh_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// List again soon; a burst of changes makes one listing.
    fn schedule_refresh(&mut self) {
        if self.ui.x.refresh_timer.is_some() {
            return;
        }
        self.ui.x.refresh_timer = self
            .loop_handle
            .insert_source(Timer::from_duration(REFRESH_DELAY), |_, _, app| {
                app.ui.x.refresh_timer = None;
                app.refresh_dir();
                TimeoutAction::Drop
            })
            .ok();
    }

    /// List the open folder again in the background.
    pub(crate) fn refresh_dir(&mut self) {
        let Location::Dir(path) = self.ui.location.clone() else {
            return;
        };
        if self.ui.folder.loading {
            return;
        }
        self.ui.x.refresh_seq += 1;
        let (generation, seq, tx) = (self.generation, self.ui.x.refresh_seq, self.events.clone());
        std::thread::spawn(move || {
            let mut all = Vec::new();
            let never = AtomicBool::new(false);
            if model::list_dir(&path, &never, |b| all.extend(b)).is_ok() {
                let _ = tx.send(Event::Refreshed(generation, seq, all));
            }
        });
    }

    /// A fresh listing arrived: swap it in, keeping selection and scroll.
    pub(crate) fn apply_refresh(&mut self, entries: Vec<Entry>) {
        let (cols, row_h) = self.cells();
        let top = (self.scroll_target / row_h).floor().max(0.0) as usize * cols;
        let top_name = self.ui.folder.entry(top).map(|e| e.name.clone());
        let within = self.scroll_target - (self.scroll_target / row_h).floor() * row_h;
        self.ui.folder.replace(entries);
        if let Some(name) = top_name {
            let f = &self.ui.folder;
            if let Some(pos) = f.visible.iter().position(|&i| f.entries[i].name == name) {
                self.scroll_target = (pos / cols) as f32 * row_h + within;
                self.ui.scroll = self.scroll_target;
            }
        }
        self.ui.x.info.key = 0;
        self.reveal_pending();
        self.apply_pending_select();
        if let Some(name) = self.ui.x.pending_rename.clone() {
            let f = &self.ui.folder;
            if let Some(pos) = f.visible.iter().position(|&i| f.entries[i].name == name) {
                self.ui.x.pending_rename = None;
                self.ui.folder.select_only(pos);
                self.start_rename();
            }
        }
        self.redraw();
    }
}

fn edit_cancel(app: &mut App) {
    app.ui.x.rename = None;
}
