//! Quick settings: the iOS-Control-Center-looking card under the status
//! icons (ARCHITECTURE.md §5.2). An xdg_popup of the panel, so the
//! compositor grabs input for it: clicking anywhere else closes it, and
//! it gets the keyboard (Escape closes it).

use std::time::{Duration, Instant};

use cnde_ui::{
    Animator, Canvas, Color, Fonts, Icons, Theme, Weight,
    anim::{MEDIUM, SHORT},
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
    App, Status,
    actions::{Action, Actions},
    panel::volume_icon,
};

/// What a point in the card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Volume,
    Mute,
    Brightness,
    /// Index into the available power profiles.
    PowerProfile(usize),
    Wifi,
    Bluetooth,
    DarkMode,
    DoNotDisturb,
    Power,
    /// Opens cnde-settings.
    Settings,
    /// Opens cnde-settings on a page (a section's label or arrow).
    SettingsPage(&'static str),
    Lock,
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
    pub brightness: Option<Rect>,
    /// The power mode tile and its segments.
    pub power_profiles: Option<(Rect, Vec<Rect>)>,
    pub footer_y: f32,
    pub power_button: Rect,
    pub settings_button: Rect,
    /// What opens a section's page in cnde-settings: the labels under
    /// toggles that have one, the arrow beside the brightness slider.
    pub settings_links: Vec<(Rect, &'static str)>,
    pub power_menu: Option<Vec<(Rect, Hit)>>,
}

/// The cnde-settings page for a section of the card, if it has one.
fn settings_page(hit: Hit) -> Option<&'static str> {
    match hit {
        Hit::Wifi => Some("wifi"),
        Hit::DarkMode => Some("appearance"),
        Hit::Brightness => Some("displays"),
        _ => None,
    }
}

const FOOTER_HEIGHT: f32 = 40.0;
const POWER_TILE_HEIGHT: f32 = 64.0;
const LABEL_GAP: f32 = 6.0;
const PROFILE_TILE_HEIGHT: f32 = 64.0;
/// The arrow after a label that opens a settings page.
const LINK_ARROW: f32 = 10.0;

/// Which optional parts the card has, from what the machine offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub brightness: bool,
    /// How many power modes to offer (0: no power-profiles-daemon).
    pub power_profiles: usize,
}

impl Features {
    pub fn of(status: &Status) -> Self {
        Self {
            brightness: status.backlight.is_some(),
            power_profiles: status
                .power_profiles
                .as_ref()
                .map_or(0, |p| p.available.len()),
        }
    }
}

pub fn layout(theme: &Theme, power_menu_open: bool, features: Features) -> Layout {
    // No ways into cnde-settings when it isn't installed.
    let settings = cfg!(test) || crate::apps::settings_installed();
    let mut settings_links = Vec::new();
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
            if settings && let Some(page) = settings_page(hit) {
                let label = Rect {
                    x: pad + slot * i as f32,
                    y: y + 14.0 + toggle + LABEL_GAP - 3.0,
                    w: slot,
                    h: theme.qs_label_size * 1.4 + 6.0,
                };
                settings_links.push((label, page));
            }
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

    let brightness = features.brightness.then(|| {
        let h = theme.qs_slider_height;
        let mut rect = Rect {
            x: pad,
            y,
            w: inner,
            h,
        };
        // A round arrow to the Displays page at its end.
        if settings && let Some(page) = settings_page(Hit::Brightness) {
            rect.w -= h + theme.qs_gap;
            let arrow = Rect {
                x: pad + inner - h,
                y,
                w: h,
                h,
            };
            settings_links.push((arrow, page));
        }
        y += theme.qs_slider_height + theme.qs_gap;
        rect
    });

    let power_profiles = (features.power_profiles > 0).then(|| {
        let tile = Rect {
            x: pad,
            y,
            w: inner,
            h: PROFILE_TILE_HEIGHT,
        };
        let n = features.power_profiles as f32;
        let inset = 6.0;
        let seg_w = (inner - 2.0 * inset) / n;
        let segments = (0..features.power_profiles)
            .map(|i| Rect {
                x: pad + inset + i as f32 * seg_w,
                y: y + inset,
                w: seg_w,
                h: PROFILE_TILE_HEIGHT - 2.0 * inset,
            })
            .collect();
        y += PROFILE_TILE_HEIGHT + theme.qs_gap;
        (tile, segments)
    });

    let power_menu = power_menu_open.then(|| {
        let gap = theme.qs_gap;
        let tile_w = (inner - 3.0 * gap) / 4.0;
        let items = [Hit::Lock, Hit::Suspend, Hit::Reboot, Hit::PowerOff]
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
    // No gear when cnde-settings isn't installed.
    let settings_button = if settings {
        Rect {
            x: power_button.x - theme.qs_gap - FOOTER_HEIGHT,
            ..power_button
        }
    } else {
        Rect {
            w: 0.0,
            h: 0.0,
            ..power_button
        }
    };
    y += FOOTER_HEIGHT + pad;

    Layout {
        width,
        height: y,
        toggle_tile,
        toggles,
        volume,
        brightness,
        power_profiles,
        footer_y,
        power_button,
        settings_button,
        settings_links,
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
        if let Some((_, page)) = self.settings_links.iter().find(|(r, _)| r.contains(point)) {
            return Some(Hit::SettingsPage(page));
        }
        if self.brightness.is_some_and(|b| b.contains(point)) {
            return Some(Hit::Brightness);
        }
        if let Some((_, segments)) = &self.power_profiles
            && let Some(i) = segments.iter().position(|r| r.contains(point))
        {
            return Some(Hit::PowerProfile(i));
        }
        self.toggles
            .iter()
            .chain(self.power_menu.iter().flatten())
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, hit)| *hit)
            .or_else(|| self.power_button.contains(point).then_some(Hit::Power))
            .or_else(|| {
                self.settings_button
                    .contains(point)
                    .then_some(Hit::Settings)
            })
    }

    /// Volume (0-100) for a pointer at `x` over the slider.
    pub fn volume_at(&self, x: f64) -> u32 {
        percent_along(self.volume, x)
    }

    pub fn brightness_at(&self, x: f64) -> Option<u32> {
        self.brightness.map(|b| percent_along(b, x))
    }
}

fn percent_along(rect: Rect, x: f64) -> u32 {
    (((x as f32 - rect.x) / rect.w).clamp(0.0, 1.0) * 100.0).round() as u32
}

/// What's animating in the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Moving {
    /// A round toggle fading between off and on (index into the toggles).
    Toggle(usize),
    Slider(Slider),
    /// The power mode highlight sliding to the active mode.
    Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slider {
    Volume,
    Brightness,
}

impl Layout {
    fn slider_at(&self, slider: Slider, x: f64) -> Option<u32> {
        match slider {
            Slider::Volume => Some(self.volume_at(x)),
            Slider::Brightness => self.brightness_at(x),
        }
    }
}

pub struct QuickSettings {
    pub popup: Popup,
    scale: i32,
    configured: bool,
    layout: Layout,
    features: Features,
    dragging: Option<Slider>,
    /// What the user just set, shown until the audio server reports it
    /// back (so the slider tracks the pointer with no lag).
    volume_override: Option<(u32, Instant)>,
    /// Toggles show their new state as soon as they're pressed; if the
    /// system hasn't confirmed it within `CONFIRM_WITHIN`, they snap back
    /// (the change was refused or failed).
    wifi_override: Option<(bool, Instant)>,
    profile_override: Option<(crate::status::PowerProfile, Instant)>,
    /// Brightness has no change notification: what we set is what it is.
    brightness: Option<u32>,
    /// A frame callback is outstanding: draw when it fires, not before.
    /// Pointer events can arrive far faster than the screen refreshes (1 kHz
    /// mice), and painting each one would fall behind.
    frame_pending: bool,
    /// Something changed since the last draw.
    dirty: bool,
    anim: Animator<Moving>,
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
        status: &Status,
        scale: i32,
    ) -> Option<Self> {
        let features = Features::of(status);
        let layout = layout(theme, false, features);
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
            features,
            dragging: None,
            volume_override: None,
            wifi_override: None,
            profile_override: None,
            brightness: status.backlight.as_ref().map(|b| b.percent()),
            frame_pending: false,
            dirty: false,
            anim: Animator::new(),
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

    /// Show or hide the power row; the popup grows or shrinks to fit.
    fn toggle_power_menu(
        &mut self,
        xdg_shell: &XdgShell,
        theme: &Theme,
        anchor: (i32, i32, i32, i32),
    ) {
        self.layout = layout(theme, self.layout.power_menu.is_none(), self.features);
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
                self.dragging = Some(Slider::Volume);
                self.slide(Slider::Volume, point.0, actions);
            }
            Some(Hit::Brightness) => {
                self.dragging = Some(Slider::Brightness);
                self.slide(Slider::Brightness, point.0, actions);
            }
            Some(Hit::PowerProfile(i)) => {
                if let Some(profile) = status
                    .power_profiles
                    .as_ref()
                    .and_then(|p| p.available.get(i))
                {
                    self.profile_override = Some((*profile, Instant::now()));
                    actions.run(Action::SetPowerProfile(*profile));
                }
            }
            Some(Hit::Mute) => actions.run(Action::ToggleMute),
            Some(Hit::Wifi) => {
                let shown = self.wifi_override.map(|(on, _)| on).or(status.wifi_enabled);
                if let Some(enabled) = shown {
                    self.wifi_override = Some((!enabled, Instant::now()));
                    actions.run(Action::SetWifi(!enabled));
                }
            }
            Some(Hit::Power) => self.toggle_power_menu(xdg_shell, theme, anchor),
            Some(Hit::Settings) => {
                crate::apps::open_settings(None);
                return true;
            }
            Some(Hit::SettingsPage(page)) => {
                crate::apps::open_settings(Some(page));
                return true;
            }
            Some(hit @ (Hit::Lock | Hit::Suspend | Hit::Reboot | Hit::PowerOff)) => {
                actions.run(match hit {
                    Hit::Lock => Action::Lock,
                    Hit::Suspend => Action::Suspend,
                    Hit::Reboot => Action::Reboot,
                    _ => Action::PowerOff,
                });
                return true;
            }
            Some(Hit::DarkMode) => crate::config::set_dark(!theme.dark),
            // Not wired up yet (Bluetooth, notifications).
            Some(Hit::Bluetooth | Hit::DoNotDisturb) | None => {}
        }
        false
    }

    pub fn motion(&mut self, point: (f64, f64), actions: &Actions) {
        if let Some(slider) = self.dragging {
            self.slide(slider, point.0, actions);
        }
    }

    pub fn release(&mut self) {
        self.dragging = None;
    }

    fn slide(&mut self, slider: Slider, x: f64, actions: &Actions) {
        let Some(percent) = self.layout.slider_at(slider, x) else {
            return;
        };
        match slider {
            Slider::Volume => {
                if self.volume_override.map(|(v, _)| v) != Some(percent) {
                    self.volume_override = Some((percent, Instant::now()));
                    actions.run(Action::SetVolume(percent));
                }
            }
            Slider::Brightness => {
                if self.brightness != Some(percent) {
                    self.brightness = Some(percent);
                    actions.run(Action::SetBrightness(percent));
                }
            }
        }
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
        status: &Status,
    ) {
        if !self.configured || self.frame_pending {
            self.dirty = true;
            return;
        }
        self.dirty = false;
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
            && self.dragging != Some(Slider::Volume)
        {
            self.volume_override = None;
        }
        // Pressed toggles, until the system confirms or the wait runs out.
        let mut shown = status.clone();
        let fresh = |since: Instant| since.elapsed() < CONFIRM_WITHIN;
        self.wifi_override = self
            .wifi_override
            .filter(|&(on, since)| fresh(since) && status.wifi_enabled != Some(on));
        if let Some((on, _)) = self.wifi_override {
            shown.wifi_enabled = Some(on);
        }
        self.profile_override = self.profile_override.filter(|&(profile, since)| {
            fresh(since)
                && status
                    .power_profiles
                    .as_ref()
                    .is_some_and(|p| p.active != profile)
        });
        if let (Some((profile, _)), Some(profiles)) =
            (self.profile_override, &mut shown.power_profiles)
        {
            profiles.active = profile;
        }
        let status = &shown;
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
            self.brightness,
            scale as f32,
            &mut self.anim,
            self.dragging,
        );
        // Still moving: draw again when this frame is shown.
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

/// How long a pressed toggle waits for the system to confirm the change.
pub const CONFIRM_WITHIN: Duration = Duration::from_secs(3);

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
    brightness: Option<u32>,
    s: f32,
    anim: &mut Animator<Moving>,
    dragging: Option<Slider>,
) {
    anim.begin();
    // Sliders glide to a clicked spot, but follow the pointer while dragged.
    let mut slide = |slider: Slider, value: f32| {
        if dragging == Some(slider) {
            anim.set(Moving::Slider(slider), value)
        } else {
            anim.get(Moving::Slider(slider), value, SHORT)
        }
    };
    let volume_shown = slide(Slider::Volume, volume.0 as f32 / 100.0);
    let brightness_shown = brightness.map(|b| slide(Slider::Brightness, b as f32 / 100.0));
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
    for (i, (rect, hit)) in layout.toggles.iter().enumerate() {
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
                theme.dark,
                true,
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
        let lit = anim.get(Moving::Toggle(i), if on { 1.0 } else { 0.0 }, SHORT);
        let circle = theme.qs_toggle_off.mix(theme.qs_toggle_on, lit);
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
            {
                let fg = theme.popup_text.mix(theme.on_accent, lit);
                fg.with_alpha(fg.a * fade)
            },
        );
        // A label that opens the section's settings page ends in an arrow.
        let linked = settings_page(*hit)
            .is_some_and(|page| layout.settings_links.iter().any(|(_, p)| *p == page));
        let arrow = if linked { LINK_ARROW + 2.0 } else { 0.0 };
        // Fit the label under its toggle: a little smaller if need be,
        // shortened only as a last resort (long network names).
        let room = layout.toggle_tile.w / layout.toggles.len() as f32 - 4.0 - arrow;
        let mut text = fonts.layout(&label, theme.qs_label_size, Weight::Normal, s);
        let mut size = theme.qs_label_size;
        while text.width > room && size > theme.qs_label_size - 1.5 {
            size -= 0.5;
            text = fonts.layout(&label, size, Weight::Normal, s);
        }
        if text.width > room {
            text = fonts.layout(&ellipsize(&label, 13), size, Weight::Normal, s);
        }
        let tx = (rect.x + rect.w / 2.0 - (text.width + arrow) / 2.0).round();
        let ty = rect.y + rect.h + LABEL_GAP;
        let dim = theme.qs_dim.with_alpha(theme.qs_dim.a * fade);
        if linked {
            canvas.icon(
                icons,
                "go-next-symbolic",
                tx + text.width + 2.0,
                ty + (text.height - LINK_ARROW) / 2.0,
                LINK_ARROW,
                dim,
            );
        }
        canvas.text(fonts, &mut text, tx, ty, dim);
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
        volume_shown,
        theme.qs_slider_track,
        fill,
    );
    let icon_size = 20.0;
    let icon_x = v.x + (v.h - icon_size) / 2.0 + 4.0;
    // Dark icon where the bright fill is under it.
    let covered = volume_shown * v.w > icon_x - v.x + icon_size;
    let icon_color = if covered && !muted {
        theme.qs_on_fill
    } else {
        theme.popup_text
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

    // Brightness: the same fat slider, a sun inside.
    if let (Some(b), Some(shown)) = (layout.brightness, brightness_shown) {
        canvas.slider(
            b.x,
            b.y,
            b.w,
            b.h,
            shown,
            theme.qs_slider_track,
            theme.qs_slider_fill,
        );
        let icon_x = b.x + (b.h - icon_size) / 2.0 + 4.0;
        let covered = shown * b.w > icon_x - b.x + icon_size;
        canvas.icon(
            icons,
            "display-brightness-symbolic",
            icon_x,
            b.y + (b.h - icon_size) / 2.0,
            icon_size,
            if covered {
                theme.qs_on_fill
            } else {
                theme.popup_text
            },
        );
    }

    // The arrow beside the brightness slider, to the Displays page.
    if let Some((rect, _)) = layout
        .settings_links
        .iter()
        .find(|(_, page)| Some(*page) == settings_page(Hit::Brightness))
    {
        canvas.circle(
            rect.x + rect.w / 2.0,
            rect.y + rect.h / 2.0,
            rect.w / 2.0,
            theme.qs_slider_track,
        );
        canvas.icon(
            icons,
            "go-next-symbolic",
            rect.x + (rect.w - 16.0) / 2.0,
            rect.y + (rect.h - 16.0) / 2.0,
            16.0,
            theme.popup_text,
        );
    }

    // Power mode: one segment per profile, the active one lit.
    if let (Some((tile, segments)), Some(profiles)) =
        (&layout.power_profiles, &status.power_profiles)
    {
        canvas.rounded_rect(
            tile.x,
            tile.y,
            tile.w,
            tile.h,
            theme.qs_tile_radius,
            theme.qs_tile,
        );
        // The highlight slides from mode to mode.
        if let Some(index) = profiles
            .available
            .iter()
            .position(|p| *p == profiles.active)
            && let Some(first) = segments.first()
        {
            let pos = anim.get(Moving::Profile, index as f32, MEDIUM);
            canvas.rounded_rect(
                first.x + pos * first.w,
                first.y,
                first.w,
                first.h,
                theme.qs_tile_radius - 6.0,
                theme.qs_toggle_on,
            );
        }
        for (rect, profile) in segments.iter().zip(&profiles.available) {
            let active = *profile == profiles.active;
            let (icon, label) = match profile {
                crate::status::PowerProfile::PowerSaver => {
                    ("power-profile-power-saver-symbolic", "Power Saver")
                }
                crate::status::PowerProfile::Balanced => {
                    ("power-profile-balanced-symbolic", "Balanced")
                }
                crate::status::PowerProfile::Performance => {
                    ("power-profile-performance-symbolic", "Performance")
                }
            };
            let fg = if active {
                theme.on_accent
            } else {
                theme.qs_dim
            };
            canvas.icon(
                icons,
                icon,
                rect.x + (rect.w - 18.0) / 2.0,
                rect.y + 7.0,
                18.0,
                fg,
            );
            let mut text = fonts.layout(label, theme.qs_label_size, Weight::Normal, s);
            let tx = (rect.x + (rect.w - text.width) / 2.0).round();
            canvas.text(fonts, &mut text, tx, rect.y + 29.0, fg);
        }
    }

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
            Hit::Lock => ("system-lock-screen-symbolic", "Lock"),
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
            theme.popup_text,
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
    let g = layout.settings_button;
    if g.w > 0.0 {
        canvas.circle(
            g.x + g.w / 2.0,
            g.y + g.h / 2.0,
            g.w / 2.0,
            theme.qs_toggle_off,
        );
        canvas.icon(
            icons,
            "emblem-system-symbolic",
            g.x + (g.w - 18.0) / 2.0,
            g.y + (g.h - 18.0) / 2.0,
            18.0,
            theme.popup_text,
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
        if layout.power_menu.is_some() {
            theme.on_accent
        } else {
            theme.popup_text
        },
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
        let l = layout(&theme, false, Features::default());
        let centre = |r: Rect| ((r.x + r.w / 2.0) as f64, (r.y + r.h / 2.0) as f64);
        assert_eq!(l.hit(centre(l.toggles[0].0)), Some(Hit::Wifi));
        assert_eq!(
            l.hit((l.volume.x as f64 + 10.0, centre(l.volume).1)),
            Some(Hit::Mute)
        );
        assert_eq!(l.hit(centre(l.volume)), Some(Hit::Volume));
        assert_eq!(l.hit(centre(l.power_button)), Some(Hit::Power));
        assert_eq!(l.hit(centre(l.settings_button)), Some(Hit::Settings));
        assert!(l.power_menu.is_none());
    }

    #[test]
    fn power_menu_grows_the_card() {
        let theme = Theme::dark();
        let closed = layout(&theme, false, Features::default());
        let open = layout(&theme, true, Features::default());
        assert!(open.height > closed.height);
        let reboot = open.power_menu.as_ref().unwrap()[2].0;
        assert_eq!(
            open.hit(((reboot.x + 5.0) as f64, (reboot.y + 5.0) as f64)),
            Some(Hit::Reboot)
        );
    }

    #[test]
    fn laptop_parts_hit() {
        let features = Features {
            brightness: true,
            power_profiles: 3,
        };
        let l = layout(&Theme::dark(), false, features);
        let b = l.brightness.unwrap();
        assert_eq!(
            l.hit(((b.x + b.w / 2.0) as f64, (b.y + 4.0) as f64)),
            Some(Hit::Brightness)
        );
        let (_, segments) = l.power_profiles.as_ref().unwrap();
        let last = segments[2];
        assert_eq!(
            l.hit(((last.x + 4.0) as f64, (last.y + 4.0) as f64)),
            Some(Hit::PowerProfile(2))
        );
        assert!(l.height > layout(&Theme::dark(), false, Features::default()).height);
    }

    #[test]
    fn sections_open_their_settings_pages() {
        let features = Features {
            brightness: true,
            power_profiles: 0,
        };
        let l = layout(&Theme::dark(), false, features);
        let (wifi, _) = l.toggles[0];
        let label = (
            (wifi.x + wifi.w / 2.0) as f64,
            (wifi.y + wifi.h + LABEL_GAP + 4.0) as f64,
        );
        assert_eq!(l.hit(label), Some(Hit::SettingsPage("wifi")));
        let b = l.brightness.unwrap();
        let end = l.toggle_tile.x + l.toggle_tile.w;
        assert!(b.x + b.w < end - b.h);
        assert_eq!(
            l.hit(((end - b.h / 2.0) as f64, (b.y + b.h / 2.0) as f64)),
            Some(Hit::SettingsPage("displays"))
        );
        // Bluetooth has no page.
        let (bt, _) = l.toggles[1];
        let label = (
            (bt.x + bt.w / 2.0) as f64,
            (bt.y + bt.h + LABEL_GAP + 4.0) as f64,
        );
        assert_eq!(l.hit(label), None);
    }

    #[test]
    fn slider_maps_x_to_percent() {
        let l = layout(&Theme::dark(), false, Features::default());
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
            battery: Some(crate::status::Battery {
                percent: 87.0,
                charging: true,
                full: false,
            }),
            network: crate::status::Network::Wifi {
                ssid: "Homenet".into(),
                strength: 80,
            },
            wifi_enabled: Some(true),
            volume: Some(crate::status::Volume {
                percent: 62,
                muted: false,
            }),
            power_profiles: Some(crate::status::PowerProfiles {
                active: crate::status::PowerProfile::Balanced,
                available: crate::status::PowerProfile::ALL.to_vec(),
            }),
            backlight: Some(crate::status::Backlight {
                path: "/sys/class/backlight/x".into(),
                max: 100,
                current: 40,
            }),
        };
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let layout = layout(
            &theme,
            std::env::var_os("CNDE_QS_POWER").is_some(),
            Features::of(&status),
        );
        let scale = 2.0;
        let (w, h) = (
            (layout.width * scale) as u32,
            (layout.height * scale) as u32,
        );
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        {
            let mut canvas = Canvas::new(&mut pixels, w, h, scale).unwrap();
            paint(
                &mut canvas,
                &layout,
                &mut fonts,
                &mut icons,
                &theme,
                &status,
                (62, false),
                Some(40),
                scale,
                &mut Animator::new(),
                None,
            );
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
