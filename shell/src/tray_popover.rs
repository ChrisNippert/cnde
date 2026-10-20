//! The tray's overflow: a small card under the bar's chevron with the
//! icons in a grid. An xdg_popup of the panel like the calendar, so a
//! click anywhere else closes it.

use cnde_ui::{Animator, Canvas, Color, Icons, Theme, anim::SHORT};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
    reexports::{
        client::{
            QueueHandle,
            protocol::{wl_seat::WlSeat, wl_shm},
        },
        protocols::xdg::shell::client::xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
    },
    seat::pointer::{PointerEvent, PointerEventKind},
    shell::xdg::{XdgPositioner, XdgShell, popup::Popup},
    shm::slot::SlotPool,
};
use tracing::warn;

use crate::{
    App,
    tray::{self, TrayItem},
};

const COLUMNS: usize = 4;
const CELL: f32 = 40.0;
const PAD: f32 = 8.0;
const ICON: f32 = 22.0;
const BTN_RIGHT: u32 = 0x111;

pub struct TrayPopover {
    pub popup: Popup,
    /// The panel it hangs from.
    panel: usize,
    scale: i32,
    configured: bool,
    /// How many icons it was sized for.
    count: usize,
    /// The grid slot under the pointer.
    hover: Option<usize>,
    anim: Animator<usize>,
    frame_pending: bool,
    dirty: bool,
}

fn size(count: usize) -> (f32, f32) {
    let columns = count.clamp(1, COLUMNS);
    let rows = count.div_ceil(COLUMNS).max(1);
    (
        columns as f32 * CELL + 2.0 * PAD,
        rows as f32 * CELL + 2.0 * PAD,
    )
}

/// The grid slot at `point`.
fn slot_at((x, y): (f64, f64), count: usize) -> Option<usize> {
    let (x, y) = (x as f32 - PAD, y as f32 - PAD);
    if x < 0.0 || y < 0.0 {
        return None;
    }
    let (col, row) = ((x / CELL) as usize, (y / CELL) as usize);
    let slot = row * COLUMNS + col;
    (col < COLUMNS && slot < count).then_some(slot)
}

impl TrayPopover {
    /// Open under `anchor` (panel-surface coordinates of the chevron).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        compositor: &CompositorState,
        xdg_shell: &XdgShell,
        qh: &QueueHandle<App>,
        parent: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
        seat: &WlSeat,
        serial: u32,
        anchor: (i32, i32, i32, i32),
        panel: usize,
        count: usize,
        scale: i32,
    ) -> Option<Self> {
        let positioner = positioner(xdg_shell, anchor, count)?;
        let surface = compositor.create_surface(qh);
        let popup = Popup::from_surface(None, &positioner, qh, surface, xdg_shell)
            .map_err(|err| warn!(?err, "can't create tray popover"))
            .ok()?;
        parent.get_popup(popup.xdg_popup());
        popup.xdg_popup().grab(seat, serial);
        popup.wl_surface().set_buffer_scale(scale);
        popup.wl_surface().commit();
        Some(Self {
            popup,
            panel,
            scale,
            configured: false,
            count,
            hover: None,
            anim: Animator::new(),
            frame_pending: false,
            dirty: false,
        })
    }

    pub fn configured(&mut self) {
        self.configured = true;
    }

    /// The compositor is ready for a new frame. Returns true if one is owed.
    pub fn frame_done(&mut self) -> bool {
        self.frame_pending = false;
        self.dirty
    }

    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.max(1);
        self.popup.wl_surface().set_buffer_scale(self.scale);
    }

    /// Returns true if it needs a redraw.
    fn motion(&mut self, point: (f64, f64)) -> bool {
        let hover = slot_at(point, self.count);
        std::mem::replace(&mut self.hover, hover) != hover
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        icons: &mut Icons,
        theme: &Theme,
        items: &mut [TrayItem],
        hidden: &[usize],
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let scale = self.scale;
        let (width, height) = size(self.count);
        let (w, h) = (
            (width * scale as f32) as i32,
            (height * scale as f32) as i32,
        );
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate tray popover buffer");
                return;
            }
        };
        let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
            return;
        };
        self.anim.begin();
        canvas.clear(Color::rgba(0, 0, 0, 0.0));
        canvas.rounded_rect(
            0.0,
            0.0,
            width,
            height,
            theme.qs_radius,
            theme.qs_background,
        );
        for (slot, &i) in hidden.iter().take(self.count).enumerate() {
            let (col, row) = (slot % COLUMNS, slot / COLUMNS);
            let (x, y) = (PAD + col as f32 * CELL, PAD + row as f32 * CELL);
            let lit = self.anim.get(
                slot,
                if self.hover == Some(slot) { 1.0 } else { 0.0 },
                SHORT,
            );
            if lit > 0.0 {
                let c = theme.qs_tile;
                canvas.rounded_rect(
                    x,
                    y,
                    CELL,
                    CELL,
                    theme.qs_tile_radius,
                    c.with_alpha(c.a * lit),
                );
            }
            let off = (CELL - ICON) / 2.0;
            items[i].draw(
                &mut canvas,
                icons,
                (x + off, y + off, ICON),
                theme.popup_text,
            );
        }
        if self.anim.running() {
            self.dirty = true;
        }
        let surface = self.popup.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.frame(qh, FrameCallbackData(surface.clone()));
            self.frame_pending = true;
            surface.commit();
        }
    }
}

fn positioner(
    xdg_shell: &XdgShell,
    (ax, ay, aw, ah): (i32, i32, i32, i32),
    count: usize,
) -> Option<XdgPositioner> {
    let positioner = XdgPositioner::new(xdg_shell)
        .map_err(|err| warn!(?err, "no xdg positioner"))
        .ok()?;
    let (w, h) = size(count);
    positioner.set_size(w as i32, h as i32);
    positioner.set_anchor_rect(ax, ay, aw.max(1), ah.max(1));
    // Hang centred below the chevron.
    positioner.set_anchor(Anchor::Bottom);
    positioner.set_gravity(Gravity::Bottom);
    positioner.set_offset(0, 6);
    positioner
        .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
    Some(positioner)
}

impl App {
    /// The icons behind the chevron.
    fn tray_hidden(&self) -> Vec<usize> {
        tray::split(&self.tray.items, self.config.panel.tray).1
    }

    /// Clicking the chevron opens the popover, or closes it.
    pub fn toggle_tray_popover(&mut self, panel_index: usize, serial: u32) {
        let just_dismissed = self
            .tray_popover_dismissed
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(300));
        if self.tray_popover.take().is_some() || just_dismissed {
            self.redraw_panels();
            return;
        }
        let count = self.tray_hidden().len();
        let (Some(panel), Some(seat)) = (self.panels.get(panel_index), &self.seat_handle) else {
            return;
        };
        let Some(anchor) = panel.chevron_anchor() else {
            return;
        };
        if count == 0 {
            return;
        }
        tracing::debug!(count, "tray popover opens");
        // Before the new grab, or the compositor disconnects us.
        self.menu = None;
        self.quick_settings = None;
        self.calendar = None;
        self.tray_popover = TrayPopover::open(
            &self.compositor,
            &self.xdg_shell,
            &self.qh,
            &panel.layer,
            seat,
            serial,
            anchor,
            panel_index,
            count,
            panel.scale(),
        );
        self.redraw_panels();
    }

    pub fn draw_tray_popover(&mut self) {
        let hidden = self.tray_hidden();
        if self.tray_popover.is_none() {
            return;
        }
        if hidden.is_empty() {
            self.tray_popover = None;
            self.redraw_panels();
            return;
        }
        let theme = self.popup_theme();
        if let Some(popover) = &mut self.tray_popover {
            popover.draw(
                &self.qh,
                &mut self.pool,
                self.shm_format,
                &mut self.icons,
                &theme,
                &mut self.tray.items,
                &hidden,
            );
        }
    }

    pub fn tray_popover_pointer(&mut self, event: &PointerEvent) {
        let hidden = self.tray_hidden();
        let Some(popover) = &mut self.tray_popover else {
            return;
        };
        let (panel, count) = (popover.panel, popover.count);
        let item = |at| slot_at(at, count).and_then(|s| hidden.get(s)).copied();
        let redraw = match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                popover.motion(event.position)
            }
            PointerEventKind::Leave { .. } => popover.motion((-1.0, -1.0)),
            PointerEventKind::Press { button, .. } => {
                let rect = self.panels.get(panel).and_then(|p| p.chevron_anchor());
                if let (Some(i), Some(rect)) = (item(event.position), rect) {
                    self.tray_press(panel, i, rect, &event.kind);
                    // A right click's menu replaces the popover itself.
                    if button != BTN_RIGHT {
                        self.tray_popover = None;
                        self.redraw_panels();
                    }
                }
                false
            }
            PointerEventKind::Axis { .. } => {
                let rect = self.panels.get(panel).and_then(|p| p.chevron_anchor());
                if let (Some(i), Some(rect)) = (item(event.position), rect) {
                    self.tray_press(panel, i, rect, &event.kind);
                }
                false
            }
            _ => false,
        };
        if redraw {
            self.draw_tray_popover();
        }
    }
}
