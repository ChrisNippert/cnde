//! The top bar (ARCHITECTURE.md §5.3): the workspace indicator on the left
//! doubles as the Activities button (opens the overview), the clock is in
//! the centre, and the status icons on the right open quick settings.

use chrono::Local;
use cnde_config::ClockPosition;
use cnde_ui::{Animator, Canvas, Color, Fonts, Icons, Theme, Weight, anim::SHORT};
use smithay_client_toolkit::{
    compositor::{CompositorState, FrameCallbackData},
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
    pub qh: &'a QueueHandle<App>,
    pub fonts: &'a mut Fonts,
    pub icons: &'a mut Icons,
    pub theme: &'a Theme,
    pub clock_format: &'a str,
    pub workspaces: &'a [Workspace],
    pub status: &'a Status,
    /// What the buttons open is open (they're drawn highlighted).
    pub overview_open: bool,
    pub quick_settings_open: bool,
    pub calendar_open: bool,
    pub clock_position: ClockPosition,
    /// Number beside the battery icon.
    pub battery_percent: bool,
    /// Below the warning level: the battery is drawn in the error colour.
    pub battery_low: bool,
    /// A window is against the bar: no see-through, whatever the style.
    pub solid: bool,
}

/// Editing the bar in place (from its right-click menu).
#[derive(Debug, Default)]
pub struct PanelEdit {
    pub drag: Option<PanelDrag>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelDrag {
    /// Dragging the bottom edge: where the press was, and the height then.
    Height { start_y: f64, start_height: u32 },
    /// Carrying the clock; its centre follows the pointer.
    Clock { x: f32 },
}

/// What a press hits while editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Done,
    Clock,
    Height,
}

/// How close to the bottom edge a press grabs it.
const EDGE_GRAB: f32 = 7.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Activities,
    Status,
    Clock,
}

pub struct Panel {
    pub output: WlOutput,
    pub layer: LayerSurface,
    /// Logical size; 0 until the compositor configures us.
    width: u32,
    /// The bar itself; the surface is `shadow` taller.
    height: u32,
    shadow: u32,
    radius: f32,
    scale: i32,
    /// The Activities button's logical x range, from the last draw.
    activities_region: Option<(f32, f32)>,
    /// The status icons' logical x range, from the last draw.
    status_region: Option<(f32, f32)>,
    /// The button under the pointer.
    hover: Option<Button>,
    /// Being edited in place.
    pub edit: Option<PanelEdit>,
    clock_region: Option<(f32, f32)>,
    done_region: Option<(f32, f32)>,
    /// Hover pills fading: (button, true) for the "open" highlight.
    anim: Animator<(Button, bool)>,
    /// Fading to solid (1) when a window's against the bar, and back.
    solid_fade: Animator<()>,
    /// A frame callback is outstanding; draw when it fires.
    frame_pending: bool,
    dirty: bool,
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
        layer.set_size(0, theme.panel_height + theme.panel_shadow);
        // Maximized windows stop below the bar (its shadow falls on them).
        layer.set_exclusive_zone(theme.panel_height as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        Self {
            output,
            layer,
            width: 0,
            height: theme.panel_height,
            shadow: theme.panel_shadow,
            radius: theme.panel_radius,
            scale: 1,
            activities_region: None,
            status_region: None,
            hover: None,
            edit: None,
            clock_region: None,
            done_region: None,
            anim: Animator::new(),
            solid_fade: Animator::new(),
            frame_pending: false,
            dirty: false,
        }
    }

    /// A new theme may change the bar's height (and the space it reserves).
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.radius = theme.panel_radius;
        if self.height != theme.panel_height || self.shadow != theme.panel_shadow {
            self.height = theme.panel_height;
            self.shadow = theme.panel_shadow;
            self.layer
                .set_size(0, theme.panel_height + theme.panel_shadow);
            self.layer.set_exclusive_zone(theme.panel_height as i32);
            self.layer.commit();
        }
    }

    /// Only the width comes from the compositor: the height is ours (and
    /// the surface's includes the shadow).
    pub fn configure(&mut self, (width, _height): (u32, u32)) {
        self.width = width;
    }

    /// Clicks on the shadow go to whatever is under it.
    pub fn update_input_region(&self, compositor: &CompositorState) {
        if let Ok(region) = smithay_client_toolkit::compositor::Region::new(compositor) {
            region.add(0, 0, self.width as i32, self.height as i32);
            self.layer
                .wl_surface()
                .set_input_region(Some(region.wl_region()));
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
        } else if self.clock_region.is_some_and(|(start, end)| x >= start && x < end) {
            Some(Button::Clock)
        } else {
            None
        }
    }

    /// What's under `point` while editing.
    pub fn edit_target(&self, (x, y): (f64, f64)) -> Option<EditTarget> {
        let (x, y) = (x as f32, y as f32);
        let inside = |r: Option<(f32, f32)>| r.is_some_and(|(a, b)| x >= a && x < b);
        if inside(self.done_region) {
            Some(EditTarget::Done)
        } else if y >= self.height as f32 - EDGE_GRAB {
            Some(EditTarget::Height)
        } else if inside(self.clock_region) {
            Some(EditTarget::Clock)
        } else {
            None
        }
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    /// Resize live while the bottom edge is dragged.
    pub fn set_height(&mut self, height: u32) {
        if height != self.height {
            self.height = height;
            self.layer.set_size(0, height + self.shadow);
            self.layer.set_exclusive_zone(height as i32);
            self.layer.commit();
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

    /// Where the calendar hangs from: the clock, panel coordinates.
    pub fn clock_anchor(&self) -> Option<(i32, i32, i32, i32)> {
        let (start, end) = self.clock_region?;
        Some((start as i32, 0, (end - start) as i32, self.height as i32))
    }

    /// The pill behind a button: brighter while what it opens is open.
    fn button_background(
        &mut self,
        canvas: &mut Canvas<'_>,
        theme: &Theme,
        (start, end): (f32, f32),
        height: f32,
        button: Button,
        open: bool,
    ) {
        let hovered = self.hover == Some(button);
        let hovered = self
            .anim
            .get((button, false), if hovered { 1.0 } else { 0.0 }, SHORT);
        let opened = self
            .anim
            .get((button, true), if open { 1.0 } else { 0.0 }, SHORT);
        if hovered <= 0.0 && opened <= 0.0 {
            return;
        }
        let hover = theme.panel_button_hover;
        let color = hover
            .with_alpha(hover.a * hovered)
            .mix(theme.panel_button_active, opened);
        let inset = theme.panel_button_inset;
        let h = height - 2.0 * inset;
        canvas.rounded_rect(start, inset, end - start, h, h / 2.0, color);
    }

    /// The frame callback fired; true if a draw was held back for it.
    pub fn frame_done(&mut self) -> bool {
        self.frame_pending = false;
        self.dirty
    }

    pub fn draw(&mut self, pool: &mut SlotPool, format: wl_shm::Format, ctx: Draw<'_>) {
        let Draw {
            qh,
            fonts,
            icons,
            theme,
            clock_format,
            workspaces,
            status,
            overview_open,
            quick_settings_open,
            calendar_open,
            clock_position,
            battery_percent,
            battery_low,
            solid,
        } = ctx;
        if self.width == 0 {
            return; // not configured yet
        }
        if self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        self.anim.begin();
        self.solid_fade.begin();
        let solid = self
            .solid_fade
            .get((), if solid { 1.0 } else { 0.0 }, cnde_ui::anim::MEDIUM);
        let scale = self.scale;
        let (w, h) = (
            self.width as i32 * scale,
            (self.height + self.shadow) as i32 * scale,
        );
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
        let (width, _) = canvas.size();
        let height = self.height as f32;
        canvas.clear(Color::rgba(0, 0, 0, 0.0));
        // A soft shadow: rounded rects each a little taller and fainter.
        for i in 1..=self.shadow {
            let fade = 1.0 - i as f32 / (self.shadow + 1) as f32;
            canvas.rounded_rect(
                0.0,
                -self.radius,
                width,
                height + self.radius + i as f32,
                self.radius + i as f32,
                Color::rgba(0, 0, 0, 0.07 * fade),
            );
        }
        // The bar: square at the top edge, rounded at the bottom (its top
        // corners are drawn off the surface).
        canvas.rounded_rect(
            0.0,
            -self.radius,
            width,
            height + self.radius,
            self.radius,
            {
                let a = theme.panel_background.a;
                theme.panel_background.with_alpha(a + (1.0 - a) * solid)
            },
        );

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
        let start = theme.panel_padding - pad / 2.0;
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
        let mut percent = status.battery.filter(|_| battery_percent).map(|battery| {
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
        let end = width - theme.panel_padding + pad / 2.0;
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
        let battery_name = status.battery.map(battery_icon);
        for name in &items {
            let low = battery_low && battery_name.as_ref() == Some(name);
            canvas.icon(
                icons,
                name,
                x,
                icon_y,
                icon,
                if low { theme.error } else { fg },
            );
            x += icon + theme.status_spacing;
        }
        if let Some(text) = &mut percent {
            let x = x - theme.status_spacing + theme.status_icon_text_gap;
            let text_y = ((height - text.height) / 2.0).round();
            let color = if battery_low { theme.error } else { fg };
            canvas.text(fonts, text, x.round(), text_y, color);
        }

        // Editing: a Done button left of the status icons.
        self.done_region = None;
        let mut right_limit = region.0;
        if self.edit.is_some() {
            let mut label = fonts.layout("Done", theme.font_size, Weight::Bold, scale as f32);
            let bw = label.width + 2.0 * pad + 8.0;
            let bx = region.0 - 8.0 - bw;
            let inset = theme.panel_button_inset;
            let bh = height - 2.0 * inset;
            canvas.rounded_rect(bx, inset, bw, bh, bh / 2.0, theme.accent);
            let ty = ((height - label.height) / 2.0).round();
            let tx = (bx + (bw - label.width) / 2.0).round();
            canvas.text(fonts, &mut label, tx, ty, theme.on_accent);
            self.done_region = Some((bx, bx + bw));
            right_limit = bx;
        }

        // Clock: centred, or beside the dots or the status icons.
        let text = format_clock(clock_format);
        let mut clock = fonts.layout(&text, theme.font_size, Weight::Bold, scale as f32);
        const GAP: f32 = 16.0;
        let slot_x = |position: ClockPosition| match position {
            ClockPosition::Left => activities.1 + GAP,
            ClockPosition::Center => (width - clock.width) / 2.0,
            ClockPosition::Right => right_limit - GAP - clock.width,
        };
        let dragged = match self.edit.as_ref().and_then(|e| e.drag) {
            Some(PanelDrag::Clock { x }) => Some(x),
            _ => None,
        };
        let clock_x = match dragged {
            Some(x) => {
                // Where it would land, outlined.
                let target = slot_x(clock_slot(x, width));
                let outline = theme.accent.with_alpha(0.7);
                canvas.stroke_rounded_rect(
                    target - 6.0,
                    3.0,
                    clock.width + 12.0,
                    height - 6.0,
                    (height - 6.0) / 2.0,
                    1.5,
                    outline,
                );
                x - clock.width / 2.0
            }
            None => slot_x(clock_position),
        }
        .round();
        let clock_y = ((height - clock.height) / 2.0).round();
        let clock_region = (clock_x - pad, clock_x + clock.width + pad);
        if self.edit.is_none() {
            self.button_background(
                &mut canvas,
                theme,
                clock_region,
                height,
                Button::Clock,
                calendar_open,
            );
        }
        canvas.text(fonts, &mut clock, clock_x, clock_y, theme.panel_foreground);
        self.clock_region = Some(clock_region);

        if self.edit.is_some() {
            // The bar outlined, the clock marked movable, a grip to resize.
            canvas.stroke_rounded_rect(0.0, 0.0, width, height, 0.0, 2.0, theme.accent);
            if dragged.is_none() {
                let dim = theme.panel_foreground.with_alpha(0.35);
                canvas.stroke_rounded_rect(
                    clock_x - 6.0,
                    3.0,
                    clock.width + 12.0,
                    height - 6.0,
                    (height - 6.0) / 2.0,
                    1.0,
                    dim,
                );
            }
            canvas.rounded_rect(
                width / 2.0 - 20.0,
                height - 5.0,
                40.0,
                3.0,
                1.5,
                theme.accent,
            );
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
        surface.frame(qh, FrameCallbackData(surface.clone()));
        self.frame_pending = true;
        // A pill still fading: draw again next frame.
        if self.anim.running() || self.solid_fade.running() {
            self.dirty = true;
        }
        self.layer.commit();
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| {
            if let Some(start) = crate::START.get() {
                tracing::debug!(elapsed = ?start.elapsed(), "first panel commit");
            }
        });
    }
}

/// Which slot a clock dropped at `x` goes to: the bar's thirds.
pub fn clock_slot(x: f32, width: f32) -> ClockPosition {
    if x < width / 3.0 {
        ClockPosition::Left
    } else if x > width * 2.0 / 3.0 {
        ClockPosition::Right
    } else {
        ClockPosition::Center
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
pub fn battery_icon(battery: Battery) -> String {
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
