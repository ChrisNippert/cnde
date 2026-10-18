//! The calendar under the clock: today's date and a month grid to page
//! through. An xdg_popup of the panel like quick settings, so a click
//! anywhere else closes it.

use chrono::{Datelike, Local, Months, NaiveDate};
use cnde_ui::{Animator, Canvas, Color, Fonts, Icons, Theme, Weight, anim::SHORT};
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

use crate::App;

const WIDTH: f32 = 316.0;
const PAD: f32 = 16.0;
/// The date heading above the month.
const HEADING: f32 = 58.0;
/// The month name and its arrows.
const TITLE: f32 = 40.0;
const WEEKDAYS: f32 = 26.0;
const CELL: f32 = 38.0;
const ROWS: usize = 6;

/// What a point in the card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    Previous,
    Next,
    /// The month name: back to this month.
    Today,
}

pub struct Calendar {
    pub popup: Popup,
    scale: i32,
    configured: bool,
    /// Months away from this one.
    offset: i32,
    hover: Option<Hit>,
    anim: Animator<Hit>,
    frame_pending: bool,
    dirty: bool,
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
    ) -> Option<Self> {
        let positioner = positioner(xdg_shell, anchor)?;
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
            offset: 0,
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
    pub fn motion(&mut self, point: (f64, f64)) -> bool {
        let hover = hit(point);
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// Returns true if it needs a redraw.
    pub fn press(&mut self, point: (f64, f64)) -> bool {
        match hit(point) {
            Some(Hit::Previous) => self.offset -= 1,
            Some(Hit::Next) => self.offset += 1,
            Some(Hit::Today) if self.offset != 0 => self.offset = 0,
            _ => return false,
        }
        true
    }

    /// The wheel pages through months. Returns true if it needs a redraw.
    pub fn scroll(&mut self, steps: i32) -> bool {
        self.offset += steps.signum();
        steps != 0
    }

    pub fn draw(
        &mut self,
        qh: &QueueHandle<App>,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let scale = self.scale;
        let (w, h) = (
            (WIDTH * scale as f32) as i32,
            (height() * scale as f32) as i32,
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
        let today = Local::now().date_naive();
        paint(
            &mut canvas,
            fonts,
            icons,
            theme,
            today,
            self.offset,
            self.hover,
            &mut self.anim,
            first_weekday_is_sunday(),
        );
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

fn height() -> f32 {
    PAD + HEADING + TITLE + WEEKDAYS + CELL * ROWS as f32 + PAD
}

/// Where the month title row starts.
fn title_y() -> f32 {
    PAD + HEADING
}

/// The arrow buttons: (x, y, size).
fn arrow(hit: Hit) -> (f32, f32, f32) {
    let size = 32.0;
    let y = title_y() + (TITLE - size) / 2.0;
    match hit {
        Hit::Previous => (WIDTH - PAD - 2.0 * size - 4.0, y, size),
        _ => (WIDTH - PAD - size, y, size),
    }
}

fn hit((x, y): (f64, f64)) -> Option<Hit> {
    let (x, y) = (x as f32, y as f32);
    for which in [Hit::Previous, Hit::Next] {
        let (ax, ay, size) = arrow(which);
        if x >= ax && x < ax + size && y >= ay && y < ay + size {
            return Some(which);
        }
    }
    let (px, _, _) = arrow(Hit::Previous);
    let t = title_y();
    (x >= PAD && x < px && y >= t && y < t + TITLE).then_some(Hit::Today)
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

/// Draw the card. Separate from the Wayland plumbing so tests can render it.
#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    today: NaiveDate,
    offset: i32,
    hover: Option<Hit>,
    anim: &mut Animator<Hit>,
    sunday_first: bool,
) {
    let s = canvas.scale();
    let fg = theme.popup_text;
    // Text ignores alpha: fade toward the card instead.
    let faded = |t: f32| fg.mix(theme.qs_background.with_alpha(fg.a), t);
    anim.begin();
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    canvas.rounded_rect(
        0.0,
        0.0,
        WIDTH,
        height(),
        theme.qs_radius,
        theme.qs_background,
    );

    // Today, in words.
    let mut weekday = fonts.layout(
        &today.format("%A").to_string(),
        13.0,
        Weight::Normal,
        s,
    );
    canvas.text(fonts, &mut weekday, PAD + 4.0, PAD, faded(0.35));
    let mut date = fonts.layout(
        &today.format("%B %-d, %Y").to_string(),
        20.0,
        Weight::Bold,
        s,
    );
    canvas.text(fonts, &mut date, PAD + 4.0, PAD + weekday.height + 2.0, fg);

    // The month card.
    let card_y = title_y();
    canvas.rounded_rect(
        PAD - 6.0,
        card_y,
        WIDTH - 2.0 * (PAD - 6.0),
        height() - card_y - PAD + 6.0,
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
    let title_x = PAD + 8.0;
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
            theme.qs_toggle_off.with_alpha(theme.qs_toggle_off.a * (0.5 + lit)),
        );
    }
    canvas.text(fonts, &mut title, title_x, title_ty, fg);
    for (which, icon) in [
        (Hit::Previous, "go-previous-symbolic"),
        (Hit::Next, "go-next-symbolic"),
    ] {
        let (ax, ay, size) = arrow(which);
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
        canvas.icon(icons, icon, ax + (size - i) / 2.0, ay + (size - i) / 2.0, i, fg);
    }

    // Weekday initials, then the days.
    let cell_w = (WIDTH - 2.0 * PAD) / 7.0;
    let days = grid(first, sunday_first);
    let names_y = card_y + TITLE;
    for (column, day) in days.iter().take(7).enumerate() {
        let name = day.format("%a").to_string();
        let initial: String = name.chars().take(2).collect();
        let mut text = fonts.layout(&initial, 11.0, Weight::Bold, s);
        let x = PAD + column as f32 * cell_w + (cell_w - text.width) / 2.0;
        let y = names_y + (WEEKDAYS - text.height) / 2.0;
        canvas.text(fonts, &mut text, x.round(), y.round(), faded(0.4));
    }
    let rows_y = names_y + WEEKDAYS;
    for (i, day) in days.iter().enumerate() {
        let (row, column) = (i / 7, i % 7);
        let cx = PAD + column as f32 * cell_w + cell_w / 2.0;
        let cy = rows_y + row as f32 * CELL + CELL / 2.0;
        let is_today = *day == today;
        let in_month = day.month() == first.month();
        let color = if is_today {
            canvas.circle(cx, cy, 15.0, theme.accent);
            theme.on_accent
        } else if in_month {
            fg
        } else {
            faded(0.65)
        };
        let weight = if is_today { Weight::Bold } else { Weight::Normal };
        let mut text = fonts.layout(&day.day().to_string(), 13.0, weight, s);
        let x = cx - text.width / 2.0;
        let y = cy - text.height / 2.0;
        canvas.text(fonts, &mut text, x.round(), y.round(), color);
    }
}

fn positioner(
    xdg_shell: &XdgShell,
    (ax, ay, aw, ah): (i32, i32, i32, i32),
) -> Option<XdgPositioner> {
    let positioner = XdgPositioner::new(xdg_shell)
        .map_err(|err| warn!(?err, "no xdg positioner"))
        .ok()?;
    positioner.set_size(WIDTH as i32, height() as i32);
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
    fn hits() {
        let (x, y, size) = arrow(Hit::Next);
        let mid = ((x + size / 2.0) as f64, (y + size / 2.0) as f64);
        assert_eq!(hit(mid), Some(Hit::Next));
        let (x, y, size) = arrow(Hit::Previous);
        let mid = ((x + size / 2.0) as f64, (y + size / 2.0) as f64);
        assert_eq!(hit(mid), Some(Hit::Previous));
        assert_eq!(hit(((PAD + 20.0) as f64, (title_y() + 20.0) as f64)), Some(Hit::Today));
        assert_eq!(hit((5.0, 5.0)), None);
    }

    #[test]
    fn paints() {
        let theme = Theme::dark();
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let (w, h) = (WIDTH as u32, height() as u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let mut canvas = Canvas::new(&mut pixels, w, h, 1.0).unwrap();
        let mut anim = Animator::new();
        let today = date(2026, 10, 18);
        paint(&mut canvas, &mut fonts, &mut icons, &theme, today, 1, None, &mut anim, true);
        assert!(pixels.iter().any(|&b| b != 0));
    }

    /// CNDE_CAL_PREVIEW=file cargo test -p cnde-shell calendar::tests::preview
    /// -- --ignored writes a 2x render ("WxH\n" + RGBA); CNDE_CAL_OFFSET
    /// pages away from this month and CNDE_CAL_LIGHT uses the light theme.
    #[test]
    #[ignore]
    fn preview() {
        let Some(path) = std::env::var_os("CNDE_CAL_PREVIEW") else {
            return;
        };
        let theme = if std::env::var_os("CNDE_CAL_LIGHT").is_some() {
            Theme::light()
        } else {
            Theme::dark()
        };
        let offset = std::env::var("CNDE_CAL_OFFSET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let scale = 2.0;
        let (w, h) = ((WIDTH * scale) as u32, (height() * scale) as u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
            let hover = (offset != 0).then_some(Hit::Today);
            paint(
                &mut canvas,
                &mut fonts,
                &mut icons,
                &theme,
                date(2026, 10, 18),
                offset,
                hover,
                &mut Animator::new(),
                true,
            );
        }
        let mut data = format!("{w}x{h}\n").into_bytes();
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }
}
