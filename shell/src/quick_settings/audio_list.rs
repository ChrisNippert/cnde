//! The audio device lists that drop down under the Output and Input
//! sliders: every device with its ports, the default ticked. Opened by the
//! arrow beside each slider; the card makes room as the list grows.

use cnde_ui::{Canvas, Color, Fonts, Icons, Theme, Weight};

use super::{Rect, fit};
use crate::{
    Status,
    audio::{Device, Kind},
};

const DEVICE_ROW: f32 = 44.0;
const PORT_ROW: f32 = 34.0;
/// Room above the first and below the last row of the list.
const LIST_INSET: f32 = 4.0;
/// How much later each row starts to show than the one above it, and how
/// far (logical px) a row slides in.
const STAGGER: f32 = 0.05;
const SLIDE: f32 = 6.0;

/// A line of the list: a device, or one of its ports (indices into the
/// status's devices of the list's kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Device(usize),
    Port(usize, usize),
}

/// A list's lines and how far open it is (0 closed, 1 open).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListState {
    pub rows: Vec<Row>,
    pub progress: f32,
}

/// The lines for `devices`: a device with several ports gets a sub-line
/// for each.
pub fn rows(devices: &[Device]) -> Vec<Row> {
    devices
        .iter()
        .enumerate()
        .flat_map(|(i, d)| {
            let ports = if d.ports.len() > 1 { d.ports.len() } else { 0 };
            std::iter::once(Row::Device(i)).chain((0..ports).map(move |j| Row::Port(i, j)))
        })
        .collect()
}

/// The height of the list with every line showing.
pub fn full_height(rows: &[Row]) -> f32 {
    let lines: f32 = rows
        .iter()
        .map(|row| match row {
            Row::Device(_) => DEVICE_ROW,
            Row::Port(..) => PORT_ROW,
        })
        .sum();
    // Room for a line saying there's nothing to list.
    let empty = if rows.is_empty() { DEVICE_ROW } else { 0.0 };
    2.0 * LIST_INSET + lines + empty
}

/// Where a list goes, as far open as it is.
#[derive(Debug, Clone, PartialEq)]
pub struct ListLayout {
    /// The part showing so far.
    pub list: Rect,
    pub progress: f32,
    /// Every line at its open place (only those inside `list` show).
    pub rows: Vec<(Rect, Row)>,
}

/// The list of `state` hanging from (`x`, `y`), `w` wide.
pub fn place(state: &ListState, x: f32, y: f32, w: f32) -> ListLayout {
    let progress = state.progress.clamp(0.0, 1.0);
    let mut row_y = y + LIST_INSET;
    let rows = state
        .rows
        .iter()
        .map(|&row| {
            let h = match row {
                Row::Device(_) => DEVICE_ROW,
                Row::Port(..) => PORT_ROW,
            };
            let rect = Rect { x, y: row_y, w, h };
            row_y += h;
            (rect, row)
        })
        .collect();
    ListLayout {
        list: Rect {
            x,
            y,
            w,
            h: full_height(&state.rows) * progress,
        },
        progress,
        rows,
    }
}

impl ListLayout {
    /// The line at `point`, if it's inside the part showing.
    pub fn hit(&self, point: (f64, f64)) -> Option<Row> {
        if !self.list.contains(point) {
            return None;
        }
        self.rows
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, row)| *row)
    }
}

/// What a press on `row` asks for, if anything: picking the default
/// device does nothing.
pub fn row_action(kind: Kind, devices: &[Device], row: Row) -> Option<crate::actions::Action> {
    use crate::actions::Action;
    match row {
        Row::Device(i) => {
            let device = devices.get(i).filter(|d| !d.default)?;
            Some(Action::SelectDevice(kind, device.name.clone()))
        }
        Row::Port(i, j) => {
            let device = devices.get(i)?;
            let port = device.ports.get(j)?;
            if device.default && device.active_port.as_deref() == Some(&port.name) {
                return None;
            }
            Some(Action::SelectPort(
                kind,
                device.name.clone(),
                port.name.clone(),
            ))
        }
    }
}

/// How far along row `n` of the list is, 0-1: later rows start a little
/// after the ones above.
pub(super) fn row_progress(progress: f32, n: usize) -> f32 {
    let delay = STAGGER * n.min(6) as f32;
    ((progress - delay) / (1.0 - STAGGER * 6.0)).clamp(0.0, 1.0)
}

/// Draw the list: the rows fade and slide in as it opens, and show only
/// inside the part that's open so far.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    canvas: &mut Canvas<'_>,
    page: &ListLayout,
    kind: Kind,
    status: &Status,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    s: f32,
) {
    let devices = status.audio.devices(kind);
    let l = page.list;
    if l.h <= 0.5 {
        return;
    }
    canvas.rounded_rect(
        l.x,
        l.y,
        l.w,
        l.h,
        theme.qs_tile_radius,
        theme.qs_toggle_off,
    );
    let clip = (l.x, l.y, l.w, l.h);
    if page.rows.is_empty() {
        let mut text = fonts.layout("No devices", theme.font_size, Weight::Normal, s);
        let ty = (l.y + (LIST_INSET + DEVICE_ROW - text.height) / 2.0).round();
        let tx = (l.x + (l.w - text.width) / 2.0).round();
        let t = row_progress(page.progress, 0);
        let dim = theme.qs_dim.with_alpha(theme.qs_dim.a * t);
        canvas.text_clipped(fonts, &mut text, tx, ty, dim, clip);
    }
    let icon_size = 18.0;
    let check = |canvas: &mut Canvas<'_>, icons: &mut Icons, rect: Rect, color: Color| {
        canvas.icon_clipped(
            icons,
            "object-select-symbolic",
            rect.x + rect.w - 12.0 - 16.0,
            rect.y + (rect.h - 16.0) / 2.0,
            16.0,
            color,
            clip,
        );
    };
    for (n, &(rect, row)) in page.rows.iter().enumerate() {
        let t = row_progress(page.progress, n);
        if t <= 0.0 {
            continue;
        }
        let fade = |c: Color| c.with_alpha(c.a * t);
        let rect = Rect {
            y: rect.y - (1.0 - t) * SLIDE,
            ..rect
        };
        match row {
            Row::Device(i) => {
                let Some(device) = devices.get(i) else {
                    continue;
                };
                if n > 0 && matches!(page.rows[n - 1].1, Row::Device(_)) {
                    canvas.rounded_rect_clipped(
                        rect.x + 12.0,
                        rect.y,
                        rect.w - 24.0,
                        1.0,
                        0.0,
                        fade(theme.card_separator),
                        clip,
                    );
                }
                let x = rect.x + 12.0;
                canvas.icon_clipped(
                    icons,
                    device.icon,
                    x,
                    rect.y + (rect.h - icon_size) / 2.0,
                    icon_size,
                    fade(theme.popup_text),
                    clip,
                );
                let text_x = x + icon_size + 10.0;
                let room = rect.x + rect.w - 12.0 - 16.0 - 8.0 - text_x;
                let mut text = fit(
                    fonts,
                    &device.description,
                    theme.font_size,
                    Weight::Normal,
                    room,
                    s,
                );
                let ty = (rect.y + (rect.h - text.height) / 2.0).round();
                canvas.text_clipped(
                    fonts,
                    &mut text,
                    text_x.round(),
                    ty,
                    fade(theme.popup_text),
                    clip,
                );
                if device.default {
                    check(canvas, icons, rect, fade(theme.qs_toggle_on));
                }
            }
            Row::Port(i, j) => {
                let Some((device, port)) =
                    devices.get(i).and_then(|d| d.ports.get(j).map(|p| (d, p)))
                else {
                    continue;
                };
                let text_x = rect.x + 12.0 + icon_size + 10.0 + 12.0;
                let room = rect.x + rect.w - 12.0 - 16.0 - 8.0 - text_x;
                let active = device.active_port.as_deref() == Some(&port.name);
                let color = if active && device.default {
                    theme.popup_text
                } else {
                    theme.qs_dim
                };
                let mut text = fit(
                    fonts,
                    &port.description,
                    theme.qs_label_size,
                    Weight::Normal,
                    room,
                    s,
                );
                let ty = (rect.y + (rect.h - text.height) / 2.0).round();
                canvas.text_clipped(fonts, &mut text, text_x.round(), ty, fade(color), clip);
                if active && device.default {
                    check(canvas, icons, rect, fade(theme.qs_toggle_on));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{Device, Port};

    fn device(name: &str, ports: usize, default: bool) -> Device {
        Device {
            index: 1,
            name: name.into(),
            description: name.into(),
            icon: "audio-speakers-symbolic",
            percent: 50,
            muted: false,
            default,
            ports: (0..ports)
                .map(|i| Port {
                    name: format!("p{i}"),
                    description: format!("Port {i}"),
                })
                .collect(),
            active_port: Some("p0".into()),
            monitor: false,
        }
    }

    #[test]
    fn ports_are_sub_lines_only_when_there_is_a_choice() {
        let devices = [
            device("a", 2, true),
            device("b", 1, false),
            device("c", 0, false),
        ];
        assert_eq!(
            rows(&devices),
            [
                Row::Device(0),
                Row::Port(0, 0),
                Row::Port(0, 1),
                Row::Device(1),
                Row::Device(2),
            ]
        );
    }

    #[test]
    fn list_height_follows_progress() {
        let devices = [device("a", 2, true), device("b", 0, false)];
        let rows = rows(&devices);
        let full = full_height(&rows);
        assert_eq!(full, 2.0 * LIST_INSET + 2.0 * DEVICE_ROW + 2.0 * PORT_ROW);
        let at = |progress| {
            let state = ListState {
                rows: rows.clone(),
                progress,
            };
            place(&state, 10.0, 100.0, 300.0)
        };
        assert_eq!(at(0.0).list.h, 0.0);
        assert_eq!(at(0.5).list.h, full / 2.0);
        assert_eq!(at(1.0).list.h, full);
        // Rows stack inside the open list.
        let open = at(1.0);
        let last = open.rows.last().unwrap().0;
        assert_eq!(last.y + last.h + LIST_INSET, open.list.y + open.list.h);
        // An empty list still has room for its message.
        assert!(full_height(&[]) > 2.0 * LIST_INSET);
    }

    #[test]
    fn only_the_open_part_takes_clicks() {
        let devices = [device("a", 2, true), device("b", 0, false)];
        let at = |progress| {
            let state = ListState {
                rows: rows(&devices),
                progress,
            };
            place(&state, 10.0, 100.0, 300.0)
        };
        let centre = |r: Rect| ((r.x + r.w / 2.0) as f64, (r.y + r.h / 2.0) as f64);
        let open = at(1.0);
        assert_eq!(open.hit(centre(open.rows[2].0)), Some(Row::Port(0, 1)));
        assert_eq!(open.hit(centre(open.rows[3].0)), Some(Row::Device(1)));
        // Half open: the last row isn't showing yet, so it isn't there.
        let half = at(0.5);
        assert_eq!(half.hit(centre(half.rows[3].0)), None);
        assert_eq!(half.hit(centre(half.rows[0].0)), Some(Row::Device(0)));
        assert_eq!(at(0.0).hit((20.0, 100.0)), None);
    }

    #[test]
    fn rows_start_one_after_another() {
        assert_eq!(row_progress(0.0, 0), 0.0);
        assert_eq!(row_progress(1.0, 6), 1.0);
        assert!(row_progress(0.3, 0) > row_progress(0.3, 3));
    }

    #[test]
    fn picking_the_default_changes_nothing() {
        use crate::actions::Action;
        let devices = [device("a", 2, true), device("b", 0, false)];
        assert_eq!(row_action(Kind::Output, &devices, Row::Device(0)), None);
        assert_eq!(
            row_action(Kind::Output, &devices, Row::Device(1)),
            Some(Action::SelectDevice(Kind::Output, "b".into()))
        );
        assert_eq!(row_action(Kind::Output, &devices, Row::Port(0, 0)), None);
        assert_eq!(
            row_action(Kind::Output, &devices, Row::Port(0, 1)),
            Some(Action::SelectPort(Kind::Output, "a".into(), "p1".into()))
        );
    }
}
