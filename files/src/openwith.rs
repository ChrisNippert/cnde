//! "Open With…": a modal sheet over the window to pick the app for a file,
//! with a filter field, the apps that handle the type first, and a switch
//! to make the pick the default.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use cnde_ui::{Color, Painter, Rect, Weight};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym};

use crate::{
    App, Event, apps, mime,
    model::fuzzy_score,
    search::{Keyed, Search},
    view::{Colors, Painted, Target},
};

const SHEET_W: f32 = 460.0;
const SHEET_H: f32 = 580.0;
const PAD: f32 = 20.0;
const ROW_H: f32 = 48.0;
const HEADER_H: f32 = 28.0;
const TOP_AREA: f32 = 100.0;
const FOOTER_H: f32 = 118.0;
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// What on the sheet the pointer can hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Outside the panel: swallowed.
    Backdrop,
    Panel,
    /// A position in [`Sheet::rows`]' apps.
    App(usize),
    Always,
    Cancel,
    Open,
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Header(&'static str),
    /// An index into the app index, and whether it is the default.
    App(usize, bool),
}

impl Row {
    fn height(&self) -> f32 {
        match self {
            Row::Header(_) => HEADER_H,
            Row::App(..) => ROW_H,
        }
    }
}

pub struct Sheet {
    pub files: Vec<PathBuf>,
    pub mime: String,
    pub title: String,
    /// "Always use for PNG images".
    pub always_label: String,
    pub always: bool,
    /// The filter field; always focused.
    pub search: Search,
    recommended: Vec<usize>,
    others: Vec<usize>,
    /// The chosen app, as a position among the app rows.
    pub selected: usize,
    pub scroll: f32,
    last_click: Option<(Instant, usize)>,
}

impl Sheet {
    pub fn new(
        files: Vec<PathBuf>,
        mime: String,
        title: String,
        always_label: String,
        recommended: Vec<usize>,
        others: Vec<usize>,
    ) -> Self {
        let search = Search {
            focused: true,
            ..Search::default()
        };
        Sheet {
            files,
            mime,
            title,
            always_label,
            always: false,
            search,
            recommended,
            others,
            selected: 0,
            scroll: 0.0,
            last_click: None,
        }
    }

    /// The list for the current filter: apps that handle the type, then the
    /// others. A filter orders each group by how well the name matches.
    pub fn rows(&self) -> Vec<Row> {
        let Some(ix) = apps::index() else {
            return Vec::new();
        };
        let query = self.search.query.trim();
        let pick = |list: &[usize]| -> Vec<usize> {
            if query.is_empty() {
                return list.to_vec();
            }
            let mut scored: Vec<(i32, usize)> = list
                .iter()
                .filter_map(|&i| Some((fuzzy_score(query, &ix.apps[i].name)?, i)))
                .collect();
            scored.sort_by_key(|&(score, i)| (std::cmp::Reverse(score), i));
            scored.into_iter().map(|(_, i)| i).collect()
        };
        let (rec, others) = (pick(&self.recommended), pick(&self.others));
        let mut rows = Vec::new();
        if !rec.is_empty() {
            rows.push(Row::Header("Recommended"));
            // The type's default is the first of its apps.
            let default = self.recommended.first().copied();
            rows.extend(rec.iter().map(|&i| Row::App(i, Some(i) == default)));
        }
        if !others.is_empty() {
            rows.push(Row::Header(if rec.is_empty() {
                "Applications"
            } else {
                "Other apps"
            }));
            rows.extend(others.iter().map(|&i| Row::App(i, false)));
        }
        rows
    }

    fn app_count(rows: &[Row]) -> usize {
        rows.iter().filter(|r| matches!(r, Row::App(..))).count()
    }

    /// The chosen app's index in the app index.
    pub fn chosen(&self) -> Option<usize> {
        self.rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::App(i, _) => Some(i),
                Row::Header(_) => None,
            })
            .nth(self.selected)
    }

    /// Top and bottom of app row `n` in the list's own coordinates.
    fn extent(rows: &[Row], n: usize) -> Option<(f32, f32)> {
        let mut y = 0.0;
        let mut seen = 0;
        for row in rows {
            if matches!(row, Row::App(..)) {
                if seen == n {
                    return Some((y, y + row.height()));
                }
                seen += 1;
            }
            y += row.height();
        }
        None
    }

    fn content_h(rows: &[Row]) -> f32 {
        rows.iter().map(Row::height).sum::<f32>() + 8.0
    }

    /// Where the panel, and its list, are in a window `w` x `h`.
    fn layout(w: f32, h: f32) -> (Rect, Rect) {
        let ph = SHEET_H.min(h - 48.0).max(260.0);
        let pw = SHEET_W.min(w - 32.0);
        let panel = Rect::new((w - pw) / 2.0, (h - ph) / 2.0, pw, ph);
        let list = Rect::new(
            panel.x,
            panel.y + TOP_AREA,
            panel.w,
            (ph - TOP_AREA - FOOTER_H).max(40.0),
        );
        (panel, list)
    }

    fn max_scroll(&self, h: f32) -> f32 {
        let (_, list) = Self::layout(f32::MAX / 4.0, h);
        (Self::content_h(&self.rows()) - list.h).max(0.0)
    }

    /// Move the choice by `delta` apps, keeping it in view.
    fn step(&mut self, delta: isize, h: f32) {
        let rows = self.rows();
        let count = Self::app_count(&rows);
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.reveal(&rows, h);
    }

    fn reveal(&mut self, rows: &[Row], h: f32) {
        let (_, list) = Self::layout(f32::MAX / 4.0, h);
        if let Some((top, bottom)) = Self::extent(rows, self.selected) {
            // The first app also shows its header.
            let top = if self.selected == 0 { 0.0 } else { top };
            if top < self.scroll {
                self.scroll = top;
            } else if bottom + 4.0 > self.scroll + list.h {
                self.scroll = bottom + 4.0 - list.h;
            }
        }
        self.scroll = self
            .scroll
            .clamp(0.0, (Self::content_h(rows) - list.h).max(0.0));
    }
}

/// Paint the sheet over everything and register what it takes. Its hits
/// are the last ones and nothing else can be hit while it is open.
pub fn paint(
    sheet: &Sheet,
    hover: Option<Target>,
    p: &mut Painter<'_, '_>,
    c: &Colors,
    out: &mut Painted,
    (w, h): (f32, f32),
    radius: f32,
) {
    let t = p.theme;
    let start = out.hits.len();
    let (panel, list) = Sheet::layout(w, h);
    // Dim the window; the backdrop eats presses.
    p.canvas.rounded_rect(
        0.0,
        0.0,
        w,
        h,
        radius,
        Color::rgba(0, 0, 0, if t.dark { 0.5 } else { 0.35 }),
    );
    out.hits
        .push((Rect::new(0.0, 0.0, w, h), Target::Sheet(Hit::Backdrop)));
    for i in 1..=5 {
        let grow = i as f32 * 3.0;
        p.canvas.rounded_rect(
            panel.x - grow,
            panel.y - grow + 6.0,
            panel.w + grow * 2.0,
            panel.h + grow * 2.0,
            t.popover_radius + grow,
            Color::rgba(0, 0, 0, 0.06),
        );
    }
    let bg = t.popover_background;
    p.canvas
        .rounded_rect(panel.x, panel.y, panel.w, panel.h, t.popover_radius, bg);
    out.hits.push((panel, Target::Sheet(Hit::Panel)));

    // The list, then the header and footer over its overflow.
    let rows = sheet.rows();
    let on = |hit: Hit| hover == Some(Target::Sheet(hit));
    let size = t.font_size;
    let mut y = list.y + 4.0 - sheet.scroll;
    let mut n = 0;
    for row in &rows {
        let rh = row.height();
        match row {
            Row::Header(text) => {
                if y + rh > list.y && y < list.bottom() {
                    p.label_v(
                        text,
                        panel.x + PAD,
                        y + rh - 9.0,
                        panel.w - PAD * 2.0,
                        size - 2.0,
                        Weight::Bold,
                        c.dim,
                    );
                }
            }
            Row::App(i, default) => {
                if y + rh > list.y && y < list.bottom() {
                    let rect = Rect::new(panel.x + 8.0, y + 2.0, panel.w - 16.0, rh - 4.0);
                    if sheet.selected == n {
                        p.canvas
                            .rounded_rect(rect.x, rect.y, rect.w, rect.h, 9.0, c.sel);
                    } else if on(Hit::App(n)) {
                        p.canvas
                            .rounded_rect(rect.x, rect.y, rect.w, rect.h, 9.0, t.hover);
                    }
                    if let Some(app) = apps::index().map(|ix| &ix.apps[*i]) {
                        app_icon(
                            p,
                            app.icon.as_deref(),
                            rect.x + 10.0,
                            rect.y + (rect.h - 32.0) / 2.0,
                        );
                        let mut right = rect.right() - 12.0;
                        if *default {
                            let label = "Default";
                            let tw = p.measure(label, size - 2.0, Weight::Normal);
                            right -= tw;
                            p.label_v(
                                label,
                                right,
                                rect.y + rect.h / 2.0,
                                tw + 2.0,
                                size - 2.0,
                                Weight::Normal,
                                c.dim,
                            );
                            right -= 10.0;
                        }
                        p.label_v(
                            &app.name,
                            rect.x + 54.0,
                            rect.y + rect.h / 2.0,
                            (right - rect.x - 54.0).max(20.0),
                            size,
                            Weight::Normal,
                            c.ink,
                        );
                    }
                    let visible = rect.y.max(list.y)..rect.bottom().min(list.bottom());
                    if visible.end > visible.start {
                        out.hits.push((
                            Rect::new(rect.x, visible.start, rect.w, visible.end - visible.start),
                            Target::Sheet(Hit::App(n)),
                        ));
                    }
                }
                n += 1;
            }
        }
        y += rh;
    }
    if rows.is_empty() {
        let msg = if apps::index().is_some() {
            "No matching apps"
        } else {
            "Loading apps…"
        };
        let tw = p.measure(msg, size, Weight::Normal);
        p.label_v(
            msg,
            panel.x + (panel.w - tw) / 2.0,
            list.y + 40.0,
            tw + 2.0,
            size,
            Weight::Normal,
            c.dim,
        );
    }

    // Header: covers the list's top overflow.
    let clip_top = (panel.x, panel.y, panel.w, TOP_AREA);
    p.canvas.rounded_rect_clipped(
        panel.x,
        panel.y,
        panel.w,
        panel.h,
        t.popover_radius,
        bg,
        clip_top,
    );
    p.label_v(
        &sheet.title,
        panel.x + PAD,
        panel.y + 30.0,
        panel.w - PAD * 2.0,
        size + 3.0,
        Weight::Bold,
        c.ink,
    );
    let field = Rect::new(panel.x + PAD, panel.y + 52.0, panel.w - PAD * 2.0, 34.0);
    p.entry(
        field,
        &sheet.search.query,
        "Search apps",
        Some(sheet.search.cursor),
        false,
    );
    p.canvas
        .rounded_rect(panel.x, list.y - 1.0, panel.w, 1.0, 0.0, c.hair);

    // Footer: the switch and the buttons.
    let foot = list.bottom();
    p.canvas.rounded_rect_clipped(
        panel.x,
        panel.y,
        panel.w,
        panel.h,
        t.popover_radius,
        bg,
        (panel.x, foot, panel.w, panel.bottom() - foot),
    );
    p.canvas
        .rounded_rect(panel.x, foot, panel.w, 1.0, 0.0, c.hair);
    let row = Rect::new(panel.x + 8.0, foot + 8.0, panel.w - 16.0, 40.0);
    if on(Hit::Always) {
        p.canvas
            .rounded_rect(row.x, row.y, row.w, row.h, 9.0, t.hover);
    }
    p.label_v(
        &sheet.always_label,
        row.x + 12.0,
        row.y + row.h / 2.0,
        row.w - 100.0,
        size,
        Weight::Normal,
        c.ink,
    );
    p.switch(
        row.right() - 12.0 - t.switch_width,
        row.y + (row.h - t.switch_height) / 2.0,
        sheet.always as u8 as f32,
    );
    out.hits.push((row, Target::Sheet(Hit::Always)));

    let (bw, bh) = (104.0, 36.0);
    let by = panel.bottom() - PAD - bh;
    let open = Rect::new(panel.right() - PAD - bw, by, bw, bh);
    let cancel = Rect::new(open.x - 10.0 - bw, by, bw, bh);
    p.button(cancel, on(Hit::Cancel) as u8 as f32);
    centred(p, cancel, "Cancel", size, c.ink);
    let can_open = sheet.chosen().is_some();
    let fill = if on(Hit::Open) && can_open {
        t.accent.mix(Color::rgb(255, 255, 255), 0.15)
    } else {
        t.accent
    };
    let fill = if can_open { fill } else { fill.with_alpha(0.4) };
    p.canvas
        .rounded_rect(open.x, open.y, open.w, open.h, t.button_radius, fill);
    centred(p, open, "Open", size, t.on_accent);
    out.hits.push((cancel, Target::Sheet(Hit::Cancel)));
    out.hits.push((open, Target::Sheet(Hit::Open)));

    // A thin indicator for a long list.
    let content = Sheet::content_h(&rows);
    if content > list.h {
        let track = list.h - 8.0;
        let thumb = (track * list.h / content).max(24.0);
        let range = (content - list.h).max(1.0);
        let ty = list.y + 4.0 + (track - thumb) * (sheet.scroll / range).clamp(0.0, 1.0);
        p.canvas.rounded_rect(
            panel.right() - 8.0,
            ty,
            3.0,
            thumb,
            1.5,
            c.ink.with_alpha(0.25),
        );
    }
    out.menu_rows = out.hits.len() - start;
}

fn centred(p: &mut Painter<'_, '_>, rect: Rect, text: &str, size: f32, ink: Color) {
    let tw = p.measure(text, size, Weight::Normal);
    p.label_v(
        text,
        rect.x + (rect.w - tw) / 2.0,
        rect.y + rect.h / 2.0,
        tw + 2.0,
        size,
        Weight::Normal,
        ink,
    );
}

/// An app's icon at (`x`, `y`), 32 px; a plain one when it has none.
fn app_icon(p: &mut Painter<'_, '_>, name: Option<&str>, x: f32, y: f32) {
    let drawn = name.is_some_and(|n| p.canvas.app_icon(p.icons, n, x, y, 32.0));
    if !drawn {
        p.icon(
            "application-x-executable-symbolic",
            x + 16.0,
            y + 16.0,
            24.0,
            p.theme.window_dim,
        );
    }
}

impl App {
    /// Show the sheet for the menu's file (a recent row, else the selection).
    pub(crate) fn open_with_sheet(&mut self, recent: Option<usize>) {
        let files = self.menu_files(recent);
        let Some((first, first_dir)) = files.first().cloned() else {
            return;
        };
        let kind = mime::of_path(&first, first_dir);
        if files.iter().any(|(p, d)| mime::of_path(p, *d) != kind) {
            self.show_error("Pick files of one type to choose an app".into());
            return;
        }
        let Some(ix) = apps::index() else {
            self.show_error("Still looking for installed apps".into());
            return;
        };
        let recommended = ix.for_mime(&kind);
        let others: Vec<usize> = ix
            .all()
            .into_iter()
            .filter(|i| !recommended.contains(i))
            .collect();
        let name = |p: &Path| {
            p.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        };
        let title = if files.len() == 1 {
            format!("Open “{}” with", name(&first))
        } else {
            format!("Open {} files with", files.len())
        };
        let fallback = mime::describe(&first, &kind);
        let label = format!("Always use for {}", apps::describe_type(&kind, &fallback));
        self.ui.x.sheet = Some(Sheet::new(
            files.into_iter().map(|(p, _)| p).collect(),
            kind,
            title,
            label,
            recommended,
            others,
        ));
        self.redraw();
    }

    /// Open the sheet's files with the chosen app, and close it.
    fn sheet_open(&mut self) {
        let Some(sheet) = &self.ui.x.sheet else {
            return;
        };
        let (Some(app), Some(ix)) = (sheet.chosen(), apps::index()) else {
            return;
        };
        let app = ix.apps[app].clone();
        let (files, kind, always) = (sheet.files.clone(), sheet.mime.clone(), sheet.always);
        self.ui.x.sheet = None;
        let (env, tx) = (ix.env.clone(), self.events.clone());
        std::thread::spawn(move || {
            if always && let Err(err) = apps::set_default(&env, &kind, &app.id) {
                let _ = tx.send(Event::Error(format!("Can't save the default app: {err}")));
            }
            let cwd = files[0].parent().unwrap_or(Path::new("/")).to_path_buf();
            if let Err(err) = apps::launch(&app, &files, &cwd) {
                let _ = tx.send(Event::Error(err));
            }
        });
        self.redraw();
    }

    fn sheet_hit(&mut self, hit: Hit) {
        match hit {
            Hit::Backdrop | Hit::Panel => {}
            Hit::Cancel => self.ui.x.sheet = None,
            Hit::Open => self.sheet_open(),
            Hit::Always => {
                if let Some(s) = &mut self.ui.x.sheet {
                    s.always = !s.always;
                }
            }
            Hit::App(n) => {
                let now = Instant::now();
                let Some(s) = &mut self.ui.x.sheet else {
                    return;
                };
                let double = s
                    .last_click
                    .is_some_and(|(t, m)| m == n && now - t < DOUBLE_CLICK);
                s.selected = n;
                s.last_click = Some((now, n));
                if double {
                    self.sheet_open();
                }
            }
        }
        self.redraw();
    }

    /// A press while the sheet is up: true (it takes every press).
    pub(crate) fn sheet_press(&mut self, position: (f64, f64)) -> bool {
        if self.ui.x.sheet.is_none() {
            return false;
        }
        if let Some(Target::Sheet(hit)) = self.painted.hit(position) {
            self.sheet_hit(hit);
        }
        true
    }

    /// The wheel over the sheet's list; true if it was up.
    pub(crate) fn sheet_wheel(&mut self, delta: f32) -> bool {
        let h = self.size.1 as f32;
        let Some(s) = &mut self.ui.x.sheet else {
            return false;
        };
        s.scroll = (s.scroll + delta).clamp(0.0, s.max_scroll(h));
        self.redraw();
        true
    }

    /// A key while the sheet is up: true (it takes every key).
    pub(crate) fn sheet_key(&mut self, event: &KeyEvent) -> bool {
        let h = self.size.1 as f32;
        let (ctrl, alt) = (self.modifiers.ctrl, self.modifiers.alt);
        let Some(s) = &mut self.ui.x.sheet else {
            return false;
        };
        match event.keysym {
            Keysym::Escape => self.ui.x.sheet = None,
            Keysym::Up => s.step(-1, h),
            Keysym::Down => s.step(1, h),
            Keysym::Page_Up => s.step(-6, h),
            Keysym::Page_Down => s.step(6, h),
            Keysym::Tab | Keysym::ISO_Left_Tab => {}
            sym => match s.search.key(sym, event.utf8.as_deref(), ctrl, alt, true) {
                Keyed::Open => {
                    self.sheet_open();
                    return true;
                }
                Keyed::Changed => {
                    s.selected = 0;
                    s.scroll = 0.0;
                }
                Keyed::Handled | Keyed::Ignored => {}
            },
        }
        self.redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_fits_small_windows() {
        let (panel, list) = Sheet::layout(640.0, 420.0);
        assert!(panel.w <= 640.0 - 32.0 + 0.1);
        assert!(panel.bottom() <= 420.0);
        assert!(list.h >= 40.0);
        assert!(list.bottom() <= panel.bottom());
    }

    #[test]
    fn extent_skips_headers() {
        let rows = vec![Row::Header("a"), Row::App(0, true), Row::App(1, false)];
        assert_eq!(Sheet::extent(&rows, 0), Some((HEADER_H, HEADER_H + ROW_H)));
        assert_eq!(
            Sheet::extent(&rows, 1),
            Some((HEADER_H + ROW_H, HEADER_H + 2.0 * ROW_H))
        );
        assert_eq!(Sheet::extent(&rows, 2), None);
    }
}
