//! The ⋮ menu in the top bar: icon size, hidden files and a few folder
//! actions; and the zoom itself (keys, wheel, slider), which is saved to
//! config.toml shortly after it stops.

use std::time::Duration;

use cnde_config::ConfigFile;
use cnde_ui::{Painter, Rect, Weight};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use tracing::warn;

use crate::{
    App,
    folder::Location,
    ops::Act,
    state,
    view::{self, Painted, Target, Ui, ViewMode},
    zoom,
};

const MENU_W: f32 = 250.0;
/// The config is written this long after the last size change.
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// The ⋮ menu while it is open.
#[derive(Default)]
pub struct MainMenu {
    /// The slider is being dragged.
    pub drag: bool,
}

const ROWS: [&str; 9] = [
    "Icon size",
    "",
    "Show hidden files",
    "Folders first",
    "New Folder",
    "Paste",
    "Select All",
    "Refresh",
    "Open Terminal Here",
];

/// The menu under the ⋮ button; its rows are the last hits.
pub fn paint(ui: &Ui, p: &mut Painter<'_, '_>, out: &mut Painted, w: f32, h: f32) {
    if ui.x.main_menu.is_none() {
        return;
    }
    let Some(button) = out
        .hits
        .iter()
        .find(|(_, t)| *t == Target::Menu)
        .map(|(r, _)| *r)
    else {
        return;
    };
    let mh = Painter::popover_height(ROWS.len());
    let x = (button.right() - MENU_W).clamp(8.0, (w - MENU_W - 8.0).max(8.0));
    let y = (button.bottom() + 6.0).min(h - mh - 8.0).max(8.0);
    let acts = [
        None,
        None,
        Some(Act::ToggleHidden),
        Some(Act::ToggleFoldersFirst),
        Some(Act::NewFolder),
        Some(Act::Paste),
        Some(Act::SelectAll),
        Some(Act::Refresh),
        Some(Act::Terminal),
    ];
    let hover = |i: usize| match acts[i] {
        Some(act) => (ui.hover == Some(Target::Act(act))) as u8 as f32,
        None => 0.0,
    };
    let rects = p.popover(x, y, MENU_W, &ROWS, None, hover, 1.0);
    let t = p.theme;
    let mode = ui.mode;
    let size = match mode {
        ViewMode::Grid => ui.grid_size,
        ViewMode::List => ui.list_size,
    };

    // The size beside its label, the slider under it.
    let value = format!("{} px", size.round() as i32);
    let vw = p.measure(&value, t.font_size - 1.0, Weight::Normal);
    let head = rects[0];
    p.label_v(
        &value,
        head.right() - 10.0 - vw,
        head.y + head.h / 2.0,
        vw + 4.0,
        t.font_size - 1.0,
        Weight::Normal,
        t.window_text.with_alpha(0.6),
    );
    let slider = Rect::new(rects[1].x + 2.0, rects[1].y, rects[1].w - 4.0, rects[1].h);
    p.scale(slider, zoom::fraction(mode, size));
    out.slider = slider;

    // The switch at the right of its row.
    let row = rects[2];
    p.switch(
        row.right() - 10.0 - t.switch_width,
        row.y + (row.h - t.switch_height) / 2.0,
        ui.folder.show_hidden as u8 as f32,
    );

    let row = rects[3];
    p.switch(
        row.right() - 10.0 - t.switch_width,
        row.y + (row.h - t.switch_height) / 2.0,
        ui.folder.folders_first as u8 as f32,
    );

    out.hits.push((Rect::new(x, y, MENU_W, mh), Target::Menu));
    out.hits.push((slider, Target::Zoom));
    let mut live = 2;
    for (rect, act) in rects.into_iter().zip(acts) {
        if let Some(act) = act {
            out.hits.push((rect, Target::Act(act)));
            live += 1;
        }
    }
    out.menu_rows = live;
}

impl App {
    /// The slider's value under the pointer, as an icon size.
    fn slider_size(&self, x: f64) -> f32 {
        let fraction = Painter::scale_value(self.painted.slider, x);
        zoom::from_fraction(self.ui.mode, fraction)
    }

    pub(crate) fn toggle_menu(&mut self) {
        self.ui.x.menu = None;
        self.ui.x.main_menu = match self.ui.x.main_menu {
            Some(_) => None,
            None => Some(MainMenu::default()),
        };
    }

    /// A press while the ⋮ menu is open. Always handled: it picks a row,
    /// drags the slider or closes the menu.
    pub(crate) fn main_menu_press(&mut self, position: (f64, f64)) -> bool {
        if self.ui.x.main_menu.is_none() {
            return false;
        }
        match self.painted.hit(position) {
            Some(Target::Zoom) => {
                if let Some(menu) = &mut self.ui.x.main_menu {
                    menu.drag = true;
                }
                let size = self.slider_size(position.0);
                self.set_zoom(size, None);
            }
            // The menu's own padding and its switch row stay open.
            Some(Target::Menu) => {}
            Some(Target::Act(Act::ToggleHidden)) => self.toggle_hidden(),
            Some(Target::Act(Act::ToggleFoldersFirst)) => self.toggle_folders_first(),
            Some(Target::Act(act)) => {
                self.ui.x.main_menu = None;
                self.act(act);
            }
            _ => self.ui.x.main_menu = None,
        }
        self.redraw();
        true
    }

    /// Pointer motion: a slider drag follows it. True when it did.
    pub(crate) fn main_menu_drag(&mut self, position: (f64, f64)) -> bool {
        if !self.ui.x.main_menu.as_ref().is_some_and(|m| m.drag) {
            return false;
        }
        let size = self.slider_size(position.0);
        self.set_zoom(size, None);
        true
    }

    pub(crate) fn main_menu_release(&mut self) {
        if let Some(menu) = &mut self.ui.x.main_menu {
            menu.drag = false;
        }
    }

    pub(crate) fn toggle_hidden(&mut self) {
        let show = !self.ui.folder.show_hidden;
        self.ui.folder.set_hidden(show);
        state::save(&self.ui.folder);
    }

    pub(crate) fn toggle_folders_first(&mut self) {
        let first = !self.ui.folder.folders_first;
        self.ui.folder.set_folders_first(first);
        state::save(&self.ui.folder);
    }

    /// Reload the open folder.
    pub(crate) fn reload_folder(&mut self) {
        if let Location::Dir(path) = self.ui.location.clone() {
            self.open_dir(path);
        }
    }

    /// The icon size of the current view.
    pub(crate) fn zoom_size(&self) -> f32 {
        match self.ui.mode {
            ViewMode::Grid => self.ui.grid_size,
            ViewMode::List => self.ui.list_size,
        }
    }

    /// Change the current view's icon size by `steps` key presses' worth
    /// (or wheel notches), keeping the item at `anchor` where it is.
    pub(crate) fn zoom_by(&mut self, steps: f32, anchor: Option<(f64, f64)>) {
        let size = self.zoom_size() + steps * zoom::step(self.ui.mode);
        self.set_zoom(size, anchor);
    }

    pub(crate) fn zoom_reset(&mut self) {
        let (_, _, default) = zoom::limits(self.ui.mode);
        self.set_zoom(default, None);
    }

    /// Set the current view's icon size. The scroll follows the item under
    /// `anchor` (a window position), else the cursor row, else the top row.
    pub(crate) fn set_zoom(&mut self, size: f32, anchor: Option<(f64, f64)>) {
        let mode = self.ui.mode;
        let size = zoom::clamp(mode, size);
        if size == self.zoom_size() {
            return;
        }
        let list = self.painted.list;
        let (cols, row_h) = self.cells();
        let scroll = self.ui.scroll;
        let row_top = |pos: usize| 4.0 + (pos / cols) as f32 * row_h - scroll;
        let n = self.ui.folder.visible.len();
        // The item that stays put, how far down the list it is, and how far
        // into its row the anchor was (a fraction of the row).
        let pointer = anchor
            .filter(|a| list.contains(*a))
            .and_then(|a| view::pos_at(&self.ui, list, a).map(|pos| (pos, a.1 as f32 - list.y)))
            .map(|(pos, py)| (pos, py, (py - row_top(pos)) / row_h));
        let cursor = self
            .ui
            .folder
            .cursor
            .filter(|&c| c < n)
            .map(|c| (c, row_top(c), 0.0))
            .filter(|&(_, top, _)| top > -row_h && top < list.h);
        let top_row = ((scroll - 4.0) / row_h).floor().max(0.0) as usize * cols;
        let (pos, py, ofs) = pointer
            .or(cursor)
            .unwrap_or((top_row, row_top(top_row), 0.0));

        match mode {
            ViewMode::Grid => self.ui.grid_size = size,
            ViewMode::List => self.ui.list_size = size,
        }
        let (cols, row_h) = self.cells();
        let max = view::max_scroll(&self.ui, self.size.0 as f32, self.size.1 as f32);
        let to = (4.0 + ((pos / cols) as f32 + ofs) * row_h - py).clamp(0.0, max);
        self.ui.scroll = to;
        self.scroll_target = to;
        self.schedule_size_save();
        self.redraw();
    }

    /// Write the sizes to config.toml once they stop changing.
    fn schedule_size_save(&mut self) {
        if let Some(token) = self.ui.x.save_timer.take() {
            self.loop_handle.remove(token);
        }
        self.ui.x.save_timer = self
            .loop_handle
            .insert_source(Timer::from_duration(SAVE_DELAY), |_, _, app| {
                app.ui.x.save_timer = None;
                app.save_sizes();
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Write the icon sizes to config.toml now, if they changed.
    pub(crate) fn save_sizes(&mut self) {
        let (grid, list) = (
            self.ui.grid_size.round() as u32,
            self.ui.list_size.round() as u32,
        );
        let files = &self.ui.config.files;
        if (files.grid_icon_size, files.list_icon_size) == (grid, list) {
            return;
        }
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let saved = ConfigFile::open(&path).and_then(|mut file| {
            file.set("files.grid_icon_size", i64::from(grid));
            file.set("files.list_icon_size", i64::from(list));
            file.save()
        });
        match saved {
            Ok(config) => self.ui.config = config,
            Err(err) => warn!(%err, "can't save the icon sizes"),
        }
    }
}
