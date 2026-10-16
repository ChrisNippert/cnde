//! Quick settings: the iOS-Control-Center-looking card under the status
//! icons (ARCHITECTURE.md §5.2). An xdg_popup of the panel, so the
//! compositor grabs input for it: clicking anywhere else closes it, and
//! it gets the keyboard (Escape closes it).

use std::time::{Duration, Instant};

use cnde_ui::{Canvas, Color, Fonts, Icons, Theme, Weight};
use smithay_client_toolkit::{
    compositor::CompositorState,
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
    App, Status,
    actions::{Action, Actions},
    panel::volume_icon,
};

/// What a point in the card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Volume,
    Mute,
    Wifi,
    Bluetooth,
    DarkMode,
    DoNotDisturb,
    Power,
    Suspend,
    Reboot,
    PowerOff,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn contains(&self, (px, py): (f64, f64)) -> bool {
        let (px, py) = (px as f32, py as f32);
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// Where everything goes, top to bottom. Pure, so it's testable.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub toggle_tile: Rect,
    /// (circle bounds, what it is), left to right.
    pub toggles: Vec<(Rect, Hit)>,
    pub volume: Rect,
    pub footer_y: f32,
    pub power_button: Rect,
    pub power_menu: Option<Vec<(Rect, Hit)>>,
}

const FOOTER_HEIGHT: f32 = 40.0;
const POWER_TILE_HEIGHT: f32 = 64.0;
const LABEL_GAP: f32 = 6.0;

pub fn layout(theme: &Theme, power_menu_open: bool) -> Layout {
    let width = theme.qs_width;
    let pad = theme.qs_padding;
    let inner = width - 2.0 * pad;
    let mut y = pad;

    // A tile of round toggles, labels underneath.
    let toggle = theme.qs_toggle_size;
    let tile_h = 14.0 + toggle + LABEL_GAP + theme.qs_label_size * 1.4 + 12.0;
    let toggle_tile = Rect {
        x: pad,
        y,
        w: inner,
        h: tile_h,
    };
    let kinds = [Hit::Wifi, Hit::Bluetooth, Hit::DarkMode, Hit::DoNotDisturb];
    let slot = inner / kinds.len() as f32;
    let toggles = kinds
        .iter()
        .enumerate()
        .map(|(i, &hit)| {
            let cx = pad + slot * (i as f32 + 0.5);
            (
                Rect {
                    x: cx - toggle / 2.0,
                    y: y + 14.0,
                    w: toggle,
                    h: toggle,
                },
                hit,
            )
        })
        .collect();
    y += tile_h + theme.qs_gap;

    let volume = Rect {
        x: pad,
        y,
        w: inner,
        h: theme.qs_slider_height,
    };
    y += theme.qs_slider_height + theme.qs_gap;

    let power_menu = power_menu_open.then(|| {
        let gap = theme.qs_gap;
        let tile_w = (inner - 2.0 * gap) / 3.0;
        let items = [Hit::Suspend, Hit::Reboot, Hit::PowerOff]
            .iter()
            .enumerate()
            .map(|(i, &hit)| {
                (
                    Rect {
                        x: pad + i as f32 * (tile_w + gap),
                        y,
                        w: tile_w,
                        h: POWER_TILE_HEIGHT,
                    },
                    hit,
                )
            })
            .collect();
        y += POWER_TILE_HEIGHT + gap;
        items
    });

    let footer_y = y;
    let power_button = Rect {
        x: width - pad - FOOTER_HEIGHT,
        y,
        w: FOOTER_HEIGHT,
        h: FOOTER_HEIGHT,
    };
    y += FOOTER_HEIGHT + pad;

    Layout {
        width,
        height: y,
        toggle_tile,
        toggles,
        volume,
        footer_y,
        power_button,
        power_menu,
    }
}

impl Layout {
    pub fn hit(&self, point: (f64, f64)) -> Option<Hit> {
        if self.volume.contains(point) {
            // The icon end of the slider mutes, like tapping the speaker.
            let icon_zone = Rect {
                w: self.volume.h,
                ..self.volume
            };
            return Some(if icon_zone.contains(point) {
                Hit::Mute
            } else {
                Hit::Volume
            });
        }
        self.toggles
            .iter()
            .chain(self.power_menu.iter().flatten())
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, hit)| *hit)
            .or_else(|| self.power_button.contains(point).then_some(Hit::Power))
    }

    /// Volume (0-100) for a pointer at `x` over the slider.
    pub fn volume_at(&self, x: f64) -> u32 {
        (((x as f32 - self.volume.x) / self.volume.w).clamp(0.0, 1.0) * 100.0).round() as u32
    }
}

pub struct QuickSettings {
    pub popup: Popup,
    scale: i32,
    configured: bool,
    layout: Layout,
    dragging_volume: bool,
    /// What the user just set, shown until the audio server reports it
    /// back (so the slider tracks the pointer with no lag).
    volume_override: Option<(u32, Instant)>,
}

impl QuickSettings {
    /// Open under `anchor` (panel-surface coordinates of the status area).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        compositor: &CompositorState,
        xdg_shell: &XdgShell,
        qh: &QueueHandle<App>,
        parent: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
        seat: &WlSeat,
        serial: u32,
        anchor: (i32, i32, i32, i32),
        theme: &Theme,
        scale: i32,
    ) -> Option<Self> {
        let layout = layout(theme, false);
        let positioner = positioner(xdg_shell, &layout, anchor)?;
        let surface = compositor.create_surface(qh);
        let popup = Popup::from_surface(None, &positioner, qh, surface, xdg_shell)
            .map_err(|err| warn!(?err, "can't create quick settings popup"))
            .ok()?;
        parent.get_popup(popup.xdg_popup());
        // Pointer and keyboard to us; a click elsewhere dismisses (done event).
        popup.xdg_popup().grab(seat, serial);
        popup.wl_surface().set_buffer_scale(scale);
        popup.wl_surface().commit();
        Some(Self {
            popup,
            scale,
            configured: false,
            layout,
            dragging_volume: false,
            volume_override: None,
        })
    }

    pub fn configured(&mut self) {
        self.configured = true;
    }

    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.max(1);
        self.popup.wl_surface().set_buffer_scale(self.scale);
    }

    /// Show or hide the power row; the popup grows or shrinks to fit.
    fn toggle_power_menu(
        &mut self,
        xdg_shell: &XdgShell,
        theme: &Theme,
        anchor: (i32, i32, i32, i32),
    ) {
        self.layout = layout(theme, self.layout.power_menu.is_none());
        if let Some(positioner) = positioner(xdg_shell, &self.layout, anchor) {
            self.popup.reposition(&positioner, 0);
        }
    }

    /// Returns true if the popup should close.
    pub fn press(
        &mut self,
        point: (f64, f64),
        status: &Status,
        actions: &Actions,
        xdg_shell: &XdgShell,
        theme: &Theme,
        anchor: (i32, i32, i32, i32),
    ) -> bool {
        match self.layout.hit(point) {
            Some(Hit::Volume) => {
                self.dragging_volume = true;
                self.set_volume(self.layout.volume_at(point.0), actions);
            }
            Some(Hit::Mute) => actions.run(Action::ToggleMute),
            Some(Hit::Wifi) => {
                if let Some(enabled) = status.wifi_enabled {
                    actions.run(Action::SetWifi(!enabled));
                }
            }
            Some(Hit::Power) => self.toggle_power_menu(xdg_shell, theme, anchor),
            Some(hit @ (Hit::Suspend | Hit::Reboot | Hit::PowerOff)) => {
                actions.run(match hit {
                    Hit::Suspend => Action::Suspend,
                    Hit::Reboot => Action::Reboot,
                    _ => Action::PowerOff,
                });
                return true;
            }
            // Not wired up yet (Bluetooth, dark mode, notifications).
            Some(Hit::Bluetooth | Hit::DarkMode | Hit::DoNotDisturb) | None => {}
        }
        false
    }

    pub fn motion(&mut self, point: (f64, f64), actions: &Actions) {
        if self.dragging_volume {
            self.set_volume(self.layout.volume_at(point.0), actions);
        }
    }

    pub fn release(&mut self) {
        self.dragging_volume = false;
    }

    fn set_volume(&mut self, percent: u32, actions: &Actions) {
        if self.volume_override.map(|(v, _)| v) != Some(percent) {
            self.volume_override = Some((percent, Instant::now()));
            actions.run(Action::SetVolume(percent));
        }
    }

    pub fn draw(
        &mut self,
        pool: &mut SlotPool,
        format: wl_shm::Format,
        fonts: &mut Fonts,
        icons: &mut Icons,
        theme: &Theme,
        status: &Status,
    ) {
        if !self.configured {
            return;
        }
        let layout = &self.layout;
        let scale = self.scale;
        let (w, h) = (
            (layout.width * scale as f32) as i32,
            (layout.height * scale as f32) as i32,
        );
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate quick settings buffer");
                return;
            }
        };
        let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
            return;
        };
        // What the slider shows: the user's own value while it's fresh.
        if let Some((_, since)) = self.volume_override
            && since.elapsed() > Duration::from_secs(2)
            && !self.dragging_volume
        {
            self.volume_override = None;
        }
        let volume = match (self.volume_override, status.volume) {
            (Some((percent, _)), current) => (percent, current.is_some_and(|c| c.muted)),
            (None, Some(current)) => (current.percent, current.muted),
            (None, None) => (0, true),
        };
        paint(
            &mut canvas,
            &self.layout,
            fonts,
            icons,
            theme,
            status,
            volume,
            scale as f32,
        );

        let surface = self.popup.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
        }
    }
}

/// Draw the card. Separate from the Wayland plumbing so tests can render it.
#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    status: &Status,
    volume: (u32, bool),
    s: f32,
) {
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    canvas.rounded_rect(
        0.0,
        0.0,
        layout.width,
        layout.height,
        theme.qs_radius,
        theme.qs_background,
    );

    // Toggles.
    let tile = layout.toggle_tile;
    canvas.rounded_rect(
        tile.x,
        tile.y,
        tile.w,
        tile.h,
        theme.qs_tile_radius,
        theme.qs_tile,
    );
    let wifi_on = status.wifi_enabled;
    for (rect, hit) in &layout.toggles {
        // (on?, available?, icon, label)
        let (on, available, icon, label) = match hit {
            Hit::Wifi => {
                let label = match &status.network {
                    crate::status::Network::Wifi { ssid, .. } if !ssid.is_empty() => ssid.clone(),
                    _ => "Wi-Fi".into(),
                };
                let icon = if wifi_on == Some(false) {
                    "network-wireless-disabled-symbolic"
                } else {
                    "network-wireless-symbolic"
                };
                (wifi_on == Some(true), wifi_on.is_some(), icon, label)
            }
            Hit::Bluetooth => (
                false,
                false,
                "bluetooth-disabled-symbolic",
                "Bluetooth".into(),
            ),
            Hit::DarkMode => (
                true,
                false,
                "weather-clear-night-symbolic",
                "Dark Mode".into(),
            ),
            _ => (
                false,
                false,
                "notifications-disabled-symbolic",
                "Do Not Disturb".into(),
            ),
        };
        let fade = if available { 1.0 } else { 0.4 };
        let circle = if on {
            theme.qs_toggle_on
        } else {
            theme.qs_toggle_off
        };
        canvas.circle(
            rect.x + rect.w / 2.0,
            rect.y + rect.h / 2.0,
            rect.w / 2.0,
            circle.with_alpha(circle.a * fade),
        );
        let icon_size = 22.0;
        canvas.icon(
            icons,
            icon,
            rect.x + (rect.w - icon_size) / 2.0,
            rect.y + (rect.h - icon_size) / 2.0,
            icon_size,
            Color::rgba(255, 255, 255, fade),
        );
        let mut text = fonts.layout(
            &ellipsize(&label, 13),
            theme.qs_label_size,
            Weight::Normal,
            s,
        );
        let tx = (rect.x + rect.w / 2.0 - text.width / 2.0).round();
        canvas.text(
            fonts,
            &mut text,
            tx,
            rect.y + rect.h + LABEL_GAP,
            theme.qs_dim.with_alpha(theme.qs_dim.a * fade),
        );
    }

    // Volume: the fat slider, speaker icon inside.
    let v = layout.volume;
    let (percent, muted) = volume;
    let fill = if muted {
        theme.qs_slider_fill.with_alpha(0.35)
    } else {
        theme.qs_slider_fill
    };
    canvas.slider(
        v.x,
        v.y,
        v.w,
        v.h,
        percent as f32 / 100.0,
        theme.qs_slider_track,
        fill,
    );
    let icon_size = 20.0;
    let icon_x = v.x + (v.h - icon_size) / 2.0 + 4.0;
    // Dark icon where the bright fill is under it.
    let covered = (percent as f32 / 100.0) * v.w > icon_x - v.x + icon_size;
    let icon_color = if covered && !muted {
        theme.qs_on_fill
    } else {
        Color::rgb(255, 255, 255)
    };
    let name = status.volume.map_or("audio-volume-muted-symbolic", |vol| {
        volume_icon(crate::status::Volume {
            percent,
            muted: vol.muted,
        })
    });
    canvas.icon(
        icons,
        name,
        icon_x,
        v.y + (v.h - icon_size) / 2.0,
        icon_size,
        icon_color,
    );

    // Power row.
    for (rect, hit) in layout.power_menu.iter().flatten() {
        canvas.rounded_rect(
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            theme.qs_tile_radius,
            theme.qs_tile,
        );
        let (icon, label) = match hit {
            Hit::Suspend => ("media-playback-pause-symbolic", "Suspend"),
            Hit::Reboot => ("system-reboot-symbolic", "Restart"),
            _ => ("system-shutdown-symbolic", "Power Off"),
        };
        canvas.icon(
            icons,
            icon,
            rect.x + (rect.w - 20.0) / 2.0,
            rect.y + 12.0,
            20.0,
            Color::rgb(255, 255, 255),
        );
        let mut text = fonts.layout(label, theme.qs_label_size, Weight::Normal, s);
        let tx = (rect.x + (rect.w - text.width) / 2.0).round();
        canvas.text(fonts, &mut text, tx, rect.y + 38.0, theme.qs_dim);
    }

    // Footer: battery summary, power button.
    if let Some(battery) = status.battery {
        let state = if battery.full {
            " · Full"
        } else if battery.charging {
            " · Charging"
        } else {
            ""
        };
        let mut text = fonts.layout(
            &format!("{:.0}%{state}", battery.percent),
            theme.font_size,
            Weight::Normal,
            s,
        );
        let ty = (layout.footer_y + (FOOTER_HEIGHT - text.height) / 2.0).round();
        canvas.text(
            fonts,
            &mut text,
            layout.toggle_tile.x + 4.0,
            ty,
            theme.qs_dim,
        );
    }
    let p = layout.power_button;
    let power_bg = if layout.power_menu.is_some() {
        theme.qs_toggle_on
    } else {
        theme.qs_toggle_off
    };
    canvas.circle(p.x + p.w / 2.0, p.y + p.h / 2.0, p.w / 2.0, power_bg);
    canvas.icon(
        icons,
        "system-shutdown-symbolic",
        p.x + (p.w - 18.0) / 2.0,
        p.y + (p.h - 18.0) / 2.0,
        18.0,
        Color::rgb(255, 255, 255),
    );
}

fn positioner(
    xdg_shell: &XdgShell,
    layout: &Layout,
    (ax, ay, aw, ah): (i32, i32, i32, i32),
) -> Option<XdgPositioner> {
    let positioner = XdgPositioner::new(xdg_shell)
        .map_err(|err| warn!(?err, "no xdg positioner"))
        .ok()?;
    positioner.set_size(layout.width as i32, layout.height as i32);
    positioner.set_anchor_rect(ax, ay, aw.max(1), ah.max(1));
    // Hang below the status icons, right edges aligned.
    positioner.set_anchor(Anchor::BottomRight);
    positioner.set_gravity(Gravity::BottomLeft);
    positioner.set_offset(-6, 6);
    positioner
        .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
    Some(positioner)
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hits() {
        let theme = Theme::dark();
        let l = layout(&theme, false);
        let centre = |r: Rect| ((r.x + r.w / 2.0) as f64, (r.y + r.h / 2.0) as f64);
        assert_eq!(l.hit(centre(l.toggles[0].0)), Some(Hit::Wifi));
        assert_eq!(
            l.hit((l.volume.x as f64 + 10.0, centre(l.volume).1)),
            Some(Hit::Mute)
        );
        assert_eq!(l.hit(centre(l.volume)), Some(Hit::Volume));
        assert_eq!(l.hit(centre(l.power_button)), Some(Hit::Power));
        assert!(l.power_menu.is_none());
    }

    #[test]
    fn power_menu_grows_the_card() {
        let theme = Theme::dark();
        let closed = layout(&theme, false);
        let open = layout(&theme, true);
        assert!(open.height > closed.height);
        let reboot = open.power_menu.as_ref().unwrap()[1].0;
        assert_eq!(
            open.hit(((reboot.x + 5.0) as f64, (reboot.y + 5.0) as f64)),
            Some(Hit::Reboot)
        );
    }

    #[test]
    fn slider_maps_x_to_percent() {
        let l = layout(&Theme::dark(), false);
        assert_eq!(l.volume_at(l.volume.x as f64), 0);
        assert_eq!(l.volume_at((l.volume.x + l.volume.w) as f64), 100);
        assert_eq!(l.volume_at((l.volume.x + l.volume.w / 2.0) as f64), 50);
    }

    /// `CNDE_QS_PREVIEW=/path cargo test -p cnde-shell preview -- --ignored`
    /// renders the card (2x) as raw RGBA with a "WxH" header line.
    #[test]
    #[ignore]
    fn preview() {
        let Some(path) = std::env::var_os("CNDE_QS_PREVIEW") else {
            return;
        };
        let theme = Theme::dark();
        let status = Status {
            battery: Some(crate::status::Battery { percent: 87.0, charging: true, full: false }),
            network: crate::status::Network::Wifi { ssid: "Homenet".into(), strength: 80 },
            wifi_enabled: Some(true),
            volume: Some(crate::status::Volume { percent: 62, muted: false }),
        };
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let layout = layout(&theme, std::env::var_os("CNDE_QS_POWER").is_some());
        let scale = 2.0;
        let (w, h) = ((layout.width * scale) as u32, (layout.height * scale) as u32);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
            paint(&mut canvas, &layout, &mut fonts, &mut icons, &theme, &status, (62, false), scale);
        }
        let mut data = format!("{w}x{h}\n").into_bytes();
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn long_names_are_shortened() {
        assert_eq!(ellipsize("Wi-Fi", 13), "Wi-Fi");
        assert_eq!(ellipsize("MyVeryLongNetworkName", 8), "MyVeryL…");
    }
}
