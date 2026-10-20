//! The card's audio pages: every output (or input) with its ports, the
//! default ticked, and that device's volume on top. Opened by the arrow
//! beside the Output and Input sliders; a back button returns to the card.

use cnde_ui::{Animator, Canvas, Color, Fonts, Icons, Theme, Weight};

use super::{Moving, Rect, SLIDER_HEIGHT, Slider, fit, paint_volume_row};
use crate::{
    Status,
    audio::{Device, Kind},
};

const DEVICE_ROW: f32 = 44.0;
const PORT_ROW: f32 = 34.0;
/// Room above the first and below the last row of the list.
const LIST_INSET: f32 = 4.0;

/// A line of the list: a device, or one of its ports (indices into the
/// status's devices of the page's kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Device(usize),
    Port(usize, usize),
}

/// What a point on the page does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageHit {
    Back,
    Mute,
    Slider,
    Row(Row),
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

#[derive(Debug, Clone, PartialEq)]
pub struct PageLayout {
    pub width: f32,
    pub height: f32,
    pub back: Rect,
    pub slider: Rect,
    pub list: Rect,
    pub rows: Vec<(Rect, Row)>,
}

pub fn layout(theme: &Theme, rows: &[Row]) -> PageLayout {
    let pad = theme.qs_padding;
    let inner = theme.qs_width - 2.0 * pad;
    let mut y = pad;
    let back = Rect {
        x: pad,
        y,
        w: SLIDER_HEIGHT,
        h: SLIDER_HEIGHT,
    };
    y += SLIDER_HEIGHT + theme.qs_gap;
    let slider = Rect {
        x: pad,
        y,
        w: inner,
        h: SLIDER_HEIGHT,
    };
    y += SLIDER_HEIGHT + theme.qs_gap;
    let top = y;
    y += LIST_INSET;
    let mut placed = Vec::new();
    for &row in rows {
        let h = if matches!(row, Row::Device(_)) {
            DEVICE_ROW
        } else {
            PORT_ROW
        };
        placed.push((
            Rect {
                x: pad,
                y,
                w: inner,
                h,
            },
            row,
        ));
        y += h;
    }
    // Room for a line saying there's nothing to list.
    if rows.is_empty() {
        y += DEVICE_ROW;
    }
    y += LIST_INSET;
    let list = Rect {
        x: pad,
        y: top,
        w: inner,
        h: y - top,
    };
    PageLayout {
        width: theme.qs_width,
        height: y + pad,
        back,
        slider,
        list,
        rows: placed,
    }
}

impl PageLayout {
    pub fn hit(&self, point: (f64, f64)) -> Option<PageHit> {
        if self.back.contains(point) {
            return Some(PageHit::Back);
        }
        if self.slider.contains(point) {
            let icon_zone = Rect {
                w: self.slider.h,
                ..self.slider
            };
            return Some(if icon_zone.contains(point) {
                PageHit::Mute
            } else {
                PageHit::Slider
            });
        }
        self.rows
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, row)| PageHit::Row(*row))
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

#[allow(clippy::too_many_arguments)]
pub fn paint(
    canvas: &mut Canvas<'_>,
    page: &PageLayout,
    kind: Kind,
    // (percent, muted) of the default device, as the slider should show it.
    volume: (u32, bool),
    status: &Status,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    s: f32,
    anim: &mut Animator<Moving>,
    dragging: Option<Slider>,
) {
    anim.begin();
    let slider = match kind {
        Kind::Output => Slider::Volume,
        Kind::Input => Slider::Input,
    };
    let target = volume.0 as f32 / 100.0;
    let shown = if dragging == Some(slider) {
        anim.set(Moving::Slider(slider), target)
    } else {
        anim.get(Moving::Slider(slider), target, cnde_ui::anim::SHORT)
    };
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    canvas.rounded_rect(
        0.0,
        0.0,
        page.width,
        page.height,
        theme.qs_radius,
        theme.qs_background,
    );

    // The back button and the title.
    let b = page.back;
    canvas.rounded_rect(b.x, b.y, b.w, b.h, b.h / 2.0, theme.qs_slider_track);
    canvas.icon(
        icons,
        "go-previous-symbolic",
        b.x + (b.w - 16.0) / 2.0,
        b.y + (b.h - 16.0) / 2.0,
        16.0,
        theme.popup_text,
    );
    let title = match kind {
        Kind::Output => "Output",
        Kind::Input => "Input",
    };
    let mut text = fonts.layout(title, 15.0, Weight::Bold, s);
    let ty = (b.y + (b.h - text.height) / 2.0).round();
    canvas.text(fonts, &mut text, b.x + b.w + 12.0, ty, theme.popup_text);

    let devices = status.audio.devices(kind);
    let icon = match (kind, volume.1) {
        (Kind::Input, true) => "microphone-sensitivity-muted-symbolic",
        (Kind::Input, false) => "audio-input-microphone-symbolic",
        (Kind::Output, muted) => crate::panel::volume_icon(crate::status::Volume {
            percent: volume.0,
            muted,
        }),
    };
    paint_volume_row(canvas, icons, theme, page.slider, shown, volume.1, icon);

    // The list.
    let l = page.list;
    canvas.rounded_rect(
        l.x,
        l.y,
        l.w,
        l.h,
        theme.qs_tile_radius,
        theme.qs_toggle_off,
    );
    if page.rows.is_empty() {
        let mut text = fonts.layout("No devices", theme.font_size, Weight::Normal, s);
        let ty = (l.y + (l.h - text.height) / 2.0).round();
        let tx = (l.x + (l.w - text.width) / 2.0).round();
        canvas.text(fonts, &mut text, tx, ty, theme.qs_dim);
    }
    let icon_size = 18.0;
    let check = |canvas: &mut Canvas<'_>, icons: &mut Icons, rect: Rect| {
        canvas.icon(
            icons,
            "object-select-symbolic",
            rect.x + rect.w - 12.0 - 16.0,
            rect.y + (rect.h - 16.0) / 2.0,
            16.0,
            theme.qs_toggle_on,
        );
    };
    for (n, &(rect, row)) in page.rows.iter().enumerate() {
        match row {
            Row::Device(i) => {
                let Some(device) = devices.get(i) else {
                    continue;
                };
                if n > 0 && matches!(page.rows[n - 1].1, Row::Device(_)) {
                    canvas.rounded_rect(
                        rect.x + 12.0,
                        rect.y,
                        rect.w - 24.0,
                        1.0,
                        0.0,
                        theme.card_separator,
                    );
                }
                let x = rect.x + 12.0;
                canvas.icon(
                    icons,
                    device.icon,
                    x,
                    rect.y + (rect.h - icon_size) / 2.0,
                    icon_size,
                    theme.popup_text,
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
                canvas.text(fonts, &mut text, text_x.round(), ty, theme.popup_text);
                if device.default {
                    check(canvas, icons, rect);
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
                canvas.text(fonts, &mut text, text_x.round(), ty, color);
                if active && device.default {
                    check(canvas, icons, rect);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{Device, Port};
    use cnde_ui::Theme;

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
    fn rows_stack_inside_the_page() {
        let theme = Theme::light();
        let devices = [device("a", 2, true), device("b", 0, false)];
        let page = layout(&theme, &rows(&devices));
        let last = page.rows.last().unwrap().0;
        assert!(last.y + last.h < page.list.y + page.list.h);
        assert!(page.list.y + page.list.h < page.height);
        let centre = |r: Rect| ((r.x + r.w / 2.0) as f64, (r.y + r.h / 2.0) as f64);
        assert_eq!(page.hit(centre(page.back)), Some(PageHit::Back));
        assert_eq!(page.hit(centre(page.slider)), Some(PageHit::Slider));
        assert_eq!(
            page.hit((page.slider.x as f64 + 4.0, centre(page.slider).1)),
            Some(PageHit::Mute)
        );
        assert_eq!(
            page.hit(centre(page.rows[2].0)),
            Some(PageHit::Row(Row::Port(0, 1)))
        );
        // An empty list still has room for its message.
        assert!(layout(&theme, &[]).list.h > 2.0 * LIST_INSET);
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
