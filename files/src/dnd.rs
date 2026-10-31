//! Drag and drop of files: starting a drag from the selection, and taking
//! drops from other apps (or from this window) onto folders.

use std::{
    io::Read,
    os::{fd::OwnedFd, unix::fs::MetadataExt},
    path::PathBuf,
};

use cnde_ui::{Canvas, Weight};
use smithay_client_toolkit::{
    data_device_manager::{
        data_device::DataDeviceHandler,
        data_offer::{DataOfferHandler, DragOffer},
    },
    reexports::client::{
        Connection, QueueHandle,
        protocol::{
            wl_data_device::WlDataDevice, wl_data_device_manager::DndAction, wl_shm,
            wl_surface::WlSurface,
        },
    },
    shell::WaylandSurface,
};
use tracing::debug;

use crate::{
    App, Event,
    clipboard::{GNOME, Payload, URI_LIST},
    folder::Location,
    transfer::{Mode, Msg},
    view::{self, SideSlot, Target},
};

/// The pointer must move this far (logical px) with the button down to drag.
const DRAG_DISTANCE: f64 = 6.0;
const MAX_LIST: u64 = 4 << 20;

/// What is highlighted as the drop target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Over {
    /// A folder row or tile (position in the listing).
    Row(usize),
}

/// A press on a selected row that might become a drag.
pub struct Press {
    pub at: (f64, f64),
    pub serial: u32,
}

#[derive(Default)]
pub struct State {
    pub press: Option<Press>,
    /// A press on part of a multiple selection: it narrows to this row on
    /// release, unless it turned into a drag.
    pub defer: Option<usize>,
    pub over: Option<Over>,
    /// A drop would add bookmarks at this slot of the sidebar.
    pub bookmark_at: Option<usize>,
    /// Where a drop would go right now.
    dest: Option<PathBuf>,
    /// What the drag coming in carries, when known.
    paths: Vec<PathBuf>,
    pos: (f64, f64),
    /// What was last told to the compositor, to not repeat it.
    told: Option<(Option<PathBuf>, DndAction)>,
    incoming: bool,
    /// The card drawn at the pointer for our own drag.
    icon: Option<WlSurface>,
}

fn same_drive(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => false,
    }
}

impl App {
    /// A press on the list landed on a selected row: it may start a drag.
    pub(crate) fn drag_arm(&mut self, at: (f64, f64), serial: u32) {
        self.ui.x.dnd.press = Some(Press { at, serial });
    }

    /// The button went up.
    pub(crate) fn drag_release(&mut self) {
        self.ui.x.dnd.press = None;
        if let Some(pos) = self.ui.x.dnd.defer.take() {
            self.ui.folder.select_only(pos);
            self.redraw();
        }
    }

    /// Pointer motion with a press armed: past the threshold, start a drag.
    pub(crate) fn drag_motion(&mut self, to: (f64, f64)) {
        let Some(press) = &self.ui.x.dnd.press else {
            return;
        };
        let moved = (to.0 - press.at.0).hypot(to.1 - press.at.1);
        if moved < DRAG_DISTANCE {
            return;
        }
        let serial = press.serial;
        self.ui.x.dnd.press = None;
        self.ui.x.dnd.defer = None;
        let paths = match self.selected_recent() {
            Some(row) => vec![row.path.clone()],
            None => self.selected_paths(),
        };
        if paths.is_empty() {
            return;
        }
        let icon = self.make_drag_icon();
        let Some(data) = &mut self.data else {
            return;
        };
        let Some(device) = &data.device else {
            return;
        };
        let payload = Payload::Files { paths, cut: false };
        let source = data.manager.create_drag_and_drop_source(
            &self.qh,
            payload.mimes(),
            DndAction::Copy | DndAction::Move,
        );
        debug!("starting a drag");
        source.start_drag(device, self.window.wl_surface(), icon.as_ref(), serial);
        data.drag = Some((source, payload));
        self.ui.hover = None;
        self.ui.hover_row = None;
        self.redraw();
    }

    /// A small card the compositor draws at the pointer while dragging: the
    /// first selected item's icon and name, with a count when there are more.
    fn make_drag_icon(&mut self) -> Option<WlSurface> {
        const H: f32 = 44.0;
        const W: f32 = 168.0;
        let (name, icon_name, count) = if let Some(row) = self.selected_recent() {
            (row.name.clone(), view::icon_for_name(&row.name, row.is_dir), 1)
        } else {
            let selected = self.selected_entries();
            let first = selected.first()?;
            (first.name.clone(), view::icon_for(first), selected.len())
        };
        let scale = self.scale.max(1);
        let (w, h) = ((W as i32) * scale, (H as i32) * scale);
        let (buffer, pixels) = self.pool.create_buffer(w, h, w * 4, self.shm_format).ok()?;
        {
            let mut canvas = Canvas::new(pixels, w as u32, h as u32, scale as f32)?;
            let res = &mut self.res;
            let t = &res.theme;
            canvas.rounded_rect(0.0, 0.0, W, H, 12.0, t.popover_background);
            canvas.stroke_rounded_rect(
                0.5,
                0.5,
                W - 1.0,
                H - 1.0,
                11.5,
                1.0,
                t.window_text.with_alpha(0.15),
            );
            canvas.icon(
                &mut res.icons,
                icon_name,
                12.0,
                (H - 20.0) / 2.0,
                20.0,
                t.window_text,
            );
            let mut layout = view::fit(
                &mut res.fonts,
                &name,
                t.font_size,
                Weight::Normal,
                scale as f32,
                W - 44.0 - if count > 1 { 22.0 } else { 0.0 },
            );
            let ty = (H - layout.height) / 2.0;
            canvas.text(&mut res.fonts, &mut layout, 40.0, ty, t.window_text);
            if count > 1 {
                canvas.circle(W - 22.0, H / 2.0, 11.0, t.accent);
                let mut n = res.fonts.layout(
                    &count.min(99).to_string(),
                    t.font_size * 0.85,
                    Weight::Normal,
                    scale as f32,
                );
                let (nx, ny) = (W - 22.0 - n.width / 2.0, (H - n.height) / 2.0);
                canvas.text(&mut res.fonts, &mut n, nx, ny, t.on_accent);
            }
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        if let Some(old) = self.ui.x.dnd.icon.take() {
            old.destroy();
        }
        let surface = self.compositor.create_surface(&self.qh);
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        buffer.attach_to(&surface).ok()?;
        surface.commit();
        self.ui.x.dnd.icon = Some(surface.clone());
        Some(surface)
    }

    /// Where a drop at `at` would land, and what to highlight.
    fn drop_target(&self, at: (f64, f64)) -> Option<(PathBuf, Option<Over>, Option<Target>)> {
        match self.painted.hit(at) {
            Some(t @ Target::Place(i)) => {
                let p = self.ui.places.get(i)?.path.clone();
                return Some((p, None, Some(t)));
            }
            Some(t @ Target::Bookmark(i)) => {
                let p = self.ui.bookmarks.get(i)?.path.clone();
                return Some((p, None, Some(t)));
            }
            Some(t @ Target::Drive(i)) => {
                let p = self.ui.drives.get(i)?.mount.clone();
                return Some((p, None, Some(t)));
            }
            Some(_) => return None,
            None => {}
        }
        let Location::Dir(open) = &self.ui.location else {
            return None;
        };
        let list = self.painted.list;
        if !list.contains(at) {
            return None;
        }
        if let Some(pos) = view::pos_at(&self.ui, list, at)
            && let Some(e) = self.ui.folder.entry(pos)
            && e.is_dir
            && !self.ui.x.dnd.paths.contains(&e.path)
        {
            return Some((e.path.clone(), Some(Over::Row(pos)), None));
        }
        Some((open.clone(), None, None))
    }

    /// Re-evaluate the drag at `at`: highlight, accept, pick an action.
    fn drag_over(&mut self, at: (f64, f64)) {
        let dnd = &mut self.ui.x.dnd;
        dnd.pos = at;
        if !dnd.incoming {
            return;
        }
        // Folders dropped between the sidebar's shortcuts become new ones.
        let slot = match view::side_slot(&self.ui, self.size.1 as f32, at, false) {
            Some(SideSlot::Insert(k)) if self.ui.x.dnd.paths.iter().all(|p| p.is_dir()) => Some(k),
            _ => None,
        };
        self.ui.x.dnd.bookmark_at = slot;
        let bm_changed = self.ui.x.bm_insert != slot;
        self.ui.x.bm_insert = slot;
        let target = match slot {
            Some(_) => Some((PathBuf::new(), None, None)),
            None => self.drop_target(at),
        };
        let (dest, over, side) = match target {
            Some((d, o, s)) => (Some(d), o, s),
            None => (None, None, None),
        };
        let dnd = &self.ui.x.dnd;
        let copy = self.modifiers.ctrl
            || dest.as_ref().is_none_or(|d| {
                dnd.paths
                    .first()
                    .and_then(|p| p.parent())
                    .is_none_or(|from| !same_drive(from, d))
            });
        let preferred = if copy {
            DndAction::Copy
        } else {
            DndAction::Move
        };
        debug!(
            ?dest,
            ?preferred,
            paths = self.ui.x.dnd.paths.len(),
            "drag over"
        );
        let told = (dest.clone(), preferred);
        let changed = dnd.told.as_ref() != Some(&told);
        if changed
            && let Some(offer) = self
                .data
                .as_ref()
                .and_then(|d| d.device.as_ref())
                .and_then(|d| d.data().drag_offer())
        {
            offer.set_actions(DndAction::Copy | DndAction::Move, preferred);
            let accept = dest.is_some().then(|| URI_LIST.to_string());
            offer.accept_mime_type(offer.serial, accept);
        }
        let dnd = &mut self.ui.x.dnd;
        dnd.told = Some(told);
        dnd.dest = dest;
        let hover_changed = dnd.over != over || self.ui.hover != side || bm_changed;
        dnd.over = over;
        self.ui.hover = side;
        if hover_changed {
            self.redraw();
        }
    }

    /// The paths of a drag from another app arrived.
    pub(crate) fn drag_paths(&mut self, paths: Vec<PathBuf>) {
        if self.ui.x.dnd.incoming {
            self.ui.x.dnd.paths = paths;
            let at = self.ui.x.dnd.pos;
            self.ui.x.dnd.told = None;
            self.drag_over(at);
        }
    }

    fn drag_clear(&mut self) {
        let was = self.ui.x.dnd.incoming || self.ui.x.dnd.over.is_some();
        let dnd = &mut self.ui.x.dnd;
        dnd.incoming = false;
        dnd.over = None;
        dnd.dest = None;
        dnd.paths.clear();
        dnd.told = None;
        dnd.bookmark_at = None;
        if self.ui.x.bm_drag.is_none() {
            self.ui.x.bm_insert = None;
        }
        if matches!(
            self.ui.hover,
            Some(Target::Place(_) | Target::Drive(_) | Target::Bookmark(_))
        ) {
            self.ui.hover = None;
        }
        if was {
            self.redraw();
        }
    }
}

impl DataDeviceHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        x: f64,
        y: f64,
        _: &WlSurface,
    ) {
        let Some(data) = &self.data else {
            return;
        };
        let Some(offer) = data.device.as_ref().and_then(|d| d.data().drag_offer()) else {
            return;
        };
        let files = offer.with_mime_types(|m| m.iter().any(|t| t == URI_LIST || t == GNOME));
        let dnd = &mut self.ui.x.dnd;
        dnd.incoming = files;
        dnd.told = None;
        dnd.paths.clear();
        if !files {
            return;
        }
        if let Some((_, Payload::Files { paths, .. })) = &data.drag {
            dnd.paths = paths.clone();
        } else if let Ok(pipe) = offer.receive(URI_LIST.to_string()) {
            // Where the files live decides copy or move.
            let _ = self.conn.flush();
            let tx = self.events.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut file = std::fs::File::from(OwnedFd::from(pipe));
                let _ = (&mut file).take(MAX_LIST).read_to_end(&mut bytes);
                let (_, paths) = crate::clipboard::parse_offer(URI_LIST, &bytes);
                let _ = tx.send(Event::Xfer(Msg::DragPaths(paths)));
            });
        }
        self.drag_over((x, y));
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        self.drag_clear();
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        self.drag_over((x, y));
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        let offer = self
            .data
            .as_ref()
            .and_then(|d| d.device.as_ref())
            .and_then(|d| d.data().drag_offer());
        let Some(offer) = offer else {
            return;
        };
        let dest = self.ui.x.dnd.dest.clone();
        let told = self.ui.x.dnd.told.as_ref().map(|t| t.1);
        if let Some(at) = self.ui.x.dnd.bookmark_at {
            let dirs: Vec<PathBuf> = self
                .ui
                .x
                .dnd
                .paths
                .iter()
                .filter(|p| p.is_dir())
                .cloned()
                .collect();
            let finish = !offer.selected_action.is_empty();
            self.drag_clear();
            finish_offer(&offer, finish);
            self.add_bookmarks(dirs, Some(at));
            return;
        }
        self.drag_clear();
        let action = offer.selected_action;
        let mode = if action.contains(DndAction::Copy) {
            Mode::Copy
        } else if action.contains(DndAction::Move) {
            Mode::Move
        } else {
            // Nothing agreed: take what we asked for.
            if told == Some(DndAction::Move) {
                Mode::Move
            } else {
                Mode::Copy
            }
        };
        debug!(?dest, ?action, ?mode, "drop");
        let Some(dest) = dest else {
            offer.destroy();
            return;
        };
        let pipe = match offer.receive(URI_LIST.to_string()) {
            Ok(p) => p,
            Err(_) => {
                offer.destroy();
                return;
            }
        };
        let _ = self.conn.flush();
        let (tx, conn) = (self.events.clone(), self.conn.clone());
        let finish = !action.is_empty();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut file = std::fs::File::from(OwnedFd::from(pipe));
            let _ = (&mut file).take(MAX_LIST).read_to_end(&mut bytes);
            finish_offer(&offer, finish);
            let _ = conn.flush();
            let _ = tx.send(Event::Xfer(Msg::Dropped {
                dest,
                data: bytes,
                mode,
            }));
        });
    }
}

/// Tell the source we have the data, and let go of the offer.
fn finish_offer(offer: &DragOffer, finish: bool) {
    if finish {
        offer.finish();
    }
    offer.destroy();
}

impl DataOfferHandler for App {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}
