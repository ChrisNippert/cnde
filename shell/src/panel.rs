//! The top bar (ARCHITECTURE.md §5.3): the workspace indicator on the left
//! doubles as the Activities button (opens the overview), the clock is in
//! the centre, and the status icons on the right open quick settings.

use chrono::Local;
use cnde_ui::{Canvas, Fonts, Icons, Theme, Weight};
use smithay_client_toolkit::{
    compositor::CompositorState,
    reexports::client::{
        QueueHandle,
        protocol::{wl_output::WlOutput, wl_shm},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface},
    },
    shm::slot::SlotPool,
};
use tracing::warn;

use crate::{
    App, Status,
    status::{Battery, Network, Volume},
    workspaces::Workspace,
};

/// Everything a panel draws from.
pub struct Draw<'a> {
    pub fonts: &'a mut Fonts,
    pub icons: &'a mut Icons,
    pub theme: &'a Theme,
    pub clock_format: &'a str,
    pub workspaces: &'a [Workspace],
    pub status: &'a Status,
    /// What the buttons open is open (they're drawn highlighted).
    pub overview_open: bool,
    pub quick_settings_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Activities,
    Status,
}

pub struct Panel {
    pub output: WlOutput,
    pub layer: LayerSurface,
    /// Logical size; 0 until the compositor configures us.
    width: u32,
    height: u32,
    scale: i32,
    /// The Activities button's logical x range, from the last draw.
    activities_region: Option<(f32, f32)>,
    /// The status icons' logical x range, from the last draw.
    status_region: Option<(f32, f32)>,
    /// The button under the pointer.
    hover: Option<Button>,
}

impl Panel {
    pub fn new(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        qh: &QueueHandle<App>,
        output: WlOutput,
        theme: &Theme,
    ) -> Self {
        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("cnde-panel"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        // Width 0 = stretch across the monitor; the compositor picks it.
        layer.set_size(0, theme.panel_height);
        // Maximized windows stop below the panel.
        layer.set_exclusive_zone(theme.panel_height as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        Self {
            output,
            layer,
            width: 0,
            height: theme.panel_height,
            scale: 1,
            activities_region: None,
            status_region: None,
            hover: None,
        }
    }

    /// A new theme may change the bar's height (and the space it reserves).
    pub fn apply_theme(&mut self, theme: &Theme) {
        if self.height != theme.panel_height {
            self.height = theme.panel_height;
            self.layer.set_size(0, theme.panel_height);
            self.layer.set_exclusive_zone(theme.panel_height as i32);
            self.layer.commit();
        }
    }

    pub fn configure(&mut self, (width, height): (u32, u32)) {
        self.width = width;
        if height > 0 {
            self.height = height;
        }
    }

    pub fn set_scale(&mut self, scale: i32) {
        self.scale = scale.max(1);
        self.layer.wl_surface().set_buffer_scale(self.scale);
    }

    pub fn scale(&self) -> i32 {
        self.scale
    }

    /// The button at `x`. The outer buttons reach all the way to the screen
    /// corners, so throwing the pointer into a corner and clicking works.
    pub fn button_at(&self, (x, _y): (f64, f64)) -> Option<Button> {
        let x = x as f32;
        if self.activities_region.is_some_and(|(_, end)| x < end) {
            Some(Button::Activities)
        } else if self.status_region.is_some_and(|(start, _)| x >= start) {
            Some(Button::Status)
        } else {
            None
        }
    }

    /// Returns true if the hover changed (and the panel needs a redraw).
    pub fn set_hover(&mut self, hover: Option<Button>) -> bool {
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// Where quick settings hangs from: the status area, panel coordinates.
    pub fn status_anchor(&self) -> Option<(i32, i32, i32, i32)> {
        let (start, end) = self.status_region?;
        Some((start as i32, 0, (end - start) as i32, self.height as i32))
    }

    /// The pill behind a button: brighter while what it opens is open.
    fn button_background(
        &self,
        canvas: &mut Canvas<'_>,
        theme: &Theme,
        (start, end): (f32, f32),
        height: f32,
        button: Button,
        open: bool,
    ) {
        let color = if open {
            theme.panel_button_active
        } else if self.hover == Some(button) {
            theme.panel_button_hover
        } else {
            return;
        };
        let inset = theme.panel_button_inset;
        let h = height - 2.0 * inset;
        canvas.rounded_rect(start, inset, end - start, h, h / 2.0, color);
    }

    pub fn draw(&mut self, pool: &mut SlotPool, format: wl_shm::Format, ctx: Draw<'_>) {
        let Draw {
            fonts,
            icons,
            theme,
            clock_format,
            workspaces,
            status,
            overview_open,
            quick_settings_open,
        } = ctx;
        if self.width == 0 {
            return; // not configured yet
        }
        let scale = self.scale;
        let (w, h) = (self.width as i32 * scale, self.height as i32 * scale);
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate panel buffer");
                return;
            }
        };
        let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) else {
            return;
        };
        let (width, height) = canvas.size();
        canvas.clear(theme.panel_background);

        // Activities: GNOME-style workspace dots, the active ones stretched
        // into pills. The whole cluster is one button.
        let dot = theme.workspace_dot_size;
        let dots_width: f32 = workspaces
            .iter()
            .map(|ws| {
                if ws.active {
                    theme.workspace_active_width
                } else {
                    dot
                }
            })
            .sum::<f32>()
            + theme.workspace_spacing * workspaces.len().saturating_sub(1) as f32;
        let pad = theme.panel_button_padding;
        let start = theme.padding - pad / 2.0;
        let activities = (start, start + pad + dots_width.max(dot) + pad);
        self.activities_region = Some(activities);
        self.button_background(
            &mut canvas,
            theme,
            activities,
            height,
            Button::Activities,
            overview_open,
        );
        let mut x = start + pad;
        for ws in workspaces {
            let (dot_w, color) = if ws.active {
                (theme.workspace_active_width, theme.workspace_active)
            } else {
                (dot, theme.workspace_dot)
            };
            canvas.rounded_rect(x, (height - dot) / 2.0, dot_w, dot, dot / 2.0, color);
            x += dot_w + theme.workspace_spacing;
        }

        // Clock, centred.
        let text = format_clock(clock_format);
        let mut clock = fonts.layout(&text, theme.font_size, Weight::Bold, scale as f32);
        let clock_x = ((width - clock.width) / 2.0).round();
        let clock_y = ((height - clock.height) / 2.0).round();
        canvas.text(fonts, &mut clock, clock_x, clock_y, theme.panel_foreground);

        // Status, right-aligned: network, volume, battery (+ percentage),
        // one button. Measured first so its hover pill goes underneath.
        let icon = theme.status_icon_size;
        let icon_y = ((height - icon) / 2.0).round();
        let fg = theme.panel_foreground;
        let mut items: Vec<String> = Vec::new();
        if let Some(name) = network_icon(&status.network) {
            items.push(name.into());
        }
        if let Some(volume) = status.volume {
            items.push(volume_icon(volume).into());
        }
        if let Some(battery) = status.battery {
            items.push(battery_icon(battery));
        }
        let mut percent = status.battery.map(|battery| {
            fonts.layout(
                &format!("{:.0}%", battery.percent),
                theme.font_size,
                Weight::Normal,
                scale as f32,
            )
        });
        let icons_width =
            items.len() as f32 * icon + theme.status_spacing * items.len().saturating_sub(1) as f32;
        let text_width = percent
            .as_ref()
            .map_or(0.0, |t| theme.status_icon_text_gap + t.width);
        let content = (icons_width + text_width).max(icon);
        let end = width - theme.padding + pad / 2.0;
        let region = (end - pad - content - pad, end);
        self.status_region = Some(region);
        self.button_background(
            &mut canvas,
            theme,
            region,
            height,
            Button::Status,
            quick_settings_open,
        );
        let mut x = region.0 + pad;
        for name in &items {
            canvas.icon(icons, name, x, icon_y, icon, fg);
            x += icon + theme.status_spacing;
        }
        if let Some(text) = &mut percent {
            let x = x - theme.status_spacing + theme.status_icon_text_gap;
            let text_y = ((height - text.height) / 2.0).round();
            canvas.text(fonts, text, x.round(), text_y, fg);
        }

        // Debug aid: CNDE_SHELL_DUMP=path writes each frame's raw RGBA
        // pixels (with a "WxH\n" header) for inspecting the look offline.

        // ARGB8888 is BGRA in memory; we drew RGBA.
        if format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }

        if let Some(path) = std::env::var_os("CNDE_SHELL_DUMP") {
            let mut data = format!("{w}x{h}\n").into_bytes();
            data.extend_from_slice(pixels);
            let _ = std::fs::write(path, data);
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if let Err(err) = buffer.attach_to(surface) {
            warn!(?err, "can't attach panel buffer");
            return;
        }
        self.layer.commit();
    }
}

pub fn volume_icon(volume: Volume) -> &'static str {
    match (volume.muted, volume.percent) {
        (true, _) | (_, 0) => "audio-volume-muted-symbolic",
        (_, 1..=33) => "audio-volume-low-symbolic",
        (_, 34..=66) => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

/// Adwaita has an icon per 10% (battery-level-40-charging-symbolic, ...).
fn battery_icon(battery: Battery) -> String {
    if battery.full {
        return "battery-level-100-charged-symbolic".into();
    }
    let level = ((battery.percent / 10.0).floor() as u32 * 10).min(100);
    if battery.charging {
        format!("battery-level-{level}-charging-symbolic")
    } else {
        format!("battery-level-{level}-symbolic")
    }
}

fn network_icon(network: &Network) -> Option<&'static str> {
    Some(match network {
        Network::Unavailable => return None,
        Network::Disconnected => "network-wireless-offline-symbolic",
        Network::Connecting => "network-wireless-acquiring-symbolic",
        Network::Wired => "network-wired-symbolic",
        Network::Wifi { strength, .. } => match strength {
            0..=19 => "network-wireless-signal-none-symbolic",
            20..=39 => "network-wireless-signal-weak-symbolic",
            40..=59 => "network-wireless-signal-ok-symbolic",
            60..=79 => "network-wireless-signal-good-symbolic",
            _ => "network-wireless-signal-excellent-symbolic",
        },
    })
}

/// The clock text. An invalid format (e.g. "%Q") makes chrono's formatter
/// fail, which would panic in `to_string`; fall back to the default.
fn format_clock(format: &str) -> String {
    use std::fmt::Write;
    let now = Local::now();
    let mut text = String::new();
    if write!(text, "{}", now.format(format)).is_ok() {
        return text;
    }
    now.format("%a %b %-d  %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_icons_exist_in_adwaita_naming() {
        let b = |percent, charging, full| Battery {
            percent,
            charging,
            full,
        };
        assert_eq!(
            battery_icon(b(87.0, false, false)),
            "battery-level-80-symbolic"
        );
        assert_eq!(
            battery_icon(b(5.0, true, false)),
            "battery-level-0-charging-symbolic"
        );
        assert_eq!(
            battery_icon(b(100.0, false, true)),
            "battery-level-100-charged-symbolic"
        );
    }

    #[test]
    fn invalid_clock_format_falls_back() {
        assert!(!format_clock("%Q").is_empty());
        assert_eq!(format_clock("x%%y"), "x%y");
    }
}
