//! The top bar: workspace indicator on the left, clock in the centre
//! (ARCHITECTURE.md §5.3). Status icons and quick settings come next.

use chrono::Local;
use cnde_ui::{Canvas, Fonts, Theme, Weight};
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

use crate::{App, workspaces::Workspace};

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

    pub fn draw(
        &mut self,
        pool: &mut SlotPool,
        fonts: &mut Fonts,
        theme: &Theme,
        clock_format: &str,
        workspaces: &[Workspace],
    ) {
        if self.width == 0 {
            return; // not configured yet
        }
        let scale = self.scale;
        let (w, h) = (self.width as i32 * scale, self.height as i32 * scale);
        let (buffer, pixels) = match pool.create_buffer(w, h, w * 4, wl_shm::Format::Abgr8888) {
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

        // Debug aid: CNDE_SHELL_DUMP=path writes each frame's raw RGBA
        // pixels (with a "WxH\n" header) for inspecting the look offline.

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
    fn invalid_clock_format_falls_back() {
        assert!(!format_clock("%Q").is_empty());
        assert_eq!(format_clock("x%%y"), "x%y");
    }
}
