//! The top bar: workspace indicator on the left, clock in the centre
//! (ARCHITECTURE.md §5.3). Status icons and quick settings come next.

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
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1;

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
}

pub struct Panel {
    pub output: WlOutput,
    pub layer: LayerSurface,
    /// Logical size; 0 until the compositor configures us.
    width: u32,
    height: u32,
    scale: i32,
    /// Clickable workspace dots from the last draw (logical x range).
    hit_regions: Vec<(f64, f64, ExtWorkspaceHandleV1)>,
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
            hit_regions: Vec::new(),
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

    pub fn workspace_at(&self, (x, _y): (f64, f64)) -> Option<ExtWorkspaceHandleV1> {
        self.hit_regions
            .iter()
            .find(|(start, end, _)| x >= *start && x < *end)
            .map(|(_, _, handle)| handle.clone())
    }

    pub fn draw(&mut self, pool: &mut SlotPool, format: wl_shm::Format, ctx: Draw<'_>) {
        let Draw {
            fonts,
            icons,
            theme,
            clock_format,
            workspaces,
            status,
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

        // Workspaces: GNOME-style dots, the active ones stretched into pills.
        self.hit_regions.clear();
        let dot = theme.workspace_dot_size;
        let mut x = theme.padding;
        for ws in workspaces {
            let (dot_w, color) = if ws.active {
                (theme.workspace_active_width, theme.workspace_active)
            } else {
                (dot, theme.workspace_dot)
            };
            canvas.rounded_rect(x, (height - dot) / 2.0, dot_w, dot, dot / 2.0, color);
            // Generous click targets: the dot plus half the gap either side.
            let half_gap = theme.workspace_spacing as f64 / 2.0;
            self.hit_regions.push((
                x as f64 - half_gap,
                (x + dot_w) as f64 + half_gap,
                ws.handle.clone(),
            ));
            x += dot_w + theme.workspace_spacing;
        }

        // Clock, centred.
        let text = format_clock(clock_format);
        let mut clock = fonts.layout(&text, theme.font_size, Weight::Bold, scale as f32);
        let clock_x = ((width - clock.width) / 2.0).round();
        let clock_y = ((height - clock.height) / 2.0).round();
        canvas.text(fonts, &mut clock, clock_x, clock_y, theme.panel_foreground);

        // Status, right-aligned: network, volume, battery (+ percentage).
        let icon = theme.status_icon_size;
        let icon_y = ((height - icon) / 2.0).round();
        let mut right = width - theme.padding;
        if let Some(battery) = status.battery {
            let mut text = fonts.layout(
                &format!("{:.0}%", battery.percent),
                theme.font_size,
                Weight::Normal,
                scale as f32,
            );
            right -= text.width;
            let text_y = ((height - text.height) / 2.0).round();
            canvas.text(
                fonts,
                &mut text,
                right.round(),
                text_y,
                theme.panel_foreground,
            );
            right -= icon + theme.status_icon_text_gap;
            canvas.icon(
                icons,
                &battery_icon(battery),
                right,
                icon_y,
                icon,
                theme.panel_foreground,
            );
            right -= theme.status_spacing;
        }
        if let Some(volume) = status.volume {
            right -= icon;
            canvas.icon(
                icons,
                volume_icon(volume),
                right,
                icon_y,
                icon,
                theme.panel_foreground,
            );
            right -= theme.status_spacing;
        }
        if let Some(name) = network_icon(&status.network) {
            right -= icon;
            canvas.icon(icons, name, right, icon_y, icon, theme.panel_foreground);
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

fn volume_icon(volume: Volume) -> &'static str {
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
