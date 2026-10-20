//! The popover under the clock: a month grid to page through, and beside
//! it the notification centre (past notifications by app, Do Not Disturb)
//! with the playing song above the month. An xdg_popup of the panel like
//! quick settings, so a click anywhere else closes it.

use chrono::{Datelike, Local, Months, NaiveDate};
use cnde_ui::{
    Animator, Canvas, Color, Fonts, Icons, Painter, Rect, Theme, Weight, anim::SHORT,
    tiny_skia::Pixmap,
};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
    reexports::{
        client::{
            QueueHandle,
            protocol::{wl_seat::WlSeat, wl_shm},
        },
        protocols::xdg::shell::client::xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
    },
    shell::xdg::{XdgPositioner, XdgShell, popup::Popup},
    shm::slot::SlotPool,
};
use tracing::warn;

use crate::{
    App,
    history::{self, Entry, Group, History},
    media::{Media, Status},
};

/// The month column.
const WIDTH: f32 = 316.0;
/// The notification column.
const LIST_WIDTH: f32 = 332.0;
const PAD: f32 = 16.0;
/// The date heading above the month.
const HEADING: f32 = 58.0;
/// The month name and its arrows.
const TITLE: f32 = 40.0;
const WEEKDAYS: f32 = 26.0;
const CELL: f32 = 38.0;
const ROWS: usize = 6;
/// Room the song takes above the date, card and gap.
const MEDIA_SLOT: f32 = 96.0;
const MEDIA_CARD: f32 = 80.0;
const MEDIA_TOP: f32 = 12.0;
/// Do Not Disturb and Clear all, under the list.
const FOOTER: f32 = 54.0;
const GROUP_HEAD: f32 = 30.0;
const ITEM_GAP: f32 = 6.0;
const GROUP_GAP: f32 = 10.0;
/// Pixels the list moves per wheel step.
const SCROLL_STEP: f32 = 44.0;

/// What a point in the month card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    Previous,
    Next,
    /// The month name: back to this month.
    Today,
}

/// Anything in the popover that reacts to the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Month(Hit),
    /// A notification (by history sequence number): its default action.
    Item(u64),
    Dismiss(u64),
    /// Clear one app's group (an index into [`Calendar::app_name`]).
    ClearApp(usize),
    ClearAll,
    DoNotDisturb,
    PlayPause,
    Next,
    Previous,
    /// Switch to the n-th live player.
    Pick(usize),
}

/// Which columns the popover has; it keeps them while open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// The notification list is shown.
    pub history: bool,
    /// Room for the song above the month.
    pub media: bool,
}

impl Layout {
    fn left(self) -> f32 {
        if self.history { LIST_WIDTH } else { 0.0 }
    }

    pub fn width(self) -> f32 {
        self.left() + WIDTH
    }

    pub fn height(self) -> f32 {
        month_height() + if self.media { MEDIA_SLOT } else { 0.0 }
    }
}

/// What the popover shows besides the month, borrowed from the app.
pub struct View<'a> {
    pub history: Option<&'a History>,
    /// `None`: another daemon shows notifications, no switch.
    pub dnd: Option<bool>,
    pub media: Option<&'a Media>,
    pub now: u64,
}

/// Everything about the popover but its surface.
pub struct Ui {
    layout: Layout,
    /// Months away from this one.
    offset: i32,
    hover: Option<Target>,
    anim: Animator<Hit>,
    /// How far the notification list is scrolled, and how far it can be.
    scroll: f32,
    max_scroll: f32,
    /// Where things were drawn, for the pointer; the last match wins.
    hits: Vec<(Rect, Target)>,
    /// The apps of the groups drawn, for [`Target::ClearApp`].
    apps: Vec<String>,
}

pub struct Calendar {
    pub popup: Popup,
    scale: i32,
    configured: bool,
    ui: Ui,
    frame_pending: bool,
    dirty: bool,
}

impl Ui {
    fn new(layout: Layout) -> Self {
        Self {
            layout,
            offset: 0,
            hover: None,
            anim: Animator::new(),
            scroll: 0.0,
            max_scroll: 0.0,
            hits: Vec::new(),
            apps: Vec::new(),
        }
    }

    fn target_at(&self, point: (f64, f64)) -> Option<Target> {
        self.hits
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, target)| *target)
    }

    fn motion(&mut self, point: (f64, f64)) -> bool {
        let hover = self.target_at(point);
        std::mem::replace(&mut self.hover, hover) != hover
    }

    fn press(&mut self, point: (f64, f64)) -> Option<Target> {
        match self.target_at(point)? {
            Target::Month(Hit::Previous) => self.offset -= 1,
            Target::Month(Hit::Next) => self.offset += 1,
            Target::Month(Hit::Today) => self.offset = 0,
            other => return Some(other),
        }
        None
    }

    fn scroll(&mut self, point: (f64, f64), steps: i32) -> bool {
        if self.layout.history && (point.0 as f32) < self.layout.left() {
            let before = self.scroll;
            self.scroll = (self.scroll + steps as f32 * SCROLL_STEP).clamp(0.0, self.max_scroll);
            return self.scroll != before;
        }
        self.offset += steps.signum();
        steps != 0
    }
}

impl Calendar {
    /// Open under `anchor` (panel-surface coordinates of the clock).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        compositor: &CompositorState,
        xdg_shell: &XdgShell,
        qh: &QueueHandle<App>,
        parent: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
        seat: &WlSeat,
        serial: u32,
        anchor: (i32, i32, i32, i32),
        scale: i32,
        layout: Layout,
    ) -> Option<Self> {
        let positioner = positioner(xdg_shell, anchor, layout)?;
        let surface = compositor.create_surface(qh);
        let popup = Popup::from_surface(None, &positioner, qh, surface, xdg_shell)
            .map_err(|err| warn!(?err, "can't create calendar popup"))
            .ok()?;
        parent.get_popup(popup.xdg_popup());
        popup.xdg_popup().grab(seat, serial);
        popup.wl_surface().set_buffer_scale(scale);
        popup.wl_surface().commit();
        Some(Self {
            popup,
            scale,
            configured: false,
            ui: Ui::new(layout),
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

    /// The app of group `i`, as last drawn.
    pub fn app_name(&self, i: usize) -> Option<&str> {
        self.ui.apps.get(i).map(String::as_str)
    }

    /// Returns true if it needs a redraw.
    pub fn motion(&mut self, point: (f64, f64)) -> bool {
        self.ui.motion(point)
    }

    /// Paging the month is done here; what needs the app is returned.
    pub fn press(&mut self, point: (f64, f64)) -> Option<Target> {
        self.ui.press(point)
    }

    /// The wheel scrolls the notifications over them and pages through
    /// months over the month. Returns true if it needs a redraw.
    pub fn scroll(&mut self, point: (f64, f64), steps: i32) -> bool {
        self.ui.scroll(point, steps)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
        view: &View,
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let scale = self.scale;
        let layout = self.ui.layout;
        let (w, h) = (
            (layout.width() * scale as f32) as i32,
            (layout.height() * scale as f32) as i32,
        );
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate calendar buffer");
                return;
            }
        };
        let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
            return;
        };
        let mut p = Painter {
            canvas: &mut canvas,
            fonts,
            icons,
            theme,
        };
        self.ui.paint(&mut p, view);
        if self.ui.anim.running() {
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

impl Ui {
    /// The whole popover into `p`'s canvas, recording where things are.
    fn paint(&mut self, p: &mut Painter<'_, '_>, view: &View) {
        let theme = p.theme;
        let layout = self.layout;
        let today = Local::now().date_naive();
        self.hits.clear();
        p.canvas.clear(Color::rgba(0, 0, 0, 0.0));
        p.canvas.rounded_rect(
            0.0,
            0.0,
            layout.width(),
            layout.height(),
            theme.qs_radius,
            theme.qs_background,
        );
        if layout.history {
            paint_list(self, p, view);
        }
        let hover = match self.hover {
            Some(Target::Month(hit)) => Some(hit),
            _ => None,
        };
        let mut hits = Vec::new();
        paint_month(
            p,
            (layout.left(), if layout.media { MEDIA_SLOT } else { 0.0 }),
            today,
            self.offset,
            hover,
            &mut self.anim,
            first_weekday_is_sunday(),
            &mut hits,
        );
        self.hits
            .extend(hits.into_iter().map(|(r, hit)| (r, Target::Month(hit))));
        if layout.media
            && let Some(media) = view.media
        {
            paint_media(self, p, media, layout.left());
        }
    }
}

/// The month column alone.
fn month_height() -> f32 {
    PAD + HEADING + TITLE + WEEKDAYS + CELL * ROWS as f32 + PAD
}

/// Where the month title row starts, within the column.
fn title_y() -> f32 {
    PAD + HEADING
}

/// The arrow buttons within the column: (x, y, size).
fn arrow(hit: Hit) -> (f32, f32, f32) {
    let size = 32.0;
    let y = title_y() + (TITLE - size) / 2.0;
    match hit {
        Hit::Previous => (WIDTH - PAD - 2.0 * size - 4.0, y, size),
        _ => (WIDTH - PAD - size, y, size),
    }
}

/// Secondary text: the foreground faded toward the card (text ignores
/// alpha, so it mixes instead).
fn faded(theme: &Theme, t: f32) -> Color {
    let fg = theme.popup_text;
    fg.mix(theme.qs_background.with_alpha(fg.a), t)
}

/// The month `offset` months from `today`'s, as its first day.
fn month_start(today: NaiveDate, offset: i32) -> NaiveDate {
    let first = today.with_day(1).unwrap_or(today);
    let months = Months::new(offset.unsigned_abs());
    let shifted = if offset < 0 {
        first.checked_sub_months(months)
    } else {
        first.checked_add_months(months)
    };
    shifted.unwrap_or(first)
}

/// The 42 days shown for the month starting `first`: from the start of
/// its first week, a whole six weeks so the card never changes height.
fn grid(first: NaiveDate, sunday_first: bool) -> Vec<NaiveDate> {
    let back = if sunday_first {
        first.weekday().num_days_from_sunday()
    } else {
        first.weekday().num_days_from_monday()
    };
    let start = first - chrono::Duration::days(back as i64);
    start.iter_days().take(ROWS * 7).collect()
}

/// Whether weeks start on Sunday where the user is (as in the US, Canada,
/// Japan…), going by LC_TIME's region; elsewhere, Monday.
fn first_weekday_is_sunday() -> bool {
    let locale = ["LC_ALL", "LC_TIME", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    sunday_region(&locale)
}

fn sunday_region(locale: &str) -> bool {
    // "en_US.UTF-8" → "US".
    let region = locale
        .split(['.', '@'])
        .next()
        .and_then(|l| l.split('_').nth(1))
        .unwrap_or("");
    const SUNDAY: &[&str] = &[
        "US", "CA", "MX", "BR", "JP", "KR", "TW", "HK", "PH", "IL", "IN", "ZA", "AU", "SA", "PE",
        "CO", "VE", "GT", "PR",
    ];
    // No region at all (C, POSIX): the US convention, like `cal`.
    region.is_empty() || SUNDAY.contains(&region)
}

/// Draw the month column with its top-left at `origin`; `hits` gets the
/// places that react. Separate from the Wayland plumbing so tests can render it.
#[allow(clippy::too_many_arguments)]
fn paint_month(
    p: &mut Painter<'_, '_>,
    (ox, oy): (f32, f32),
    today: NaiveDate,
    offset: i32,
    hover: Option<Hit>,
    anim: &mut Animator<Hit>,
    sunday_first: bool,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let theme = p.theme;
    let s = p.canvas.scale();
    let fg = theme.popup_text;
    let (fonts, icons) = (&mut *p.fonts, &mut *p.icons);
    let canvas = &mut *p.canvas;
    anim.begin();

    // Today, in words.
    let mut weekday = fonts.layout(&today.format("%A").to_string(), 13.0, Weight::Normal, s);
    canvas.text(
        fonts,
        &mut weekday,
        ox + PAD + 4.0,
        oy + PAD,
        faded(theme, 0.35),
    );
    let mut date = fonts.layout(
        &today.format("%B %-d, %Y").to_string(),
        20.0,
        Weight::Bold,
        s,
    );
    canvas.text(
        fonts,
        &mut date,
        ox + PAD + 4.0,
        oy + PAD + weekday.height + 2.0,
        fg,
    );

    // The month card.
    let card_y = oy + title_y();
    canvas.rounded_rect(
        ox + PAD - 6.0,
        card_y,
        WIDTH - 2.0 * (PAD - 6.0),
        month_height() + oy - card_y - PAD + 6.0,
        theme.qs_tile_radius,
        theme.qs_tile,
    );
    let first = month_start(today, offset);
    let title_text = if first.year() == today.year() {
        first.format("%B").to_string()
    } else {
        first.format("%B %Y").to_string()
    };
    let mut title = fonts.layout(&title_text, 15.0, Weight::Bold, s);
    let title_x = ox + PAD + 8.0;
    let title_ty = (card_y + (TITLE - title.height) / 2.0).round();
    // Away from this month, the title is a "back to today" button.
    if offset != 0 {
        let lit = anim.get(
            Hit::Today,
            if hover == Some(Hit::Today) { 1.0 } else { 0.0 },
            SHORT,
        );
        let h = 28.0;
        canvas.rounded_rect(
            title_x - 8.0,
            card_y + (TITLE - h) / 2.0,
            title.width + 16.0,
            h,
            h / 2.0,
            theme
                .qs_toggle_off
                .with_alpha(theme.qs_toggle_off.a * (0.5 + lit)),
        );
    }
    canvas.text(fonts, &mut title, title_x, title_ty, fg);
    let (px, _, _) = arrow(Hit::Previous);
    hits.push((Rect::new(ox + PAD, card_y, px - PAD, TITLE), Hit::Today));
    for (which, icon) in [
        (Hit::Previous, "go-previous-symbolic"),
        (Hit::Next, "go-next-symbolic"),
    ] {
        let (ax, ay, size) = arrow(which);
        let (ax, ay) = (ax + ox, ay + oy);
        hits.push((Rect::new(ax, ay, size, size), which));
        let lit = anim.get(which, if hover == Some(which) { 1.0 } else { 0.0 }, SHORT);
        if lit > 0.0 {
            let c = theme.qs_toggle_off;
            canvas.circle(
                ax + size / 2.0,
                ay + size / 2.0,
                size / 2.0,
                c.with_alpha(c.a * lit),
            );
        }
        let i = 16.0;
        canvas.icon(
            icons,
            icon,
            ax + (size - i) / 2.0,
            ay + (size - i) / 2.0,
            i,
            fg,
        );
    }

    // Weekday initials, then the days.
    let cell_w = (WIDTH - 2.0 * PAD) / 7.0;
    let days = grid(first, sunday_first);
    let names_y = card_y + TITLE;
    for (column, day) in days.iter().take(7).enumerate() {
        let name = day.format("%a").to_string();
        let initial: String = name.chars().take(2).collect();
        let mut text = fonts.layout(&initial, 11.0, Weight::Bold, s);
        let x = ox + PAD + column as f32 * cell_w + (cell_w - text.width) / 2.0;
        let y = names_y + (WEEKDAYS - text.height) / 2.0;
        canvas.text(fonts, &mut text, x.round(), y.round(), faded(theme, 0.4));
    }
    let rows_y = names_y + WEEKDAYS;
    for (i, day) in days.iter().enumerate() {
        let (row, column) = (i / 7, i % 7);
        let cx = ox + PAD + column as f32 * cell_w + cell_w / 2.0;
        let cy = rows_y + row as f32 * CELL + CELL / 2.0;
        let is_today = *day == today;
        let in_month = day.month() == first.month();
        let color = if is_today {
            canvas.circle(cx, cy, 15.0, theme.accent);
            theme.on_accent
        } else if in_month {
            fg
        } else {
            faded(theme, 0.65)
        };
        let weight = if is_today {
            Weight::Bold
        } else {
            Weight::Normal
        };
        let mut text = fonts.layout(&day.day().to_string(), 13.0, weight, s);
        let x = cx - text.width / 2.0;
        let y = cy - text.height / 2.0;
        canvas.text(fonts, &mut text, x.round(), y.round(), color);
    }
}

/// One notification's tile.
fn item_height(entry: &Entry) -> f32 {
    let body = if entry.body.is_empty() { 0.0 } else { 34.0 };
    10.0 + 18.0 + body + 10.0
}

/// The list's height: groups of tiles under their app's heading.
fn content_height(groups: &[Group<'_>]) -> f32 {
    let mut y = 0.0;
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            y += GROUP_GAP;
        }
        y += GROUP_HEAD;
        for entry in &group.entries {
            y += item_height(entry) + ITEM_GAP;
        }
        y -= ITEM_GAP;
    }
    y
}

/// The left column: notifications by app, scrolled, then Do Not Disturb
/// and Clear all.
fn paint_list(cal: &mut Ui, p: &mut Painter<'_, '_>, view: &View) {
    let theme = p.theme;
    let layout = cal.layout;
    let scale = p.canvas.scale();
    let height = layout.height();
    let view_rect = Rect::new(PAD, 12.0, LIST_WIDTH - 2.0 * PAD, height - FOOTER - 12.0);
    let empty = History::default();
    let history = view.history.unwrap_or(&empty);
    let groups = history.groups();
    let content = content_height(&groups);
    cal.max_scroll = (content - view_rect.h).max(0.0);
    cal.scroll = cal.scroll.clamp(0.0, cal.max_scroll);
    cal.apps = groups.iter().map(|g| g.app.to_string()).collect();

    if groups.is_empty() {
        let cx = view_rect.x + view_rect.w / 2.0;
        let cy = view_rect.y + view_rect.h / 2.0;
        let dim = faded(theme, 0.55);
        p.icon("notifications-symbolic", cx, cy - 16.0, 44.0, dim);
        let w = p.measure("No notifications", 14.0, Weight::Bold);
        p.label(
            "No notifications",
            cx - w / 2.0,
            cy + 16.0,
            14.0,
            Weight::Bold,
            dim,
        );
    } else {
        // Drawn on a layer the size of the list so what scrolls past its
        // edges is cut off.
        let (pw, ph) = (
            (view_rect.w * scale).round() as u32,
            (view_rect.h * scale).round() as u32,
        );
        if let Some(mut layer) = Pixmap::new(pw, ph) {
            if let Some(mut canvas) = Canvas::new(layer.data_mut(), pw, ph, scale) {
                let mut lp = Painter {
                    canvas: &mut canvas,
                    fonts: &mut *p.fonts,
                    icons: &mut *p.icons,
                    theme,
                };
                paint_groups(cal, &mut lp, view, &groups, view_rect);
            }
            p.canvas.image(
                layer.as_ref(),
                view_rect.x,
                view_rect.y,
                view_rect.w,
                view_rect.h,
                0.0,
            );
        }
    }

    // The footer.
    let fy = height - FOOTER;
    let cy = fy + FOOTER / 2.0 - 2.0;
    let fg = theme.popup_text;
    if let Some(dnd) = view.dnd {
        let label = "Do Not Disturb";
        let w = p.label_v(label, PAD + 4.0, cy, 140.0, 13.0, Weight::Normal, fg);
        let on = if dnd { 1.0 } else { 0.0 };
        let sx = PAD + 4.0 + w + 10.0;
        let sy = cy - theme.switch_height / 2.0;
        let rect = p.switch(sx, sy, on);
        // The label is part of the switch.
        cal.hits.push((
            Rect::new(PAD, fy, rect.right() - PAD + 4.0, FOOTER - 4.0),
            Target::DoNotDisturb,
        ));
    }
    if !history.is_empty() {
        let label = "Clear all";
        let w = p.measure(label, 13.0, Weight::Normal);
        let rect = Rect::new(LIST_WIDTH - PAD - w - 24.0, cy - 15.0, w + 24.0, 30.0);
        let hovered = cal.hover == Some(Target::ClearAll);
        p.button(rect, if hovered { 1.0 } else { 0.0 });
        p.label_v(label, rect.x + 12.0, cy, w + 2.0, 13.0, Weight::Normal, fg);
        cal.hits.push((rect, Target::ClearAll));
    }
}

fn paint_groups(
    cal: &mut Ui,
    p: &mut Painter<'_, '_>,
    view: &View,
    groups: &[Group<'_>],
    vp: Rect,
) {
    let theme = p.theme;
    let fg = theme.popup_text;
    let dim = faded(theme, 0.4);
    let hover = cal.hover;
    // Local coordinates have the list's top-left at the origin; hits are
    // recorded in the popover's, cut to the list's edges.
    let record = |hits: &mut Vec<(Rect, Target)>, r: Rect, target: Target| {
        let top = (vp.y + r.y).max(vp.y);
        let bottom = (vp.y + r.y + r.h).min(vp.bottom());
        if bottom > top {
            hits.push((Rect::new(vp.x + r.x, top, r.w, bottom - top), target));
        }
    };
    let mut y = -cal.scroll;
    for (gi, group) in groups.iter().enumerate() {
        if gi > 0 {
            y += GROUP_GAP;
        }
        // The heading: icon, app, and a button to clear the lot.
        if y + GROUP_HEAD > 0.0 && y < vp.h {
            let icon = if group.icon.is_empty() {
                group.entries[0].desktop_entry.as_deref().unwrap_or("")
            } else {
                group.icon
            };
            let cy = y + GROUP_HEAD / 2.0;
            let shown = !icon.is_empty() && {
                let (c, i) = (&mut *p.canvas, &mut *p.icons);
                c.app_icon(i, icon, 4.0, cy - 8.0, 16.0)
            };
            // No icon at all: the name starts at the edge.
            let mut text_x = 28.0;
            if !shown {
                let (c, i) = (&mut *p.canvas, &mut *p.icons);
                if !c.icon(i, "notifications-symbolic", 4.0, cy - 7.0, 14.0, dim) {
                    text_x = 4.0;
                }
            }
            p.label_v(
                group.app,
                text_x,
                cy,
                vp.w - text_x - 30.0,
                12.0,
                Weight::Bold,
                dim,
            );
            let x = Rect::new(vp.w - 26.0, cy - 11.0, 22.0, 22.0);
            let lit = hover == Some(Target::ClearApp(gi));
            if lit {
                let c = theme.qs_toggle_off;
                p.canvas.circle(x.x + 11.0, cy, 11.0, c);
            }
            p.icon(
                "window-close-symbolic",
                x.x + 11.0,
                cy,
                12.0,
                if lit { fg } else { dim },
            );
            record(&mut cal.hits, x, Target::ClearApp(gi));
        }
        y += GROUP_HEAD;
        for entry in &group.entries {
            let h = item_height(entry);
            if y + h > 0.0 && y < vp.h {
                let lit =
                    matches!(hover, Some(Target::Item(s) | Target::Dismiss(s)) if s == entry.seq);
                let tile = if lit && entry.has_default() {
                    theme.qs_tile.mix(theme.qs_toggle_off, 0.5)
                } else {
                    theme.qs_tile
                };
                p.canvas
                    .rounded_rect(0.0, y, vp.w, h, theme.qs_tile_radius, tile);
                // Time, or the dismiss button over it.
                let right = vp.w - 10.0;
                let limit = if lit {
                    let x = Rect::new(vp.w - 32.0, y + 6.0, 22.0, 22.0);
                    let hot = hover == Some(Target::Dismiss(entry.seq));
                    if hot {
                        p.canvas
                            .circle(x.x + 11.0, x.y + 11.0, 11.0, theme.qs_toggle_off);
                    }
                    p.icon(
                        "window-close-symbolic",
                        x.x + 11.0,
                        x.y + 11.0,
                        12.0,
                        if hot { fg } else { dim },
                    );
                    x.x - 4.0
                } else {
                    let time = history::relative(view.now, entry.at);
                    let w = p.measure(&time, 11.0, Weight::Normal);
                    p.label(&time, right - w, y + 12.0, 11.0, Weight::Normal, dim);
                    right - w - 8.0
                };
                let row = y + 10.0 + 9.0;
                p.label_v(
                    &entry.summary,
                    12.0,
                    row,
                    limit - 12.0,
                    13.0,
                    Weight::Bold,
                    fg,
                );
                if !entry.body.is_empty() {
                    let lines = crate::polkit::wrap(p, &entry.body, vp.w - 24.0, 12.0, 2);
                    for (i, line) in lines.iter().enumerate() {
                        p.label(
                            line,
                            12.0,
                            y + 10.0 + 18.0 + 4.0 + i as f32 * 15.0,
                            12.0,
                            Weight::Normal,
                            faded(theme, 0.25),
                        );
                    }
                }
                record(
                    &mut cal.hits,
                    Rect::new(0.0, y, vp.w, h),
                    Target::Item(entry.seq),
                );
                // Recorded in this order the button wins over its tile.
                if lit {
                    record(
                        &mut cal.hits,
                        Rect::new(vp.w - 32.0, y + 6.0, 22.0, 22.0),
                        Target::Dismiss(entry.seq),
                    );
                }
            }
            y += h + ITEM_GAP;
        }
        y -= ITEM_GAP;
    }
}

/// The song above the month: art, title, artist, buttons, progress.
fn paint_media(cal: &mut Ui, p: &mut Painter<'_, '_>, media: &Media, ox: f32) {
    let Some(player) = media.current() else {
        return;
    };
    let theme = p.theme;
    let fg = theme.popup_text;
    let dim = faded(theme, 0.4);
    let card = Rect::new(
        ox + PAD - 6.0,
        MEDIA_TOP,
        WIDTH - 2.0 * (PAD - 6.0),
        MEDIA_CARD,
    );
    p.canvas.rounded_rect(
        card.x,
        card.y,
        card.w,
        card.h,
        theme.qs_tile_radius,
        theme.qs_tile,
    );
    // Art: a local PNG, else the player's own icon, else a note.
    let art = Rect::new(card.x + 8.0, card.y + 8.0, 56.0, 56.0);
    let shown = {
        let (c, i) = (&mut *p.canvas, &mut *p.icons);
        let from_file = player
            .art_path()
            .is_some_and(|path| c.app_icon(i, &path, art.x, art.y, art.w));
        from_file
            || (!player.desktop_entry.is_empty()
                && c.app_icon(i, &player.desktop_entry, art.x, art.y, art.w))
    };
    if !shown {
        p.canvas
            .rounded_rect(art.x, art.y, art.w, art.h, 10.0, theme.qs_toggle_off);
        p.icon(
            "audio-x-generic-symbolic",
            art.x + art.w / 2.0,
            art.y + art.h / 2.0,
            24.0,
            dim,
        );
    }
    let text_x = art.right() + 10.0;
    let right = card.right() - 10.0;
    // Dots to switch between players, top right.
    let live = media.live();
    let mut dots_w = 0.0;
    if live.len() > 1 {
        for (i, other) in live.iter().enumerate().rev() {
            let cx = right - 5.0 - dots_w;
            let cy = card.y + 14.0;
            let on = other.bus == player.bus;
            p.canvas.circle(
                cx,
                cy,
                if on { 3.5 } else { 3.0 },
                if on { fg } else { dim },
            );
            cal.hits
                .push((Rect::new(cx - 6.0, cy - 8.0, 12.0, 16.0), Target::Pick(i)));
            dots_w += 12.0;
        }
    }
    let title = if player.title.is_empty() {
        if player.identity.is_empty() {
            "Unknown"
        } else {
            &player.identity
        }
    } else {
        &player.title
    };
    p.label_v(
        title,
        text_x,
        card.y + 17.0,
        right - text_x - dots_w,
        13.0,
        Weight::Bold,
        fg,
    );
    let sub = if player.artist.is_empty() {
        player.identity.as_str()
    } else {
        player.artist.as_str()
    };
    p.label_v(
        sub,
        text_x,
        card.y + 34.0,
        right - text_x,
        12.0,
        Weight::Normal,
        dim,
    );
    // Previous, play/pause, next.
    let playing = player.status == Status::Playing;
    let buttons = [
        (
            Target::Previous,
            "media-skip-backward-symbolic",
            player.can_previous,
        ),
        (
            Target::PlayPause,
            if playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            },
            if playing {
                player.can_pause
            } else {
                player.can_play
            },
        ),
        (Target::Next, "media-skip-forward-symbolic", player.can_next),
    ];
    for (i, (target, icon, enabled)) in buttons.into_iter().enumerate() {
        let size = 26.0;
        let rect = Rect::new(text_x + i as f32 * (size + 4.0), card.y + 38.0, size, size);
        let (cx, cy) = (rect.x + size / 2.0, rect.y + size / 2.0);
        if enabled && cal.hover == Some(target) {
            p.canvas.circle(cx, cy, size / 2.0, theme.qs_toggle_off);
        }
        p.icon(
            icon,
            cx,
            cy,
            16.0,
            if enabled { fg } else { faded(theme, 0.7) },
        );
        if enabled {
            cal.hits.push((rect, target));
        }
    }
    // The song's progress.
    if let Some(progress) = player.progress() {
        let bar = Rect::new(card.x + 10.0, card.bottom() - 9.0, card.w - 20.0, 3.0);
        p.canvas
            .rounded_rect(bar.x, bar.y, bar.w, bar.h, 1.5, theme.qs_slider_track);
        if progress > 0.0 {
            p.canvas.rounded_rect(
                bar.x,
                bar.y,
                (bar.w * progress).max(3.0),
                bar.h,
                1.5,
                theme.qs_slider_fill,
            );
        }
    }
}

fn positioner(
    xdg_shell: &XdgShell,
    (ax, ay, aw, ah): (i32, i32, i32, i32),
    layout: Layout,
) -> Option<XdgPositioner> {
    let positioner = XdgPositioner::new(xdg_shell)
        .map_err(|err| warn!(?err, "no xdg positioner"))
        .ok()?;
    positioner.set_size(layout.width() as i32, layout.height() as i32);
    positioner.set_anchor_rect(ax, ay, aw.max(1), ah.max(1));
    // Hang centred below the clock.
    positioner.set_anchor(Anchor::Bottom);
    positioner.set_gravity(Gravity::Bottom);
    positioner.set_offset(0, 6);
    positioner
        .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
    Some(positioner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{history::Entry, media::Player};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn months_page_across_years() {
        let today = date(2026, 10, 18);
        assert_eq!(month_start(today, 0), date(2026, 10, 1));
        assert_eq!(month_start(today, 3), date(2027, 1, 1));
        assert_eq!(month_start(today, -10), date(2025, 12, 1));
    }

    #[test]
    fn grid_starts_on_the_first_weekday() {
        // 1 Oct 2026 is a Thursday.
        let sunday = grid(date(2026, 10, 1), true);
        assert_eq!(sunday.len(), 42);
        assert_eq!(sunday[0], date(2026, 9, 27));
        assert_eq!(sunday[4], date(2026, 10, 1));
        let monday = grid(date(2026, 10, 1), false);
        assert_eq!(monday[0], date(2026, 9, 28));
        assert_eq!(monday[3], date(2026, 10, 1));
    }

    #[test]
    fn week_start_follows_the_region() {
        assert!(sunday_region("en_US.UTF-8"));
        assert!(sunday_region("C"));
        assert!(!sunday_region("en_GB.UTF-8"));
        assert!(!sunday_region("de_DE@euro"));
    }

    #[test]
    fn layout_sizes() {
        let both = Layout {
            history: true,
            media: true,
        };
        let month = Layout {
            history: false,
            media: false,
        };
        assert_eq!(month.width(), WIDTH);
        assert_eq!(month.height(), month_height());
        assert_eq!(both.width(), WIDTH + LIST_WIDTH);
        assert_eq!(both.height(), month_height() + MEDIA_SLOT);
    }

    fn entry(id: u32, app: &str, summary: &str, body: &str) -> Entry {
        Entry {
            seq: 0,
            id,
            app_name: app.into(),
            icon: String::new(),
            summary: summary.into(),
            body: body.into(),
            at: 1_000,
            actions: vec![("default".into(), "Open".into())],
            desktop_entry: None,
        }
    }

    fn history() -> History {
        let mut h = History::default();
        h.push(
            entry(
                1,
                "Mail",
                "Hello",
                "A long message body that wraps across a couple of lines of the tile",
            ),
            false,
        );
        h.push(entry(2, "Chat", "Ping", ""), false);
        h.push(entry(3, "Mail", "Second", "short"), false);
        h
    }

    /// Draws every part into one popover-sized buffer.
    fn render(scale: f32, light: bool) -> (Vec<u8>, u32, u32, Vec<(Rect, Target)>) {
        let theme = if light { Theme::light() } else { Theme::dark() };
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let layout = Layout {
            history: true,
            media: true,
        };
        let (w, h) = (
            (layout.width() * scale) as u32,
            (layout.height() * scale) as u32,
        );
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let hits = {
            let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
            canvas.rounded_rect(
                0.0,
                0.0,
                layout.width(),
                layout.height(),
                theme.qs_radius,
                theme.qs_background,
            );
            let hist = history();
            let mut media = Media::default();
            media.players = vec![
                Player {
                    bus: "a".into(),
                    identity: "Player".into(),
                    title: "A Fairly Long Song Title".into(),
                    artist: "Some Artist".into(),
                    status: crate::media::Status::Playing,
                    length: Some(200),
                    position: 80,
                    can_next: true,
                    can_previous: true,
                    can_pause: true,
                    can_play: true,
                    ..Player::default()
                },
                Player {
                    bus: "b".into(),
                    status: crate::media::Status::Paused,
                    ..Player::default()
                },
            ];
            let view = View {
                history: Some(&hist),
                dnd: Some(false),
                media: Some(&media),
                now: 1_000 + 300,
            };
            // Not on a Wayland surface: the pieces drawn directly.
            let mut cal = Ui::new(layout);
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut fonts,
                icons: &mut icons,
                theme: &theme,
            };
            paint_list(&mut cal, &mut p, &view);
            let mut month_hits = Vec::new();
            paint_month(
                &mut p,
                (layout.left(), MEDIA_SLOT),
                date(2026, 10, 18),
                0,
                None,
                &mut Animator::new(),
                true,
                &mut month_hits,
            );
            paint_media(&mut cal, &mut p, &media, layout.left());
            cal.hits
                .extend(month_hits.into_iter().map(|(r, h)| (r, Target::Month(h))));
            cal.hits
        };
        (pixels, w, h, hits)
    }

    #[test]
    fn paints_and_records_hits() {
        let (pixels, _, _, hits) = render(1.0, false);
        assert!(pixels.iter().any(|&b| b != 0));
        let has = |f: &dyn Fn(&Target) -> bool| hits.iter().any(|(_, t)| f(t));
        assert!(has(&|t| matches!(t, Target::Item(_))));
        assert!(has(&|t| matches!(t, Target::ClearApp(0))));
        assert!(has(&|t| matches!(t, Target::ClearAll)));
        assert!(has(&|t| matches!(t, Target::DoNotDisturb)));
        assert!(has(&|t| matches!(t, Target::PlayPause)));
        assert!(has(&|t| matches!(t, Target::Pick(1))));
        assert!(has(&|t| matches!(t, Target::Month(Hit::Next))));
    }

    /// CNDE_CAL_PREVIEW=file cargo test -p cnde-shell calendar::tests::preview
    /// -- --ignored writes a 2x render ("WxH\n" + RGBA); CNDE_CAL_LIGHT uses
    /// the light theme.
    #[test]
    #[ignore]
    fn preview() {
        let Some(path) = std::env::var_os("CNDE_CAL_PREVIEW") else {
            return;
        };
        let (pixels, w, h, _) = render(2.0, std::env::var_os("CNDE_CAL_LIGHT").is_some());
        let mut data = format!("{w}x{h}\n").into_bytes();
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }
}
