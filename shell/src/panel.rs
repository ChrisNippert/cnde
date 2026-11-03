//! The top bar (ARCHITECTURE.md §5.3): the workspace indicator on the left
//! doubles as the Activities button (opens the overview), the clock is in
//! the centre, and the status icons on the right open quick settings.

use chrono::Local;
use cnde_config::{BarMode, ClockPosition, PanelStyle, TrayMode};
use cnde_ipc::panel::GAP as FLOAT_GAP;
use cnde_ui::{
    Animator, Canvas, Color, Fonts, Icons, Theme, Weight,
    anim::{MEDIUM, SHORT},
};
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
    tray::{self, TrayItem},
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
    pub tray: &'a mut [TrayItem],
    pub tray_mode: TrayMode,
    pub tray_popover_open: bool,
    /// A power icon at the end of the status button.
    pub power_icon: bool,
    /// A microphone while an app is recording.
    pub mic_indicator: bool,
    /// A keyboard button left of the tray, and whether the keyboard is up.
    pub keyboard_button: bool,
    pub keyboard_open: bool,
    /// What the buttons open is open (they're drawn highlighted).
    pub overview_open: bool,
    pub quick_settings_open: bool,
    pub calendar_open: bool,
    /// A dot beside the clock: notifications nobody's looked at.
    pub unread: bool,
    pub clock_position: ClockPosition,
    /// Number beside the battery icon.
    pub battery_percent: bool,
    /// Below the warning level: the battery is drawn in the error colour.
    pub battery_low: bool,
    /// A window is against the bar: no see-through, whatever the style.
    pub solid: bool,
    pub mode: BarMode,
    /// What the bar looks like while it floats free.
    pub style: PanelStyle,
    /// The wallpaper behind the bar is light: a clear bar's text goes dark.
    pub backdrop_light: bool,
    /// The colour for the bar to take (a window's, else the wallpaper's).
    pub tint: Option<cnde_ui::Tint>,
    /// `panel.edge_line`.
    pub edge_line: u32,
    /// The keyboard layout's label, with more than one layout.
    pub layout: Option<&'a str>,
    /// The bar is frosted: islands take the bar's lighter tint, as it does.
    pub frosted: bool,
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
/// Between the tray icons and the status button.
const TRAY_GAP: f32 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Activities,
    Status,
    Clock,
    /// A tray icon in the bar (an index into the tray's items).
    Tray(usize),
    /// The chevron that opens the tray popover.
    Chevron,
    /// The keyboard layout: a click switches to the next.
    Layout,
    /// Shows or hides the on-screen keyboard.
    Keyboard,
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
    /// Each tray icon's (index, logical x range), from the last draw.
    tray_regions: Vec<(usize, f32, f32)>,
    /// The tray chevron's logical x range, from the last draw.
    chevron_region: Option<(f32, f32)>,
    /// The keyboard layout's logical x range, from the last draw.
    layout_region: Option<(f32, f32)>,
    /// The keyboard button's logical x range, from the last draw.
    keyboard_region: Option<(f32, f32)>,
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
    /// Easing towards the colour it's taking.
    tint_fade: cnde_ui::TintFade,
    /// What it's wearing now; quick settings, the calendar and menus follow.
    pub shown: Option<cnde_ui::Shown>,
    /// How far it floats off the edge: 0 attached, 1 floating.
    float: Animator<()>,
    /// The layer margin last asked for: `FLOAT_GAP` times how far it floats.
    margin: i32,
    mode: BarMode,
    /// A frame callback is outstanding; draw when it fires.
    frame_pending: bool,
    dirty: bool,
    /// The islands drawn last: their logical x ranges, and how visible.
    islands: Vec<(f32, f32)>,
    island_alpha: f32,
    /// The frost cards last sent to the compositor for them.
    pub frost_sent: Vec<cnde_ipc::CardFrost>,
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
            tray_regions: Vec::new(),
            chevron_region: None,
            layout_region: None,
            keyboard_region: None,
            hover: None,
            edit: None,
            clock_region: None,
            done_region: None,
            anim: Animator::new(),
            solid_fade: Animator::new(),
            tint_fade: cnde_ui::TintFade::new(),
            shown: None,
            float: Animator::new(),
            margin: 0,
            mode: BarMode::Attached,
            frame_pending: false,
            dirty: false,
            islands: Vec::new(),
            island_alpha: 0.0,
            frost_sent: Vec::new(),
        }
    }

    /// The space kept free for the bar below its margin. Attached or auto,
    /// margin plus zone is always the bar's height, so maximized windows
    /// don't move while it floats off or snaps back; floating for good, a
    /// gap is kept below it too.
    fn exclusive_zone(&self) -> i32 {
        match self.mode {
            BarMode::Floating => self.height as i32 + 2 * FLOAT_GAP - self.margin,
            BarMode::Attached | BarMode::Auto => self.height as i32 - self.margin,
        }
    }

    fn apply_margin(&mut self, margin: i32, mode: BarMode) {
        if margin != self.margin || mode != self.mode {
            self.margin = margin;
            self.mode = mode;
            self.layer.set_margin(margin, margin, 0, margin);
            self.layer.set_exclusive_zone(self.exclusive_zone());
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
            self.layer.set_exclusive_zone(self.exclusive_zone());
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
        } else if let Some(i) = self.tray_at((x as f64, 0.0)) {
            Some(Button::Tray(i))
        } else if self.chevron_region.is_some_and(|(a, b)| x >= a && x < b) {
            Some(Button::Chevron)
        } else if self.layout_region.is_some_and(|(a, b)| x >= a && x < b) {
            Some(Button::Layout)
        } else if self.keyboard_region.is_some_and(|(a, b)| x >= a && x < b) {
            Some(Button::Keyboard)
        } else if self
            .clock_region
            .is_some_and(|(start, end)| x >= start && x < end)
        {
            Some(Button::Clock)
        } else {
            None
        }
    }

    /// The tray icon (an index into the tray's items) at `x`.
    pub fn tray_at(&self, (x, _y): (f64, f64)) -> Option<usize> {
        let x = x as f32;
        let &(i, ..) = self
            .tray_regions
            .iter()
            .find(|(_, a, b)| x >= *a && x < *b)?;
        Some(i)
    }

    /// Where the tray popover hangs from: the chevron, panel coordinates.
    pub fn chevron_anchor(&self) -> Option<(i32, i32, i32, i32)> {
        let (a, b) = self.chevron_region?;
        Some((a as i32, 0, (b - a) as i32, self.height as i32))
    }

    /// The first tray icon, for the debug aid.
    pub fn tray_at_first(&self) -> Option<usize> {
        self.tray_regions.first().map(|r| r.0)
    }

    /// Where a tray icon's menu hangs from: panel coordinates.
    pub fn tray_rect(&self, i: usize) -> Option<(i32, i32, i32, i32)> {
        let &(_, a, b) = self.tray_regions.iter().find(|(n, ..)| *n == i)?;
        Some((a as i32, 0, (b - a) as i32, self.height as i32))
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

    /// The islands drawn last, as frost cards for the compositor: each as
    /// tall as the bar, as visible as the islands are. Empty without islands.
    pub fn island_cards(&self) -> Vec<cnde_ipc::CardFrost> {
        let height = self.height as f64;
        self.islands
            .iter()
            .map(|&(a, b)| cnde_ipc::CardFrost {
                rect: [a as f64, 0.0, (b - a) as f64, height],
                alpha: self.island_alpha,
                radius: Some(height / 2.0),
            })
            .collect()
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
            self.layer.set_exclusive_zone(self.exclusive_zone());
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
        let radius = (h / 2.0).min((end - start) / 2.0);
        canvas.rounded_rect(start, inset, end - start, h, radius, color);
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
            tray,
            tray_mode,
            tray_popover_open,
            power_icon,
            mic_indicator,
            keyboard_button,
            keyboard_open,
            overview_open,
            quick_settings_open,
            calendar_open,
            unread,
            clock_position,
            battery_percent,
            battery_low,
            solid,
            mode,
            style,
            backdrop_light,
            tint,
            edge_line,
            layout,
            frosted,
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
        self.float.begin();
        // Islands only make sense off the edge.
        let mode = match (style, mode) {
            (PanelStyle::Islands, BarMode::Attached) => BarMode::Auto,
            _ => mode,
        };
        let clear = style == PanelStyle::Clear;
        let islands = style == PanelStyle::Islands;
        let floats = match mode {
            BarMode::Attached => false,
            BarMode::Floating => true,
            // Off the edge until a window touches it, while it's edited
            // flush so its edge can be dragged.
            BarMode::Auto => !solid,
        } && self.edit.is_none();
        let float = self.float.get((), if floats { 1.0 } else { 0.0 }, MEDIUM);
        // Set with this frame's buffer: the compositor cuts the frost to
        // the same shape in the same commit.
        self.apply_margin((FLOAT_GAP as f32 * float).round() as i32, mode);
        let solid = self
            .solid_fade
            .get((), if solid { 1.0 } else { 0.0 }, MEDIUM);
        self.shown = self.tint_fade.update(tint);
        let matched;
        let theme = match &self.shown {
            Some(shown) => {
                matched = shown.apply(theme);
                &matched
            }
            None => theme,
        };
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
        // Clear and floating, the text sits on the wallpaper itself: dark
        // on a light one, light on a dark one, easing back as it fills in.
        let on_wallpaper = if clear { float * (1.0 - solid) } else { 0.0 };
        let mut adjusted;
        let theme = if on_wallpaper > 0.0 {
            let wanted = if backdrop_light {
                Color::rgb(0x1b, 0x1d, 0x1a)
            } else {
                Color::rgb(0xff, 0xff, 0xff)
            };
            adjusted = theme.clone();
            let fg = theme.panel_foreground.mix(wanted, on_wallpaper);
            adjusted.panel_foreground = fg;
            adjusted.workspace_active = fg;
            adjusted.workspace_dot = fg.with_alpha(fg.a * 0.45);
            let ink = if backdrop_light { 0x00 } else { 0xff };
            adjusted.panel_button_hover = theme
                .panel_button_hover
                .mix(Color::rgba(ink, ink, ink, 0.12), on_wallpaper);
            adjusted.panel_button_active = theme
                .panel_button_active
                .mix(Color::rgba(ink, ink, ink, 0.2), on_wallpaper);
            &adjusted
        } else {
            theme
        };
        let background = {
            let a = theme.panel_background.a;
            // Clear and islands: fades out as it floats off, back in when
            // touched.
            let a = if clear || islands { a * (1.0 - float) } else { a };
            theme.panel_background.with_alpha(a + (1.0 - a) * solid)
        };
        // A soft shadow: rounded rects each a little taller and fainter.
        let shadow = if float > 0.0 { 0 } else { self.shadow };
        for i in 1..=shadow {
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
        if float > 0.0 {
            // Floating: rounded all round, the corners growing as it
            // leaves the edge.
            let r = theme.panel_float_radius * float;
            canvas.rounded_rect(0.0, 0.0, width, height, r, background);
        } else {
            // The bar: square at the top edge, rounded at the bottom (its
            // top corners are drawn off the surface).
            canvas.rounded_rect(
                0.0,
                -self.radius,
                width,
                height + self.radius,
                self.radius,
                background,
            );
        }
        // Taking the colour of a window against it, a shade along the
        // bottom keeps the bar apart from the window's own title bar.
        let line = edge_line as f32 * (1.0 - float);
        if self.shown.is_some() && line > 0.0 && solid > 0.0 {
            let shade = if background.luminance() > 0.4 {
                Color::rgba(0, 0, 0, 0.14 * solid)
            } else {
                Color::rgba(0xff, 0xff, 0xff, 0.12 * solid)
            };
            canvas.rounded_rect_clipped(
                0.0,
                -self.radius,
                width,
                height + self.radius,
                self.radius,
                shade,
                (0.0, height - line, width, line),
            );
        }

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
        if mic_indicator && status.audio.recording {
            items.push("audio-input-microphone-symbolic".into());
        }
        if let Some(battery) = status.battery {
            items.push(battery_icon(battery));
        }
        if power_icon {
            items.push("system-shutdown-symbolic".into());
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

        // Tray icons, just left of the status button: inline ones, and in
        // the collapsed style a chevron (and any wanting attention).
        self.tray_regions.clear();
        self.chevron_region = None;
        let (inline, hidden) = tray::split(tray, tray_mode);
        let gap = theme.status_spacing;
        let slot = icon + gap;
        let mut tray_start = region.0;
        let mut right = region.0 - TRAY_GAP;
        if !hidden.is_empty() {
            let a = right - slot;
            self.chevron_region = Some((a, right));
            self.button_background(
                &mut canvas,
                theme,
                (a, right),
                height,
                Button::Chevron,
                tray_popover_open,
            );
            let name = if tray_popover_open {
                "pan-up-symbolic"
            } else {
                "pan-down-symbolic"
            };
            canvas.icon(icons, name, a + gap / 2.0, icon_y, icon, fg);
            right = a;
            tray_start = a;
        }
        if !inline.is_empty() {
            tray_start = right - inline.len() as f32 * slot;
            let mut x = tray_start;
            for i in inline {
                self.button_background(
                    &mut canvas,
                    theme,
                    (x, x + slot),
                    height,
                    Button::Tray(i),
                    false,
                );
                tray[i].draw(&mut canvas, icons, (x + gap / 2.0, icon_y, icon), fg);
                self.tray_regions.push((i, x, x + slot));
                x += slot;
            }
        }

        // The keyboard layout, left of the tray.
        self.layout_region = None;
        if let Some(label) = layout {
            let mut text = fonts.layout(label, theme.font_size, Weight::Bold, scale as f32);
            let b = tray_start - if tray_start < region.0 { 0.0 } else { TRAY_GAP };
            let a = b - text.width - pad * 2.0;
            self.layout_region = Some((a, b));
            self.button_background(&mut canvas, theme, (a, b), height, Button::Layout, false);
            let y = ((height - text.height) / 2.0).round();
            canvas.text(fonts, &mut text, (a + pad).round(), y, fg);
            tray_start = a;
        }

        // The on-screen keyboard's button, left of the tray.
        self.keyboard_region = None;
        if keyboard_button && self.edit.is_none() {
            let a = tray_start - TRAY_GAP / 2.0 - slot;
            let b = a + slot;
            self.keyboard_region = Some((a, b));
            self.button_background(
                &mut canvas,
                theme,
                (a, b),
                height,
                Button::Keyboard,
                keyboard_open,
            );
            canvas.icon(icons, "input-keyboard-symbolic", a + gap / 2.0, icon_y, icon, fg);
            tray_start = a;
        }

        // Editing: a Done button left of the status icons.
        self.done_region = None;
        let mut right_limit = tray_start;
        if self.edit.is_some() {
            let mut label = fonts.layout("Done", theme.font_size, Weight::Bold, scale as f32);
            let bw = label.width + 2.0 * pad + 8.0;
            let bx = tray_start - 8.0 - bw;
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
        let slot_x = |position: ClockPosition| {
            clock_x_at(position, width, clock.width, activities.1, right_limit)
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
        if unread {
            canvas.circle(
                clock_x + clock.width + 7.0,
                (height / 2.0).round(),
                3.0,
                theme.accent,
            );
        }
        self.clock_region = Some(clock_region);

        // Islands: a pill behind each group, under the hover pills drawn
        // above. The regions (and so hit-testing) are the same as the bar's.
        let amount = if islands && self.edit.is_none() {
            float * (1.0 - solid)
        } else {
            0.0
        };
        self.islands.clear();
        self.island_alpha = amount;
        if amount > 0.0 {
            let bg = theme.panel_background;
            // Frosted, the islands take the bar's own lighter tint.
            let tint = if frosted { bg.a } else { bg.a.max(0.8) };
            let color = bg.with_alpha(tint * amount);
            let spans = island_spans(
                &[
                    activities,
                    clock_region,
                    (tray_start, self.status_region.map_or(tray_start, |r| r.1)),
                ],
                ISLAND_PAD,
                ISLAND_MERGE,
            );
            for &(a, b) in &spans {
                canvas.rounded_rect_behind(a, 0.0, b - a, height, height / 2.0, color);
            }
            self.islands = spans;
        }

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
        if self.anim.running()
            || self.solid_fade.running()
            || self.tint_fade.running()
            || self.float.running()
        {
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

/// The bar's label for each keyboard layout: its code ("us" is "US"), or
/// where two share a code, the start of what tells them apart ("English
/// (Dvorak)" is "DV").
pub fn layout_labels(codes: &[String], names: &[String]) -> Vec<String> {
    codes
        .iter()
        .zip(names)
        .map(|(code, name)| {
            let shared = codes.iter().filter(|c| *c == code).count() > 1;
            let detail = name
                .split_once('(')
                .map(|(_, rest)| rest.trim_end_matches(')'));
            let label = match detail {
                Some(detail) if shared || code.is_empty() => detail,
                _ if code.is_empty() => name.as_str(),
                _ => code.as_str(),
            };
            label.chars().take(2).collect::<String>().to_uppercase()
        })
        .collect()
}

/// How far an island reaches past the buttons it holds.
const ISLAND_PAD: f32 = 4.0;
/// Islands closer than this (after padding) are one.
const ISLAND_MERGE: f32 = 12.0;
/// Between the clock and the dots or status icons beside it.
const CLOCK_GAP: f32 = 16.0;

/// Where the clock's text starts for a slot: `act_end` is the right end of
/// the Activities button, `right_limit` the left end of what's on the right.
fn clock_x_at(
    position: ClockPosition,
    width: f32,
    clock_width: f32,
    act_end: f32,
    right_limit: f32,
) -> f32 {
    match position {
        ClockPosition::Left => act_end + CLOCK_GAP,
        ClockPosition::Center => (width - clock_width) / 2.0,
        ClockPosition::Right => right_limit - CLOCK_GAP - clock_width,
    }
}

/// The islands for groups of buttons (x ranges): each grown by `pad`, and
/// ones within `merge` of each other joined, so a clock beside the dots
/// shares their pill.
fn island_spans(groups: &[(f32, f32)], pad: f32, merge: f32) -> Vec<(f32, f32)> {
    let mut sorted: Vec<(f32, f32)> = groups.iter().map(|&(a, b)| (a - pad, b + pad)).collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f32, f32)> = Vec::new();
    for span in sorted {
        match out.last_mut() {
            Some(last) if span.0 - last.1 <= merge => last.1 = last.1.max(span.1),
            _ => out.push(span),
        }
    }
    out
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
    fn layouts_are_labelled_apart() {
        let codes = ["us", "us", "de", "gr"].map(String::from);
        let names = ["English (US)", "English (Dvorak)", "German", "Greek"].map(String::from);
        assert_eq!(layout_labels(&codes, &names), ["US", "DV", "DE", "GR"]);
    }

    #[test]
    fn centre_clock_gets_its_own_island() {
        let width = 1000.0;
        let clock = 100.0;
        let x = clock_x_at(ClockPosition::Center, width, clock, 60.0, 900.0);
        let spans = island_spans(
            &[(7.0, 60.0), (x - 10.0, x + clock + 10.0), (900.0, 995.0)],
            ISLAND_PAD,
            ISLAND_MERGE,
        );
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0], (3.0, 64.0));
        assert_eq!(spans[2], (896.0, 999.0));
    }

    #[test]
    fn side_clock_joins_its_neighbour() {
        let x = clock_x_at(ClockPosition::Left, 1000.0, 100.0, 60.0, 900.0);
        assert_eq!(x, 76.0);
        let spans = island_spans(
            &[(7.0, 60.0), (x - 10.0, x + 110.0), (900.0, 995.0)],
            ISLAND_PAD,
            ISLAND_MERGE,
        );
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0], (3.0, 190.0));
        let x = clock_x_at(ClockPosition::Right, 1000.0, 100.0, 60.0, 900.0);
        assert_eq!(x, 784.0);
    }

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
