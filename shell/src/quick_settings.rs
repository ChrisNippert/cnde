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
    /// The Bluetooth devices tile's header: opens or closes the list.
    BluetoothDevices,
    /// A paired device in the open list (index into the status's
    /// devices): connects or disconnects it.
    BluetoothDevice(usize),
    DarkMode,
    DoNotDisturb,
    Power,
    /// Opens cnde-settings.
    Settings,
    /// Opens cnde-settings on a page (a section's label or arrow).
    SettingsPage(&'static str),
    Lock,
    LogOut,
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
    /// Paired Bluetooth devices, while Bluetooth is on and has some.
    pub devices: Option<DeviceList>,
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

/// The Bluetooth devices tile: a header row that opens it, and a row per
/// device when it's open.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceList {
    pub tile: Rect,
    pub header: Rect,
    pub rows: Vec<Rect>,
}

/// The cnde-settings page for a section of the card, if it has one.
fn settings_page(hit: Hit) -> Option<&'static str> {
    match hit {
        Hit::Wifi => Some("wifi"),
        Hit::Bluetooth => Some("bluetooth"),
        Hit::DarkMode => Some("appearance"),
        Hit::Brightness => Some("displays"),
        _ => None,
    }
}

const FOOTER_HEIGHT: f32 = 40.0;
const POWER_TILE_HEIGHT: f32 = 64.0;
/// A toggle tile: two columns of these.
const TOGGLE_TILE_HEIGHT: f32 = 56.0;
const TOGGLE_GAP: f32 = 8.0;
/// The icon chip inside a toggle tile.
const CHIP: f32 = 36.0;
/// The chevron zone at a toggle tile's right end, to its Settings page.
const CHEVRON_ZONE: f32 = 28.0;
const SLIDER_HEIGHT: f32 = 40.0;
/// The power mode control: inner padding around the segments.
const PROFILE_INSET: f32 = 4.0;
const PROFILE_SEGMENT_HEIGHT: f32 = 36.0;
const DEVICE_ROW_HEIGHT: f32 = 40.0;
/// More devices than this are left to the Settings page.
const MAX_DEVICES: usize = 6;

/// Which optional parts the card has, from what the machine offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub brightness: bool,
    /// How many power modes to offer (0: no power-profiles-daemon).
    pub power_profiles: usize,
    /// Paired Bluetooth devices to list (0: no list, e.g. Bluetooth's off).
    pub bluetooth_devices: usize,
}

impl Features {
    pub fn of(status: &Status) -> Self {
        Self {
            brightness: status.backlight.is_some(),
            power_profiles: status
                .power_profiles
                .as_ref()
                .map_or(0, |p| p.available.len()),
            bluetooth_devices: status
                .bluetooth
                .as_ref()
                .filter(|b| b.powered)
                .map_or(0, |b| b.devices.len().min(MAX_DEVICES)),
        }
    }
}

pub fn layout(
    theme: &Theme,
    power_menu_open: bool,
    devices_open: bool,
    features: Features,
) -> Layout {
    // No ways into cnde-settings when it isn't installed.
    let settings = cfg!(test) || crate::apps::settings_installed();
    let mut settings_links = Vec::new();
    let width = theme.qs_width;
    let pad = theme.qs_padding;
    let inner = width - 2.0 * pad;
    let mut y = pad;

    // Two columns of labelled tiles.
    let kinds = [Hit::Wifi, Hit::Bluetooth, Hit::DarkMode, Hit::DoNotDisturb];
    let tile_w = (inner - TOGGLE_GAP) / 2.0;
    let rows = kinds.len().div_ceil(2) as f32;
    let tile_h = rows * TOGGLE_TILE_HEIGHT + (rows - 1.0) * TOGGLE_GAP;
    let toggle_tile = Rect {
        x: pad,
        y,
        w: inner,
        h: tile_h,
    };
    let toggles = kinds
        .iter()
        .enumerate()
        .map(|(i, &hit)| {
            let rect = Rect {
                x: pad + (i % 2) as f32 * (tile_w + TOGGLE_GAP),
                y: y + (i / 2) as f32 * (TOGGLE_TILE_HEIGHT + TOGGLE_GAP),
                w: tile_w,
                h: TOGGLE_TILE_HEIGHT,
            };
            if settings && let Some(page) = settings_page(hit) {
                let chevron = Rect {
                    x: rect.x + rect.w - CHEVRON_ZONE,
                    w: CHEVRON_ZONE,
                    ..rect
                };
                settings_links.push((chevron, page));
            }
            (rect, hit)
        })
        .collect();
    y += tile_h + theme.qs_gap;

    // Paired Bluetooth devices: a header row, the devices under it if open.
    let devices = (features.bluetooth_devices > 0).then(|| {
        let shown = if devices_open {
            features.bluetooth_devices
        } else {
            0
        };
        let row = |i: usize| Rect {
            x: pad,
            y: y + i as f32 * DEVICE_ROW_HEIGHT,
            w: inner,
            h: DEVICE_ROW_HEIGHT,
        };
        let list = DeviceList {
            tile: Rect {
                h: DEVICE_ROW_HEIGHT * (shown + 1) as f32,
                ..row(0)
            },
            header: row(0),
            rows: (1..=shown).map(row).collect(),
        };
        y += list.tile.h + theme.qs_gap;
        list
    });

    let volume = Rect {
        x: pad,
        y,
        w: inner,
        h: SLIDER_HEIGHT,
    };
    y += SLIDER_HEIGHT + theme.qs_gap;

    let brightness = features.brightness.then(|| {
        let h = SLIDER_HEIGHT;
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
        y += SLIDER_HEIGHT + theme.qs_gap;
        rect
    });

    let power_profiles = (features.power_profiles > 0).then(|| {
        let h = PROFILE_SEGMENT_HEIGHT + 2.0 * PROFILE_INSET;
        let tile = Rect {
            x: pad,
            y,
            w: inner,
            h,
        };
        let n = features.power_profiles as f32;
        let seg_w = (inner - 2.0 * PROFILE_INSET) / n;
        let segments = (0..features.power_profiles)
            .map(|i| Rect {
                x: pad + PROFILE_INSET + i as f32 * seg_w,
                y: y + PROFILE_INSET,
                w: seg_w,
                h: PROFILE_SEGMENT_HEIGHT,
            })
            .collect();
        y += h + theme.qs_gap;
        (tile, segments)
    });

    let power_menu = power_menu_open.then(|| {
        let gap = theme.qs_gap;
        let tile_w = (inner - 3.0 * gap) / 4.0;
        let items = [Hit::Lock, Hit::LogOut, Hit::Reboot, Hit::PowerOff]
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
        devices,
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
        if let Some(list) = &self.devices {
            if list.header.contains(point) {
                return Some(Hit::BluetoothDevices);
            }
            if let Some(i) = list.rows.iter().position(|r| r.contains(point)) {
                return Some(Hit::BluetoothDevice(i));
            }
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
    /// The power row opening (1) or closing (0).
    PowerMenu,
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
    bluetooth_override: Option<(bool, Instant)>,
    /// A device being connected (true) or disconnected, by address; it
    /// says so until BlueZ reports it done or `CONNECT_WITHIN` runs out.
    device_override: Option<(String, bool, Instant)>,
    /// The Bluetooth devices list is open.
    devices_open: bool,
    /// The panel-surface rect the card hangs from.
    anchor: (i32, i32, i32, i32),
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
    /// The power row is sliding shut: the open layout to keep painting
    /// until it's gone, then the smaller size to shrink the popup to.
    collapsing: Option<(Layout, XdgPositioner)>,
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
        let layout = layout(theme, false, false, features);
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
            bluetooth_override: None,
            device_override: None,
            devices_open: false,
            anchor,
            profile_override: None,
            brightness: status.backlight.as_ref().map(|b| b.percent()),
            frame_pending: false,
            dirty: false,
            anim: Animator::new(),
            collapsing: None,
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

    /// Show or hide the power row. Opening grows the popup at once and the
    /// row slides out into the room; closing slides it away first and
    /// shrinks the popup once it's gone (see `draw`).
    fn toggle_power_menu(
        &mut self,
        xdg_shell: &XdgShell,
        theme: &Theme,
        anchor: (i32, i32, i32, i32),
    ) {
        let open = self.layout.power_menu.is_none();
        let new = layout(theme, open, self.devices_open, self.features);
        let positioner = positioner(xdg_shell, &new, anchor);
        let old = std::mem::replace(&mut self.layout, new);
        if open {
            self.collapsing = None;
            if let Some(positioner) = positioner {
                self.popup.reposition(&positioner, 0);
            }
        } else if let Some(positioner) = positioner {
            self.collapsing = Some((old, positioner));
        }
    }

    /// Lay the card out again (a list opened or closed, or what the
    /// machine offers changed), resizing the popup at once.
    fn relayout(&mut self, xdg_shell: &XdgShell, theme: &Theme) {
        let new = layout(
            theme,
            self.layout.power_menu.is_some(),
            self.devices_open,
            self.features,
        );
        if new == self.layout {
            return;
        }
        self.layout = new;
        self.collapsing = None;
        if let Some(positioner) = positioner(xdg_shell, &self.layout, self.anchor) {
            self.popup.reposition(&positioner, 0);
        }
    }

    /// The status changed: grow or shrink for parts that came or went (the
    /// Bluetooth devices, as it's switched on and off).
    pub fn status_changed(&mut self, xdg_shell: &XdgShell, theme: &Theme, status: &Status) {
        let features = Features {
            // Read only when the card opens.
            brightness: self.features.brightness,
            ..Features::of(status)
        };
        if features != self.features {
            self.features = features;
            self.relayout(xdg_shell, theme);
        }
    }

    /// A pressed toggle or device is still waiting for the system to
    /// confirm it (so it's worth drawing again soon).
    pub fn waiting(&self) -> bool {
        let fresh = |since: &Instant| since.elapsed() < CONFIRM_WITHIN;
        self.wifi_override.as_ref().is_some_and(|(_, t)| fresh(t))
            || self
                .bluetooth_override
                .as_ref()
                .is_some_and(|(_, t)| fresh(t))
            || self
                .profile_override
                .as_ref()
                .is_some_and(|(_, t)| fresh(t))
            || self
                .device_override
                .as_ref()
                .is_some_and(|(_, _, t)| t.elapsed() < CONNECT_WITHIN)
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
                    actions.run(if status.daemon_profiles.is_some() {
                        Action::SetPowerProfile(*profile)
                    } else {
                        Action::SetCustomProfile(*profile, status.profile_command.clone())
                    });
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
            Some(Hit::Bluetooth) => {
                let shown = self
                    .bluetooth_override
                    .map(|(on, _)| on)
                    .or(status.bluetooth.as_ref().map(|b| b.powered));
                if let Some(powered) = shown {
                    self.bluetooth_override = Some((!powered, Instant::now()));
                    actions.run(Action::SetBluetooth(!powered));
                }
            }
            Some(Hit::BluetoothDevices) => {
                self.devices_open = !self.devices_open;
                self.relayout(xdg_shell, theme);
            }
            Some(Hit::BluetoothDevice(i)) => {
                let device = status.bluetooth.as_ref().and_then(|b| b.devices.get(i));
                let busy = self
                    .device_override
                    .as_ref()
                    .is_some_and(|(_, _, since)| since.elapsed() < CONNECT_WITHIN);
                if let Some(device) = device
                    && !busy
                {
                    let connect = !device.connected;
                    self.device_override = Some((device.address.clone(), connect, Instant::now()));
                    actions.run(Action::ConnectBluetooth(device.address.clone(), connect));
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
            Some(hit @ (Hit::Lock | Hit::LogOut | Hit::Reboot | Hit::PowerOff)) => {
                actions.run(match hit {
                    Hit::Lock => Action::Lock,
                    Hit::LogOut => Action::LogOut,
                    Hit::Reboot => Action::Reboot,
                    _ => Action::PowerOff,
                });
                return true;
            }
            Some(Hit::DarkMode) => crate::config::set_dark(!theme.dark),
            // Not wired up yet (notifications).
            Some(Hit::DoNotDisturb) | None => {}
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
        // While the power row closes, the popup keeps its open size.
        let layout = self.collapsing.as_ref().map_or(&self.layout, |(l, _)| l);
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
        let powered = status.bluetooth.as_ref().map(|b| b.powered);
        self.bluetooth_override = self
            .bluetooth_override
            .filter(|&(on, since)| fresh(since) && powered != Some(on));
        if let (Some((on, _)), Some(bluetooth)) = (self.bluetooth_override, &mut shown.bluetooth) {
            bluetooth.powered = on;
        }
        self.device_override = self
            .device_override
            .take()
            .filter(|(address, connect, since)| {
                since.elapsed() < CONNECT_WITHIN
                    && status.bluetooth.as_ref().is_some_and(|b| {
                        b.devices
                            .iter()
                            .any(|d| d.address == *address && d.connected != *connect)
                    })
            });
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
            layout,
            self.layout.power_menu.is_some(),
            fonts,
            icons,
            theme,
            status,
            volume,
            self.brightness,
            scale as f32,
            &mut self.anim,
            self.dragging,
            self.device_override
                .as_ref()
                .map(|(address, connect, _)| (address.as_str(), *connect)),
        );
        // Still moving: draw again when this frame is shown.
        if self.anim.running() {
            self.dirty = true;
        } else if let Some((_, positioner)) = self.collapsing.take() {
            // The power row is gone: shrink to fit, drawn next frame.
            self.popup.reposition(&positioner, 0);
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
/// How long a Bluetooth device may take to connect (or disconnect).
pub const CONNECT_WITHIN: Duration = Duration::from_secs(20);

/// Draw the card. Separate from the Wayland plumbing so tests can render it.
#[allow(clippy::too_many_arguments)]
fn paint(
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    power_open: bool,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    status: &Status,
    volume: (u32, bool),
    brightness: Option<u32>,
    s: f32,
    anim: &mut Animator<Moving>,
    dragging: Option<Slider>,
    // A Bluetooth device being connected (true) or disconnected.
    pending: Option<(&str, bool)>,
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
    // The power row fades in as the footer slides down to make room for it
    // (`shift` is how much of its room isn't open yet).
    let reveal = anim.get(
        Moving::PowerMenu,
        if power_open { 1.0 } else { 0.0 },
        MEDIUM,
    );
    let shift = if layout.power_menu.is_some() {
        (1.0 - reveal) * (POWER_TILE_HEIGHT + theme.qs_gap)
    } else {
        0.0
    };
    canvas.clear(Color::rgba(0, 0, 0, 0.0));
    canvas.rounded_rect(
        0.0,
        0.0,
        layout.width,
        layout.height - shift,
        theme.qs_radius,
        theme.qs_background,
    );

    // Toggles: labelled tiles, an icon chip and two lines of text.
    let wifi_on = status.wifi_enabled;
    for (i, (rect, hit)) in layout.toggles.iter().enumerate() {
        // (on?, available?, icon, name, state)
        let (on, available, icon, name, state) = match hit {
            Hit::Wifi => {
                let (name, connected) = match &status.network {
                    crate::status::Network::Wifi { ssid, .. } if !ssid.is_empty() => {
                        (ssid.clone(), true)
                    }
                    crate::status::Network::Wifi { .. } => ("Wi-Fi".into(), true),
                    _ => ("Wi-Fi".into(), false),
                };
                let icon = if wifi_on == Some(false) {
                    "network-wireless-disabled-symbolic"
                } else {
                    "network-wireless-symbolic"
                };
                let state = if wifi_on == Some(false) {
                    "Off"
                } else if connected {
                    "Connected"
                } else {
                    "Not connected"
                };
                (wifi_on == Some(true), wifi_on.is_some(), icon, name, state)
            }
            Hit::Bluetooth => {
                let bluetooth = status.bluetooth.as_ref();
                let on = bluetooth.is_some_and(|b| b.powered);
                // The connected device's name, the way Wi-Fi shows the network.
                let device = bluetooth.filter(|b| b.powered).and_then(|b| b.connected());
                let name = device.map_or_else(|| "Bluetooth".into(), |d| d.name.clone());
                let icon = if on {
                    "bluetooth-active-symbolic"
                } else {
                    "bluetooth-disabled-symbolic"
                };
                let state = if device.is_some() {
                    "Connected"
                } else if on {
                    "On"
                } else {
                    "Off"
                };
                (on, bluetooth.is_some(), icon, name, state)
            }
            Hit::DarkMode => (
                theme.dark,
                true,
                "weather-clear-night-symbolic",
                "Dark Mode".into(),
                if theme.dark { "On" } else { "Off" },
            ),
            _ => (
                false,
                false,
                "notifications-disabled-symbolic",
                "Do Not Disturb".into(),
                "Off",
            ),
        };
        let fade = if available { 1.0 } else { 0.4 };
        let lit = anim.get(Moving::Toggle(i), if on { 1.0 } else { 0.0 }, SHORT);
        canvas.rounded_rect(
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            theme.qs_tile_radius,
            theme.qs_toggle_off,
        );
        // The chip: accent when on, a subtle fill when off.
        let chip_off = theme.popup_text.with_alpha(0.08);
        let chip_color = chip_off.mix(theme.qs_toggle_on, lit);
        let chip_x = rect.x + (rect.h - CHIP) / 2.0;
        let chip_y = rect.y + (rect.h - CHIP) / 2.0;
        canvas.rounded_rect(
            chip_x,
            chip_y,
            CHIP,
            CHIP,
            (theme.qs_tile_radius - 4.0).clamp(4.0, CHIP / 2.0),
            chip_color.with_alpha(chip_color.a * fade),
        );
        let icon_size = 20.0;
        let fg = theme.popup_text.mix(theme.on_accent, lit);
        canvas.icon(
            icons,
            icon,
            chip_x + (CHIP - icon_size) / 2.0,
            chip_y + (CHIP - icon_size) / 2.0,
            icon_size,
            fg.with_alpha(fg.a * fade),
        );
        // A chevron to the section's settings page.
        let linked = settings_page(*hit)
            .is_some_and(|page| layout.settings_links.iter().any(|(_, p)| *p == page));
        let text_x = chip_x + CHIP + 10.0;
        let right = rect.x + rect.w - if linked { CHEVRON_ZONE } else { 10.0 };
        let dim = theme.qs_dim.with_alpha(theme.qs_dim.a * fade);
        if linked {
            let size = 14.0;
            canvas.icon(
                icons,
                "go-next-symbolic",
                rect.x + rect.w - CHEVRON_ZONE + (CHEVRON_ZONE - size) / 2.0 - 2.0,
                rect.y + (rect.h - size) / 2.0,
                size,
                dim,
            );
        }
        let room = right - text_x;
        let mut name_text = fit(fonts, &name, 13.0, Weight::Bold, room, s);
        let mut state_text = fit(fonts, state, 11.5, Weight::Normal, room, s);
        let block = name_text.height + state_text.height + 1.0;
        let ty = rect.y + (rect.h - block) / 2.0;
        let name_color = theme.popup_text.with_alpha(theme.popup_text.a * fade);
        let state_y = (ty + name_text.height + 1.0).round();
        canvas.text(
            fonts,
            &mut name_text,
            text_x.round(),
            ty.round(),
            name_color,
        );
        canvas.text(fonts, &mut state_text, text_x.round(), state_y, dim);
    }

    // Paired Bluetooth devices.
    if let (Some(list), Some(bluetooth)) = (&layout.devices, &status.bluetooth) {
        paint_devices(canvas, list, bluetooth, pending, fonts, icons, theme, s);
    }

    // Volume: the fat slider, speaker icon inside.
    let v = layout.volume;
    let (percent, muted) = volume;
    let fill = if muted {
        theme.qs_slider_fill.with_alpha(0.35)
    } else {
        theme.qs_slider_fill
    };
    canvas.slider_rounded(
        v.x,
        v.y,
        v.w,
        v.h,
        theme.qs_tile_radius,
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
        canvas.slider_rounded(
            b.x,
            b.y,
            b.w,
            b.h,
            theme.qs_tile_radius,
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
        canvas.rounded_rect(
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            theme.qs_tile_radius,
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
            theme.qs_toggle_off,
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
                (theme.qs_tile_radius - PROFILE_INSET).max(0.0),
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
                theme.popup_text
            };
            // Icon and label side by side, centred in the segment.
            let icon_size = 16.0;
            let mut text = fit(
                fonts,
                label,
                theme.qs_label_size,
                Weight::Normal,
                rect.w - icon_size - 6.0 - 8.0,
                s,
            );
            let total = icon_size + 6.0 + text.width;
            let x = rect.x + (rect.w - total) / 2.0;
            canvas.icon(
                icons,
                icon,
                x.round(),
                rect.y + (rect.h - icon_size) / 2.0,
                icon_size,
                fg,
            );
            let ty = (rect.y + (rect.h - text.height) / 2.0).round();
            canvas.text(fonts, &mut text, (x + icon_size + 6.0).round(), ty, fg);
        }
    }

    // Power row.
    let faded = |c: Color| c.with_alpha(c.a * reveal);
    for (rect, hit) in layout.power_menu.iter().flatten() {
        // Everything in the row (tile, icon, label) only shows inside the
        // room it has opened so far, between the rows above and the footer,
        // so it doesn't draw over its neighbours while it grows or shrinks.
        // The clip is the row's fixed slot, not the sliding tile.
        let room = POWER_TILE_HEIGHT + theme.qs_gap;
        let visible = (room - shift - theme.qs_gap).max(0.0);
        if visible <= 0.0 {
            continue;
        }
        let clip = (rect.x, rect.y, rect.w, visible);
        let rect = Rect {
            y: rect.y - shift / 2.0,
            ..*rect
        };
        canvas.rounded_rect_clipped(
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            theme.qs_tile_radius,
            faded(theme.qs_tile),
            clip,
        );
        let (icon, label) = match hit {
            Hit::Lock => ("system-lock-screen-symbolic", "Lock"),
            Hit::LogOut => ("system-log-out-symbolic", "Log Out"),
            Hit::Reboot => ("system-reboot-symbolic", "Restart"),
            _ => ("system-shutdown-symbolic", "Power Off"),
        };
        canvas.icon_clipped(
            icons,
            icon,
            rect.x + (rect.w - 20.0) / 2.0,
            rect.y + 12.0,
            20.0,
            faded(theme.popup_text),
            clip,
        );
        let mut text = fonts.layout(label, theme.qs_label_size, Weight::Normal, s);
        let tx = (rect.x + (rect.w - text.width) / 2.0).round();
        canvas.text_clipped(
            fonts,
            &mut text,
            tx,
            rect.y + 38.0,
            faded(theme.qs_dim),
            clip,
        );
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
        let ty = (layout.footer_y - shift + (FOOTER_HEIGHT - text.height) / 2.0).round();
        canvas.text(
            fonts,
            &mut text,
            layout.toggle_tile.x + 4.0,
            ty,
            theme.qs_dim,
        );
    }
    let button_radius = (theme.qs_tile_radius - 4.0).max(0.0);
    let g = Rect {
        y: layout.settings_button.y - shift,
        ..layout.settings_button
    };
    if g.w > 0.0 {
        canvas.rounded_rect(g.x, g.y, g.w, g.h, button_radius, theme.qs_toggle_off);
        canvas.icon(
            icons,
            "emblem-system-symbolic",
            g.x + (g.w - 18.0) / 2.0,
            g.y + (g.h - 18.0) / 2.0,
            18.0,
            theme.popup_text,
        );
    }
    let p = Rect {
        y: layout.power_button.y - shift,
        ..layout.power_button
    };
    let power_bg = theme.qs_toggle_off.mix(theme.qs_toggle_on, reveal);
    canvas.rounded_rect(p.x, p.y, p.w, p.h, button_radius, power_bg);
    canvas.icon(
        icons,
        "system-shutdown-symbolic",
        p.x + (p.w - 18.0) / 2.0,
        p.y + (p.h - 18.0) / 2.0,
        18.0,
        theme.popup_text.mix(theme.on_accent, reveal),
    );
}

/// The Bluetooth devices tile: "Devices" and an arrow, then (open) a row
/// per paired device saying whether it's connected.
#[allow(clippy::too_many_arguments)]
fn paint_devices(
    canvas: &mut Canvas<'_>,
    list: &DeviceList,
    bluetooth: &crate::status::Bluetooth,
    pending: Option<(&str, bool)>,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    s: f32,
) {
    let open = !list.rows.is_empty();
    let header = list.header;
    let arrow = if open {
        "pan-up-symbolic"
    } else {
        "pan-down-symbolic"
    };
    canvas.icon(
        icons,
        arrow,
        header.x + header.w - 4.0 - 16.0,
        header.y + (header.h - 16.0) / 2.0,
        16.0,
        theme.qs_dim,
    );
    let icon_size = 18.0;
    // One line: an icon, a name, and something on the right.
    let mut line = |canvas: &mut Canvas<'_>,
                    rect: Rect,
                    icon: &str,
                    name: &str,
                    right: Option<(&str, Color)>| {
        let iy = rect.y + (rect.h - icon_size) / 2.0;
        if !canvas.icon(icons, icon, rect.x + 4.0, iy, icon_size, theme.popup_text) {
            canvas.icon(
                icons,
                "bluetooth-active-symbolic",
                rect.x + 4.0,
                iy,
                icon_size,
                theme.popup_text,
            );
        }
        let mut right_w = 4.0;
        if let Some((text, color)) = right {
            let mut text = fonts.layout(text, theme.qs_label_size, Weight::Normal, s);
            right_w += text.width + 8.0;
            let ty = (rect.y + (rect.h - text.height) / 2.0).round();
            let tx = (rect.x + rect.w - 4.0 - text.width).round();
            canvas.text(fonts, &mut text, tx, ty, color);
        }
        let x = rect.x + 4.0 + icon_size + 10.0;
        let room = rect.x + rect.w - right_w - x;
        let mut text = fonts.layout(name, theme.font_size, Weight::Normal, s);
        let mut keep = name.chars().count();
        while text.width > room && keep > 2 {
            keep -= 1;
            text = fonts.layout(&ellipsize(name, keep), theme.font_size, Weight::Normal, s);
        }
        let ty = (rect.y + (rect.h - text.height) / 2.0).round();
        canvas.text(fonts, &mut text, x.round(), ty, theme.popup_text);
    };

    line(canvas, header, "bluetooth-active-symbolic", "Devices", None);
    for (rect, device) in list.rows.iter().zip(&bluetooth.devices) {
        canvas.rounded_rect(rect.x, rect.y, rect.w, 1.0, 0.0, theme.card_separator);
        let connected = match device.battery {
            Some(percent) => format!("Connected · {percent}%"),
            None => "Connected".into(),
        };
        let state = match pending {
            Some((address, true)) if address == device.address => {
                Some(("Connecting…", theme.qs_dim))
            }
            Some((address, false)) if address == device.address => {
                Some(("Disconnecting…", theme.qs_dim))
            }
            _ if device.connected => Some((connected.as_str(), theme.qs_toggle_on)),
            _ => None,
        };
        let icon = format!("{}-symbolic", device.icon);
        line(canvas, *rect, &icon, &device.name, state);
    }
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

/// Text that fits in `room`: shortened only as needed.
fn fit(
    fonts: &mut Fonts,
    text: &str,
    size: f32,
    weight: Weight,
    room: f32,
    s: f32,
) -> cnde_ui::TextLayout {
    let mut keep = text.chars().count();
    let mut layout = fonts.layout(text, size, weight, s);
    while layout.width > room && keep > 2 {
        keep -= 1;
        layout = fonts.layout(&ellipsize(text, keep), size, weight, s);
    }
    layout
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
        let theme = Theme::quiet_dark();
        let l = layout(&theme, false, false, Features::default());
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
        let closed = layout(&theme, false, false, Features::default());
        let open = layout(&theme, true, false, Features::default());
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
            ..Features::default()
        };
        let l = layout(&Theme::dark(), false, false, features);
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
        assert!(l.height > layout(&Theme::dark(), false, false, Features::default()).height);
    }

    #[test]
    fn sections_open_their_settings_pages() {
        let features = Features {
            brightness: true,
            ..Features::default()
        };
        let l = layout(&Theme::dark(), false, false, features);
        // The chevron at a tile's right end opens its page; the body toggles.
        let chevron = |r: Rect| ((r.x + r.w - 10.0) as f64, (r.y + r.h / 2.0) as f64);
        let (wifi, _) = l.toggles[0];
        assert_eq!(l.hit(chevron(wifi)), Some(Hit::SettingsPage("wifi")));
        assert_eq!(
            l.hit(((wifi.x + 20.0) as f64, (wifi.y + wifi.h / 2.0) as f64)),
            Some(Hit::Wifi)
        );
        let b = l.brightness.unwrap();
        let end = l.toggle_tile.x + l.toggle_tile.w;
        assert!(b.x + b.w < end - b.h);
        assert_eq!(
            l.hit(((end - b.h / 2.0) as f64, (b.y + b.h / 2.0) as f64)),
            Some(Hit::SettingsPage("displays"))
        );
        let (bt, _) = l.toggles[1];
        assert_eq!(l.hit(chevron(bt)), Some(Hit::SettingsPage("bluetooth")));
        // Do Not Disturb has no page: its whole tile toggles.
        let (dnd, _) = l.toggles[3];
        assert_eq!(l.hit(chevron(dnd)), Some(Hit::DoNotDisturb));
    }

    #[test]
    fn toggles_are_two_columns() {
        let l = layout(&Theme::dark(), false, false, Features::default());
        let rects: Vec<Rect> = l.toggles.iter().map(|t| t.0).collect();
        assert_eq!(rects[0].y, rects[1].y);
        assert_eq!(rects[2].y, rects[3].y);
        assert!(rects[2].y > rects[0].y + rects[0].h);
        assert!(rects[1].x > rects[0].x + rects[0].w);
        assert_eq!(rects[0].h, TOGGLE_TILE_HEIGHT);
    }

    #[test]
    fn bluetooth_devices_open_and_close() {
        let theme = Theme::dark();
        let centre = |r: Rect| ((r.x + r.w / 2.0) as f64, (r.y + r.h / 2.0) as f64);
        // No paired devices (or Bluetooth's off): no list.
        let none = layout(&theme, false, true, Features::default());
        assert!(none.devices.is_none());
        let features = Features {
            bluetooth_devices: 3,
            ..Features::default()
        };
        let closed = layout(&theme, false, false, features);
        let list = closed.devices.as_ref().unwrap();
        assert!(list.rows.is_empty());
        assert_eq!(closed.hit(centre(list.header)), Some(Hit::BluetoothDevices));
        assert!(closed.volume.y > list.tile.y + list.tile.h);

        let open = layout(&theme, false, true, features);
        let list = open.devices.as_ref().unwrap();
        assert_eq!(list.rows.len(), 3);
        assert_eq!(
            open.hit(centre(list.rows[2])),
            Some(Hit::BluetoothDevice(2))
        );
        assert!(open.height > closed.height);
        // The rest of the card moves down out of the way.
        assert!(open.volume.y > list.rows[2].y + list.rows[2].h);
        assert_eq!(open.hit(centre(open.volume)), Some(Hit::Volume));
    }

    #[test]
    fn bluetooth_features_follow_status() {
        let mut status = Status::default();
        assert_eq!(Features::of(&status).bluetooth_devices, 0);
        let device = |name: &str| crate::status::BluetoothDevice {
            address: name.into(),
            name: name.into(),
            icon: String::new(),
            connected: false,
            battery: None,
        };
        status.bluetooth = Some(crate::status::Bluetooth {
            powered: false,
            devices: vec![device("a"), device("b")],
        });
        assert_eq!(Features::of(&status).bluetooth_devices, 0);
        status.bluetooth.as_mut().unwrap().powered = true;
        assert_eq!(Features::of(&status).bluetooth_devices, 2);
        status.bluetooth.as_mut().unwrap().devices = (0..20).map(|_| device("x")).collect();
        assert_eq!(Features::of(&status).bluetooth_devices, MAX_DEVICES);
    }

    #[test]
    fn slider_maps_x_to_percent() {
        let l = layout(&Theme::dark(), false, false, Features::default());
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
        // CNDE_QS_THEME=quiet / quiet-light / light picks the palette.
        let theme = match std::env::var("CNDE_QS_THEME").as_deref() {
            Ok("quiet") => Theme::quiet_dark(),
            Ok("quiet-light") => Theme::quiet_light(),
            Ok("light") => Theme::light(),
            _ => Theme::dark(),
        };
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
            bluetooth: Some(crate::status::Bluetooth {
                powered: true,
                devices: vec![
                    crate::status::BluetoothDevice {
                        address: "00:00:00:00:00:01".into(),
                        name: "Earbuds".into(),
                        icon: "audio-headphones".into(),
                        connected: true,
                        battery: Some(82),
                    },
                    crate::status::BluetoothDevice {
                        address: "00:00:00:00:00:02".into(),
                        name: "Trackball".into(),
                        icon: "input-mouse".into(),
                        connected: false,
                        battery: None,
                    },
                ],
            }),
            ..Status::default()
        };
        let mut fonts = Fonts::new(theme.font_families);
        let mut icons = Icons::new();
        let power_open = std::env::var_os("CNDE_QS_POWER").is_some();
        let devices_open = std::env::var_os("CNDE_QS_DEVICES").is_some();
        let layout = layout(&theme, power_open, devices_open, Features::of(&status));
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
                power_open,
                &mut fonts,
                &mut icons,
                &theme,
                &status,
                (62, false),
                Some(40),
                scale,
                &mut Animator::new(),
                None,
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
