//! Picker mode (`cnde-files --pick ...`, run by `cnde-files-portal`): the
//! same window, listing and sidebar, with a bottom row to choose with. The
//! answer goes to stdout (see [`crate::request::Answer`]); exit status 0 is
//! "chosen", 1 is "cancelled".

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    rc::Rc,
};

use cnde_ui::{Color, Painter, Rect, Weight};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};

use crate::{
    App,
    folder::{Location, Shown},
    model::Entry,
    request::{Answer, Kind, Request},
    search::{Keyed, Search},
    view::{Colors, Painted, SIDE_W, Target, Ui},
};

/// Height of the bottom row.
pub const BAR_H: f32 = 64.0;
const FIELD_H: f32 = 36.0;

/// Something in the picker's own controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// The file name field (Save).
    Name,
    /// The filter dropdown, and one of its choices.
    Filter,
    Choice(usize),
    Cancel,
    Accept,
    /// The overwrite question: replace, or keep looking.
    Replace,
    Keep,
}

pub struct Picker {
    pub req: Request,
    /// The filter in use.
    pub filter: usize,
    /// The Save name; `focused` says whether it has the keyboard.
    pub name: Search,
    /// The filter choices are showing.
    pub open: bool,
    /// Asking whether to overwrite this file.
    pub confirm: Option<PathBuf>,
    /// Set when the user chose; the window then closes.
    pub done: Option<Answer>,
}

impl Picker {
    pub fn new(req: Request) -> Self {
        let mut name = Search::default();
        name.query = req.name.clone();
        name.cursor = req.name.chars().count();
        name.focused = req.kind == Kind::Save;
        Self {
            filter: req.current_filter,
            req,
            name,
            open: false,
            confirm: None,
            done: None,
        }
    }

    pub fn accept_label(&self) -> &str {
        if !self.req.accept.is_empty() {
            return &self.req.accept;
        }
        match self.req.kind {
            Kind::Open => "Open",
            Kind::Save => "Save",
            Kind::Folder => "Select",
        }
    }

    /// The window title.
    pub fn title(&self) -> String {
        if !self.req.title.is_empty() {
            return self.req.title.clone();
        }
        match self.req.kind {
            Kind::Open => "Open File",
            Kind::Save => "Save File",
            Kind::Folder => "Select Folder",
        }
        .into()
    }

    /// Which files the list shows: the current filter on files (folders
    /// always show), or no files at all when choosing a folder.
    pub fn shown(&self) -> Shown {
        if self.req.kind == Kind::Folder {
            return Rc::new(|_| false);
        }
        match self.req.filters.get(self.filter).cloned() {
            Some(filter) => Rc::new(move |e| filter.matches(&e.name)),
            None => Rc::new(|_| true),
        }
    }
}

/// What the accept button would do now.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to accept yet; the button is dimmed.
    Nothing,
    /// Go into this folder.
    Enter(PathBuf),
    /// These are the answer.
    Choose(Vec<PathBuf>),
    /// Save here, but the file exists: ask first.
    Overwrite(PathBuf),
}

/// What accept does with `selected` names (or the typed `name`) in `dir`.
pub fn plan(
    kind: Kind,
    name: &str,
    dir: &Path,
    entries: &[Entry],
    visible: &[usize],
    selected: &HashSet<String>,
) -> Plan {
    let chosen: Vec<&Entry> = visible
        .iter()
        .map(|&i| &entries[i])
        .filter(|e| selected.contains(&e.name))
        .collect();
    match kind {
        Kind::Open => {
            let files: Vec<PathBuf> = chosen
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.path.clone())
                .collect();
            if !files.is_empty() {
                return Plan::Choose(files);
            }
            match chosen.first() {
                Some(folder) => Plan::Enter(folder.path.clone()),
                None => Plan::Nothing,
            }
        }
        Kind::Folder => {
            let dirs: Vec<PathBuf> = chosen
                .iter()
                .filter(|e| e.is_dir)
                .map(|e| e.path.clone())
                .collect();
            Plan::Choose(if dirs.is_empty() {
                vec![dir.to_path_buf()]
            } else {
                dirs
            })
        }
        Kind::Save => {
            let name = name.trim();
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                return Plan::Nothing;
            }
            let path = dir.join(name);
            match entries.iter().find(|e| e.name == name) {
                Some(e) if e.is_dir => Plan::Enter(path),
                Some(_) => Plan::Overwrite(path),
                None => Plan::Choose(vec![path]),
            }
        }
    }
}

/// [`plan`] for what is on screen.
pub fn plan_of(ui: &Ui) -> Plan {
    let (Some(picker), Location::Dir(dir)) = (&ui.picker, &ui.location) else {
        return Plan::Nothing;
    };
    plan(
        picker.req.kind,
        &picker.name.query,
        dir,
        &ui.folder.entries,
        &ui.folder.visible,
        &ui.folder.selected,
    )
}

fn pressed(ui: &Ui, hit: Hit) -> f32 {
    (ui.hover == Some(Target::Pick(hit))) as u8 as f32
}

/// The bottom row. Drawn over the end of the list.
pub fn paint_bar(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    (w, h): (f32, f32),
    radius: f32,
) {
    let Some(picker) = &ui.picker else {
        return;
    };
    let t = p.theme;
    let by = h - BAR_H;
    p.canvas.rounded_rect_clipped(
        SIDE_W - radius,
        by - radius - 4.0,
        w - SIDE_W + radius,
        BAR_H + radius + 4.0,
        radius,
        t.window_background,
        (SIDE_W, by, w - SIDE_W, BAR_H),
    );
    p.canvas
        .rounded_rect(SIDE_W, by, w - SIDE_W, 1.0, 0.0, c.hair);
    let cy = by + BAR_H / 2.0;
    let size = t.font_size;
    let button = |out: &mut Painted, x: f32, bw: f32, hit: Hit| {
        let rect = Rect::new(x, cy - FIELD_H / 2.0, bw, FIELD_H);
        out.hits.push((rect, Target::Pick(hit)));
        rect
    };

    // From the right edge in: accept, cancel, filter.
    let plan = plan_of(ui);
    let label = picker.accept_label();
    let aw = (p.measure(label, size, Weight::Bold) + 40.0).max(96.0);
    let mut x = w - 16.0 - aw;
    let rect = button(out, x, aw, Hit::Accept);
    let enabled = plan != Plan::Nothing;
    let fill = if enabled {
        t.accent
            .mix(Color::rgb(255, 255, 255), 0.12 * pressed(ui, Hit::Accept))
    } else {
        t.accent.with_alpha(0.3)
    };
    p.canvas
        .rounded_rect(rect.x, rect.y, rect.w, rect.h, t.button_radius, fill);
    let tw = p.measure(label, size, Weight::Bold).min(aw - 16.0);
    p.label_v(
        label,
        rect.x + (aw - tw) / 2.0,
        cy,
        aw - 16.0,
        size,
        Weight::Bold,
        t.on_accent.with_alpha(if enabled { 1.0 } else { 0.7 }),
    );
    if !enabled {
        out.hits.pop();
    }

    let cw = (p.measure("Cancel", size, Weight::Normal) + 40.0).max(88.0);
    x -= 10.0 + cw;
    let rect = button(out, x, cw, Hit::Cancel);
    p.button(rect, pressed(ui, Hit::Cancel));
    let tw = p.measure("Cancel", size, Weight::Normal);
    p.label_v(
        "Cancel",
        rect.x + (cw - tw) / 2.0,
        cy,
        cw,
        size,
        Weight::Normal,
        c.ink,
    );

    let labels: Vec<&str> = picker
        .req
        .filters
        .iter()
        .map(|f| f.label.as_str())
        .collect();
    if !labels.is_empty() {
        let fw = p.dropdown_width(&labels).clamp(120.0, 260.0);
        x -= 16.0 + fw;
        let rect = button(out, x, fw, Hit::Filter);
        let current = labels.get(picker.filter).copied().unwrap_or("");
        p.dropdown(rect, current, pressed(ui, Hit::Filter));
    }

    // The left: the name to save as, or what is chosen.
    let left = SIDE_W + 22.0;
    let room = (x - 16.0 - left).max(0.0);
    if picker.req.kind == Kind::Save {
        let field = Rect::new(left, cy - FIELD_H / 2.0, room, FIELD_H);
        let cursor = picker.name.focused.then_some(picker.name.cursor);
        p.entry(field, &picker.name.query, "File name", cursor, false);
        out.hits.push((field, Target::Pick(Hit::Name)));
    } else {
        let text = summary(ui);
        p.label_v(&text, left, cy, room, size, Weight::Normal, c.dim);
    }
}

/// "3 selected" and the like, for the bar's left.
fn summary(ui: &Ui) -> String {
    let n = ui.folder.selected.len();
    match (&ui.picker, n) {
        (Some(p), 0) if p.req.kind == Kind::Folder => "Choose a folder".into(),
        (_, 0) => String::new(),
        (_, 1) => ui
            .folder
            .selected
            .iter()
            .next()
            .cloned()
            .unwrap_or_default(),
        (_, n) => format!("{n} selected"),
    }
}

/// The filter choices and the overwrite question, over everything else.
pub fn paint_overlay(
    ui: &Ui,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    (w, h): (f32, f32),
    radius: f32,
) {
    let Some(picker) = &ui.picker else {
        return;
    };
    let t = p.theme;
    let size = t.font_size;
    if picker.open {
        let Some(&(anchor, _)) = out
            .hits
            .iter()
            .rev()
            .find(|(_, t)| *t == Target::Pick(Hit::Filter))
        else {
            return;
        };
        let labels: Vec<&str> = picker
            .req
            .filters
            .iter()
            .map(|f| f.label.as_str())
            .collect();
        let ph = Painter::popover_height(labels.len());
        let rects = p.popover(
            anchor.x,
            anchor.y - ph - 6.0,
            anchor.w,
            &labels,
            Some(picker.filter),
            |i| (ui.hover == Some(Target::Pick(Hit::Choice(i)))) as u8 as f32,
            1.0,
        );
        let first = out.hits.len();
        for (i, rect) in rects.into_iter().enumerate() {
            out.hits.push((rect, Target::Pick(Hit::Choice(i))));
        }
        out.menu_rows = out.hits.len() - first;
    }
    if let Some(path) = &picker.confirm {
        p.canvas
            .rounded_rect(0.0, 0.0, w, h, radius, Color::rgba(0, 0, 0, 0.35));
        let (cw, ch) = (380.0f32.min(w - 32.0), 156.0);
        let (cx, cy) = ((w - cw) / 2.0, (h - ch) / 2.0);
        p.canvas
            .rounded_rect(cx, cy, cw, ch, t.popover_radius, t.popover_background);
        p.canvas
            .stroke_rounded_rect(cx, cy, cw, ch, t.popover_radius, 1.0, t.card_separator);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        p.label_v(
            &format!("Replace “{name}”?"),
            cx + 22.0,
            cy + 34.0,
            cw - 44.0,
            size + 3.0,
            Weight::Bold,
            c.ink,
        );
        p.label_v(
            "It already exists here and would be overwritten.",
            cx + 22.0,
            cy + 62.0,
            cw - 44.0,
            size - 1.0,
            Weight::Normal,
            c.dim,
        );
        let first = out.hits.len();
        let (bw, by) = (96.0, cy + ch - 20.0 - FIELD_H);
        let keep = Rect::new(cx + cw - 22.0 - bw * 2.0 - 10.0, by, bw, FIELD_H);
        let replace = Rect::new(cx + cw - 22.0 - bw, by, bw, FIELD_H);
        p.button(keep, pressed(ui, Hit::Keep));
        let tw = p.measure("Cancel", size, Weight::Normal);
        p.label_v(
            "Cancel",
            keep.x + (bw - tw) / 2.0,
            by + FIELD_H / 2.0,
            bw,
            size,
            Weight::Normal,
            c.ink,
        );
        let fill = t
            .accent
            .mix(Color::rgb(255, 255, 255), 0.12 * pressed(ui, Hit::Replace));
        p.canvas
            .rounded_rect(replace.x, replace.y, bw, FIELD_H, t.button_radius, fill);
        let tw = p.measure("Replace", size, Weight::Bold);
        p.label_v(
            "Replace",
            replace.x + (bw - tw) / 2.0,
            by + FIELD_H / 2.0,
            bw,
            size,
            Weight::Bold,
            t.on_accent,
        );
        out.hits.push((keep, Target::Pick(Hit::Keep)));
        out.hits.push((replace, Target::Pick(Hit::Replace)));
        out.menu_rows = out.hits.len() - first;
    }
}

impl App {
    /// Apply the current filter to the listing.
    pub(crate) fn picker_refilter(&mut self) {
        let Some(picker) = &self.ui.picker else {
            return;
        };
        let shown = picker.shown();
        self.ui.folder.set_shown(Some(shown));
        self.ui.scroll = 0.0;
        self.scroll_target = 0.0;
    }

    /// A press while the filter choices or the question are up: true if
    /// it was used (the picker takes every press then).
    pub(crate) fn picker_press(&mut self, position: (f64, f64)) -> bool {
        let Some(picker) = &self.ui.picker else {
            return false;
        };
        if !picker.open && picker.confirm.is_none() {
            return false;
        }
        if let Some(Target::Pick(hit)) = self.painted.hit(position) {
            self.picker_hit(hit);
        } else if picker.confirm.is_none() {
            self.picker_close_popups();
        }
        self.redraw();
        true
    }

    fn picker_close_popups(&mut self) {
        if let Some(p) = &mut self.ui.picker {
            p.open = false;
            p.confirm = None;
        }
    }

    pub(crate) fn picker_hit(&mut self, hit: Hit) {
        match hit {
            Hit::Name => {
                if let Some(p) = &mut self.ui.picker {
                    p.name.focused = true;
                    p.name.cursor = p.name.query.chars().count();
                }
            }
            Hit::Filter => {
                if let Some(p) = &mut self.ui.picker {
                    p.open = !p.open;
                }
            }
            Hit::Choice(i) => {
                if let Some(p) = &mut self.ui.picker {
                    p.open = false;
                    p.filter = i;
                }
                self.picker_refilter();
            }
            Hit::Cancel => self.exit.stop(),
            Hit::Accept => self.picker_accept(),
            Hit::Keep => self.picker_close_popups(),
            Hit::Replace => {
                if let Some(path) = self.ui.picker.as_mut().and_then(|p| p.confirm.take()) {
                    self.picker_finish(vec![path]);
                }
            }
        }
        self.redraw();
    }

    /// The accept button, Enter in the name field, or a double click.
    pub(crate) fn picker_accept(&mut self) {
        match plan_of(&self.ui) {
            Plan::Nothing => {}
            Plan::Enter(dir) => {
                if let Some(p) = &mut self.ui.picker {
                    p.name.focused = p.req.kind == Kind::Save;
                }
                self.go(Location::Dir(dir));
            }
            Plan::Choose(paths) => self.picker_finish(paths),
            Plan::Overwrite(path) => {
                if let Some(p) = &mut self.ui.picker {
                    p.confirm = Some(path);
                }
            }
        }
    }

    fn picker_finish(&mut self, paths: Vec<PathBuf>) {
        if let Some(p) = &mut self.ui.picker {
            let filter = (!p.req.filters.is_empty()).then_some(p.filter);
            p.done = Some(Answer { paths, filter });
        }
        self.exit.stop();
    }

    /// A file was opened (double click, Enter): that is the answer.
    pub(crate) fn picker_open_file(&mut self, path: PathBuf) {
        let Some(picker) = &mut self.ui.picker else {
            return;
        };
        if picker.req.kind == Kind::Save {
            if let Some(name) = path.file_name() {
                picker.name.query = name.to_string_lossy().into_owned();
                picker.name.cursor = picker.name.query.chars().count();
            }
            self.picker_accept();
        } else if picker.req.kind == Kind::Open {
            self.picker_finish(vec![path]);
        }
        self.redraw();
    }

    /// Save: a single selected file puts its name in the field.
    pub(crate) fn picker_sync_name(&mut self) {
        let Some(picker) = &mut self.ui.picker else {
            return;
        };
        if picker.req.kind != Kind::Save || self.ui.folder.selected.len() != 1 {
            return;
        }
        let Some(entry) = self
            .ui
            .folder
            .cursor
            .and_then(|c| self.ui.folder.entry(c))
            .filter(|e| !e.is_dir)
        else {
            return;
        };
        picker.name.query = entry.name.clone();
        picker.name.cursor = picker.name.query.chars().count();
    }

    /// Keys in picker mode; true if used. Escape cancels, the name field
    /// takes typing, and the file operations are off.
    pub(crate) fn picker_key(&mut self, event: &KeyEvent) -> bool {
        let (ctrl, alt, shift) = (
            self.modifiers.ctrl,
            self.modifiers.alt,
            self.modifiers.shift,
        );
        let Some(picker) = &mut self.ui.picker else {
            return false;
        };
        let sym = event.keysym;
        if picker.confirm.is_some() {
            match sym {
                Keysym::Escape => picker.confirm = None,
                Keysym::Return | Keysym::KP_Enter => {
                    if let Some(path) = picker.confirm.take() {
                        self.picker_finish(vec![path]);
                    }
                }
                _ => return true,
            }
            self.redraw();
            return true;
        }
        if picker.open {
            picker.open = false;
            self.redraw();
            return sym == Keysym::Escape;
        }
        if sym == Keysym::Escape && !self.ui.search.active() && !self.ui.search.focused {
            self.exit.stop();
            return true;
        }
        let save = picker.req.kind == Kind::Save;
        // Typing with the list focused goes to the name, in Save.
        let typing = event
            .utf8
            .as_deref()
            .is_some_and(|t| !ctrl && !alt && !t.is_empty() && !t.chars().any(char::is_control));
        if save && !picker.name.focused && !self.ui.search.focused && typing {
            picker.name.focused = true;
            picker.name.cursor = picker.name.query.chars().count();
        }
        if picker.name.focused {
            let keyed = picker
                .name
                .key(sym, event.utf8.as_deref(), ctrl, alt, false);
            match keyed {
                Keyed::Open => self.picker_accept(),
                Keyed::Ignored => {
                    // Up and Down leave the field for the list.
                    if matches!(
                        sym,
                        Keysym::Up | Keysym::Down | Keysym::Page_Up | Keysym::Page_Down
                    ) {
                        picker.name.focused = false;
                        return false;
                    }
                    return false;
                }
                _ => {}
            }
            self.redraw();
            return true;
        }
        match sym {
            Keysym::Delete | Keysym::F2 => true,
            Keysym::c
            | Keysym::C
            | Keysym::x
            | Keysym::X
            | Keysym::v
            | Keysym::V
            | Keysym::z
            | Keysym::Z
                if ctrl =>
            {
                true
            }
            Keysym::n | Keysym::N if ctrl && shift && !save => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from("/d").join(name),
            is_dir,
            is_symlink: false,
            hidden: false,
            size: 1,
            items: None,
            modified: None,
            kind: String::new(),
        }
    }

    fn run(kind: Kind, name: &str, selected: &[&str]) -> Plan {
        let entries = vec![
            entry("sub", true),
            entry("a.txt", false),
            entry("b.txt", false),
        ];
        let visible: Vec<usize> = (0..entries.len()).collect();
        let selected = selected.iter().map(|s| s.to_string()).collect();
        plan(kind, name, Path::new("/d"), &entries, &visible, &selected)
    }

    #[test]
    fn open_plans() {
        assert_eq!(run(Kind::Open, "", &[]), Plan::Nothing);
        assert_eq!(
            run(Kind::Open, "", &["b.txt", "a.txt"]),
            Plan::Choose(vec!["/d/a.txt".into(), "/d/b.txt".into()])
        );
        assert_eq!(run(Kind::Open, "", &["sub"]), Plan::Enter("/d/sub".into()));
        // Files win over a folder in the same selection.
        assert_eq!(
            run(Kind::Open, "", &["sub", "a.txt"]),
            Plan::Choose(vec!["/d/a.txt".into()])
        );
    }

    #[test]
    fn folder_plans() {
        assert_eq!(run(Kind::Folder, "", &[]), Plan::Choose(vec!["/d".into()]));
        assert_eq!(
            run(Kind::Folder, "", &["sub", "a.txt"]),
            Plan::Choose(vec!["/d/sub".into()])
        );
    }

    #[test]
    fn save_plans() {
        assert_eq!(run(Kind::Save, "  ", &[]), Plan::Nothing);
        assert_eq!(run(Kind::Save, "x/y", &[]), Plan::Nothing);
        assert_eq!(run(Kind::Save, "..", &[]), Plan::Nothing);
        assert_eq!(
            run(Kind::Save, "new.txt", &[]),
            Plan::Choose(vec!["/d/new.txt".into()])
        );
        assert_eq!(
            run(Kind::Save, " new.txt ", &[]),
            Plan::Choose(vec!["/d/new.txt".into()])
        );
        assert_eq!(
            run(Kind::Save, "a.txt", &[]),
            Plan::Overwrite("/d/a.txt".into())
        );
        assert_eq!(run(Kind::Save, "sub", &[]), Plan::Enter("/d/sub".into()));
    }

    #[test]
    fn labels_and_filters() {
        let req = Request::from_options(Kind::Open, "", &Default::default());
        let p = Picker::new(req);
        assert_eq!(p.accept_label(), "Open");
        assert_eq!(p.title(), "Open File");
        let mut req = Request::from_options(Kind::Open, "Pick", &Default::default());
        req.filters = vec![crate::request::Filter {
            label: "Text".into(),
            rules: vec![crate::request::Rule::Glob("*.txt".into())],
        }];
        let p = Picker::new(req);
        let shown = p.shown();
        assert!(shown(&entry("a.txt", false)));
        assert!(!shown(&entry("a.png", false)));
        let folders = Picker::new(Request::from_options(Kind::Folder, "", &Default::default()));
        assert!(!folders.shown()(&entry("a.txt", false)));
    }
}
