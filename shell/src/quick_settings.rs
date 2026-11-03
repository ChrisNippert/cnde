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
    audio::Kind,
    panel::volume_icon,
};

mod audio_list;
mod devices;

use audio_list::{ListLayout, ListState};
use devices::{DeviceList, DeviceRows};

/// Where a kind's list is in the per-kind arrays.
fn slot(kind: Kind) -> usize {
    match kind {
        Kind::Output => 0,
        Kind::Input => 1,
    }
}

/// What a point in the card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Volume,
    Mute,
    /// The input slider (the microphone's gain) and its mute end.
    Input,
    MicMute,
    /// The arrow beside a slider: drops that device list down (or folds it up).
    AudioList(Kind),
    /// A device or port in a dropped-down list.
    AudioRow(Kind, audio_list::Row),
    Brightness,
    /// Index into the available power profiles.
    PowerProfile(usize),
    Wifi,
    Bluetooth,
    /// The Bluetooth devices tile's header: opens or closes the list.
    BluetoothDevices,
    /// A paired device in the list (index into the status's devices):
    /// connects or disconnects it.
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
    /// Paired Bluetooth devices, while there are some to list (or some
    /// fading out of the list).
    pub devices: Option<DeviceList>,
    pub volume: Rect,
    /// The arrow to the output list.
    pub volume_arrow: Rect,
    /// The microphone slider and its arrow, when there's a microphone.
    pub input: Option<(Rect, Rect)>,
    /// The output and input lists under their sliders, while open (or
    /// opening or closing).
    pub lists: [Option<ListLayout>; 2],
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

/// Which optional parts the card has, from what the machine offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub brightness: bool,
    /// How many power modes to offer (0: no power-profiles-daemon).
    pub power_profiles: usize,
    /// A microphone to show a slider for.
    pub input: bool,
}

impl Features {
    pub fn of(status: &Status) -> Self {
        Self {
            brightness: status.backlight.is_some(),
            input: !status.audio.inputs.is_empty(),
            power_profiles: status
                .power_profiles
                .as_ref()
                .map_or(0, |p| p.available.len()),
        }
    }
}

pub fn layout(
    theme: &Theme,
    power_menu_open: bool,
    devices: &DeviceRows,
    features: Features,
) -> Layout {
    layout_with(
        theme,
        power_menu_open,
        devices,
        features,
        &[ListState::default(), ListState::default()],
    )
}

/// [`layout`] with the audio lists as far open as `lists` says: the rows
/// below them sit lower by as much, so the card's height follows.
pub fn layout_with(
    theme: &Theme,
    power_menu_open: bool,
    bluetooth: &DeviceRows,
    features: Features,
    lists: &[ListState; 2],
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

    // Paired Bluetooth devices: a header row, the devices under it as far
    // as the list has opened.
    let devices = (!bluetooth.rows.is_empty()).then(|| {
        let list = bluetooth.list(pad, y, inner);
        y += list.tile.h + theme.qs_gap;
        list
    });

    // Each audio slider has a round arrow at its end, to the device list,
    // which drops down under the slider.
    let mut dropped = [None, None];
    let mut drop_list = |kind: Kind, y: &mut f32| {
        let state = &lists[slot(kind)];
        if state.progress <= 0.0 {
            return;
        }
        let list = audio_list::place(state, pad, *y, inner);
        *y += list.list.h + theme.qs_gap * list.progress;
        dropped[slot(kind)] = Some(list);
    };
    let audio_row = |y: f32| {
        let slider = Rect {
            x: pad,
            y,
            w: inner - SLIDER_HEIGHT - theme.qs_gap,
            h: SLIDER_HEIGHT,
        };
        let arrow = Rect {
            x: pad + inner - SLIDER_HEIGHT,
            w: SLIDER_HEIGHT,
            ..slider
        };
        (slider, arrow)
    };
    let (volume, volume_arrow) = audio_row(y);
    y += SLIDER_HEIGHT + theme.qs_gap;
    drop_list(Kind::Output, &mut y);
    let input = features.input.then(|| {
        let row = audio_row(y);
        y += SLIDER_HEIGHT + theme.qs_gap;
        drop_list(Kind::Input, &mut y);
        row
    });

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
        volume_arrow,
        input,
        lists: dropped,
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
        for kind in [Kind::Output, Kind::Input] {
            if let Some(list) = &self.lists[slot(kind)]
                && let Some(row) = list.hit(point)
            {
                return Some(Hit::AudioRow(kind, row));
            }
        }
        if self.volume_arrow.contains(point) {
            return Some(Hit::AudioList(Kind::Output));
        }
        if let Some((input, arrow)) = self.input {
            if arrow.contains(point) {
                return Some(Hit::AudioList(Kind::Input));
            }
            if input.contains(point) {
                let icon_zone = Rect {
                    w: input.h,
                    ..input
                };
                return Some(if icon_zone.contains(point) {
                    Hit::MicMute
                } else {
                    Hit::Input
                });
            }
        }
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
            let inside = |row: &devices::PlacedRow| {
                list.tile.contains(point) && row.rect.contains(point)
            };
            if let Some(i) = list.rows.iter().filter(|r| inside(r)).find_map(|r| r.index) {
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

    pub fn input_at(&self, x: f64) -> Option<u32> {
        self.input.map(|(input, _)| percent_along(input, x))
    }

    pub fn brightness_at(&self, x: f64) -> Option<u32> {
        self.brightness.map(|b| percent_along(b, x))
    }
}

/// Which list is down after `clicked`'s arrow is pressed: that one, or none
/// if it already was. Never both.
fn next_open(open: Option<Kind>, clicked: Kind) -> Option<Kind> {
    (open != Some(clicked)).then_some(clicked)
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
    /// An audio list dropping down (1) or folding up (0).
    List(Kind),
    /// The Bluetooth devices list opening (1) or folding up (0).
    Devices,
    /// How many rows the Bluetooth devices list is tall for.
    DevicesCount,
    /// A Bluetooth device's row (by row id): its slot, how far in it is,
    /// and how far connected.
    DeviceSlot(u32),
    DevicePresence(u32),
    DeviceLink(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slider {
    Volume,
    Input,
    Brightness,
}

impl Layout {
    fn slider_at(&self, slider: Slider, x: f64) -> Option<u32> {
        match slider {
            Slider::Volume => Some(self.volume_at(x)),
            Slider::Input => self.input_at(x),
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
    input_override: Option<(u32, Instant)>,
    /// The audio list that's dropped down (or dropping): only one is.
    open_list: Option<Kind>,
    /// Both lists' lines and how far open they are, as last drawn.
    lists: [ListState; 2],
    /// The height the popup is sized to. Lists open and close inside it
    /// (the card is drawn shorter and the window geometry says so), so
    /// the surface changes size only when they start and when they've
    /// settled, not every frame.
    surface_height: f32,
    /// Toggles show their new state as soon as they're pressed; if the
    /// system hasn't confirmed it within `CONFIRM_WITHIN`, they snap back
    /// (the change was refused or failed).
    wifi_override: Option<(bool, Instant)>,
    bluetooth_override: Option<(bool, Instant)>,
    /// A device being connected (true) or disconnected, by address; it
    /// says so until BlueZ reports it done or `CONNECT_WITHIN` runs out.
    device_override: Option<(String, bool, Instant)>,
    /// The Bluetooth devices list: its rows, and how open it is.
    devices: DeviceRows,
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
    /// until it's gone, then the popup shrinks to fit.
    collapsing: Option<Layout>,
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
        let mut devices = DeviceRows::default();
        devices.sync(devices::shown(status.bluetooth.as_ref()));
        let layout = layout(theme, false, &devices, features);
        let positioner = positioner(xdg_shell, &layout, anchor)?;
        let surface_height = layout.height;
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
            input_override: None,
            open_list: None,
            lists: [ListState::default(), ListState::default()],
            surface_height,
            wifi_override: None,
            bluetooth_override: None,
            device_override: None,
            devices,
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
    fn toggle_power_menu(&mut self, xdg_shell: &XdgShell, theme: &Theme) {
        let open = self.layout.power_menu.is_none();
        let new = layout_with(theme, open, &self.devices, self.features, &self.lists);
        let old = std::mem::replace(&mut self.layout, new);
        if open {
            self.collapsing = None;
            self.grow_surface(xdg_shell, theme);
        } else {
            self.collapsing = Some(old);
        }
    }

    /// The popup's size with the power row as it is and every list that's
    /// open or moving all the way open: the most the card will need.
    fn room_needed(&self, theme: &Theme) -> f32 {
        let mut lists = self.lists.clone();
        for kind in [Kind::Output, Kind::Input] {
            let list = &mut lists[slot(kind)];
            let moving = self.open_list == Some(kind) || list.progress > 0.0;
            list.progress = if moving { 1.0 } else { 0.0 };
        }
        let mut devices = self.devices.clone();
        let moving = devices.open || devices.progress > 0.0;
        devices.progress = if moving { 1.0 } else { 0.0 };
        devices.count = devices.shown_count() as f32;
        let open = self.layout.power_menu.is_some() || self.collapsing.is_some();
        layout_with(theme, open, &devices, self.features, &lists).height
    }

    /// Make the popup big enough for what's about to open.
    fn grow_surface(&mut self, xdg_shell: &XdgShell, theme: &Theme) {
        let needed = self.room_needed(theme).max(self.layout.height);
        if needed > self.surface_height + 0.5 {
            self.surface_height = needed;
            self.resize_surface(xdg_shell);
        }
        self.dirty = true;
    }

    fn resize_surface(&mut self, xdg_shell: &XdgShell) {
        if let Some(positioner) = sized_positioner(
            xdg_shell,
            (self.layout.width, self.surface_height),
            self.anchor,
        ) {
            self.popup.reposition(&positioner, 0);
        }
    }

    /// Lay the card out again (a list opened or closed, or what the
    /// machine offers changed), resizing the popup at once.
    fn relayout(&mut self, xdg_shell: &XdgShell, theme: &Theme) {
        let new = layout_with(
            theme,
            self.layout.power_menu.is_some(),
            &self.devices,
            self.features,
            &self.lists,
        );
        if new == self.layout {
            return;
        }
        self.layout = new;
        self.collapsing = None;
        self.grow_surface(xdg_shell, theme);
        // Smaller, with nothing moving: shrink now.
        if !self.anim.running() && self.surface_height > self.layout.height + 0.5 {
            self.surface_height = self.layout.height;
            self.resize_surface(xdg_shell);
        }
    }

    /// Drop `kind`'s list down, or fold it up if it's down. Opening one
    /// folds the other, so each moves at the same time.
    fn toggle_list(&mut self, kind: Kind, xdg_shell: &XdgShell, theme: &Theme, status: &Status) {
        self.open_list = next_open(self.open_list, kind);
        if self.open_list.is_some() {
            self.lists[slot(kind)].rows = audio_list::rows(status.audio.devices(kind));
            self.grow_surface(xdg_shell, theme);
        }
        self.dirty = true;
    }

    /// The status changed: grow or shrink for parts that came or went, and
    /// fade Bluetooth devices in and out as they're switched on and off or
    /// come and go.
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
        let before = self.devices.rows.clone();
        for id in self.devices.sync(devices::shown(status.bluetooth.as_ref())) {
            self.anim.restart(Moving::DevicePresence(id), 0.0, 1.0, MEDIUM);
        }
        if self.devices.rows != before {
            self.relayout(xdg_shell, theme);
            self.dirty = true;
        }
        // Devices came or went (a headset plugged in) or gained ports.
        let mut changed = false;
        for kind in [Kind::Output, Kind::Input] {
            let list = &mut self.lists[slot(kind)];
            if self.open_list == Some(kind) || list.progress > 0.0 {
                let rows = audio_list::rows(status.audio.devices(kind));
                if rows != list.rows {
                    list.rows = rows;
                    changed = true;
                }
            }
        }
        if changed {
            self.grow_surface(xdg_shell, theme);
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
        _anchor: (i32, i32, i32, i32),
    ) -> bool {
        match self.layout.hit(point) {
            Some(Hit::AudioList(kind)) => self.toggle_list(kind, xdg_shell, theme, status),
            Some(Hit::AudioRow(kind, row)) => {
                if let Some(action) = audio_list::row_action(kind, status.audio.devices(kind), row)
                {
                    actions.run(action);
                }
            }
            Some(Hit::Input) => {
                self.dragging = Some(Slider::Input);
                self.slide(Slider::Input, point.0, actions);
            }
            Some(Hit::MicMute) => actions.run(Action::ToggleMicMute),
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
                self.devices.open = !self.devices.open;
                self.grow_surface(xdg_shell, theme);
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
            Some(Hit::Power) => self.toggle_power_menu(xdg_shell, theme),
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
            Some(Hit::DoNotDisturb) => {
                if let Some(on) = status.do_not_disturb {
                    crate::config::set_do_not_disturb(!on);
                }
            }
            None => {}
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
            Slider::Input => {
                if self.input_override.map(|(v, _)| v) != Some(percent) {
                    self.input_override = Some((percent, Instant::now()));
                    actions.run(Action::SetMicVolume(percent));
                }
            }
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
        xdg_shell: &XdgShell,
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
        // The lists' progress this frame: the card follows it, inside a
        // popup already big enough.
        self.anim.begin();
        for kind in [Kind::Output, Kind::Input] {
            let target = if self.open_list == Some(kind) {
                1.0
            } else {
                0.0
            };
            let progress = self.anim.get(Moving::List(kind), target, MEDIUM);
            let list = &mut self.lists[slot(kind)];
            list.progress = progress;
            if progress <= 0.0 && target == 0.0 {
                list.rows.clear();
            }
        }
        self.devices.animate(&mut self.anim);
        if self.collapsing.is_none() {
            self.layout = layout_with(
                theme,
                self.layout.power_menu.is_some(),
                &self.devices,
                self.features,
                &self.lists,
            );
        }
        // While the power row closes, the popup keeps its open size.
        let layout = self.collapsing.as_ref().unwrap_or(&self.layout);
        let scale = self.scale;
        let (width, height) = (layout.width, self.surface_height.max(layout.height));
        let (w, h) = (
            (width * scale as f32) as i32,
            (height * scale as f32) as i32,
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
        if let Some((_, since)) = self.input_override
            && since.elapsed() > Duration::from_secs(2)
            && self.dragging != Some(Slider::Input)
        {
            self.input_override = None;
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
        let input = match (
            self.input_override,
            status.audio.default_device(Kind::Input),
        ) {
            (Some((percent, _)), current) => Some((percent, current.is_some_and(|c| c.muted))),
            (None, Some(current)) => Some((current.percent, current.muted)),
            (None, None) => None,
        };
        let card_height = paint(
            &mut canvas,
            layout,
            self.layout.power_menu.is_some(),
            fonts,
            icons,
            theme,
            status,
            volume,
            input,
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
        } else if self.collapsing.take().is_some() || self.surface_height > self.layout.height + 0.5
        {
            // The power row or a list is gone: shrink to fit, drawn next
            // frame.
            self.surface_height = self.layout.height;
            self.resize_surface(xdg_shell);
            self.dirty = true;
        }

        // Only the card itself counts as the popup (for the compositor's
        // frost behind it), not the room kept below it for what's opening.
        self.popup.xdg_surface().set_window_geometry(
            0,
            0,
            width.ceil() as i32,
            card_height.ceil() as i32,
        );

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
    input: Option<(u32, bool)>,
    brightness: Option<u32>,
    s: f32,
    anim: &mut Animator<Moving>,
    dragging: Option<Slider>,
    // A Bluetooth device being connected (true) or disconnected.
    pending: Option<(&str, bool)>,
) -> f32 {
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
    let input_shown = input.map(|(percent, _)| slide(Slider::Input, percent as f32 / 100.0));
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
    let card_height = layout.height - shift;
    canvas.rounded_rect(
        0.0,
        0.0,
        layout.width,
        card_height,
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
                status.do_not_disturb == Some(true),
                status.do_not_disturb.is_some(),
                "notifications-disabled-symbolic",
                "Do Not Disturb".into(),
                if status.do_not_disturb == Some(true) {
                    "On"
                } else {
                    "Off"
                },
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
    if let Some(list) = &layout.devices {
        paint_devices(canvas, list, pending, fonts, icons, theme, s);
    }

    // Output and input: the fat slider, speaker or microphone inside, and
    // an arrow to the device list.
    let (percent, muted) = volume;
    let name = status.volume.map_or("audio-volume-muted-symbolic", |vol| {
        volume_icon(crate::status::Volume {
            percent,
            muted: vol.muted,
        })
    });
    paint_volume_row(
        canvas,
        icons,
        theme,
        layout.volume,
        volume_shown,
        muted,
        name,
    );
    let opened = |kind: Kind| {
        layout.lists[slot(kind)]
            .as_ref()
            .map_or(0.0, |l| l.progress)
    };
    paint_arrow(
        canvas,
        icons,
        theme,
        layout.volume_arrow,
        opened(Kind::Output),
    );
    if let (Some((rect, arrow)), Some((_, muted)), Some(shown)) = (layout.input, input, input_shown)
    {
        let name = if muted {
            "microphone-sensitivity-muted-symbolic"
        } else {
            "audio-input-microphone-symbolic"
        };
        paint_volume_row(canvas, icons, theme, rect, shown, muted, name);
        paint_arrow(canvas, icons, theme, arrow, opened(Kind::Input));
    }
    for kind in [Kind::Output, Kind::Input] {
        if let Some(list) = &layout.lists[slot(kind)] {
            audio_list::paint(canvas, list, kind, status, fonts, icons, theme, s);
        }
    }

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
        let icon_size = 20.0;
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
    card_height
}

/// A fat slider with `icon` at its start; the icon is dark where the bright
/// fill is under it. A muted one is dimmed.
fn paint_volume_row(
    canvas: &mut Canvas<'_>,
    icons: &mut Icons,
    theme: &Theme,
    v: Rect,
    shown: f32,
    muted: bool,
    icon: &str,
) {
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
        shown,
        theme.qs_slider_track,
        fill,
    );
    let icon_size = 20.0;
    let icon_x = v.x + (v.h - icon_size) / 2.0 + 4.0;
    let covered = shown * v.w > icon_x - v.x + icon_size;
    let icon_color = if covered && !muted {
        theme.qs_on_fill
    } else {
        theme.popup_text
    };
    canvas.icon(
        icons,
        icon,
        icon_x,
        v.y + (v.h - icon_size) / 2.0,
        icon_size,
        icon_color,
    );
}

/// The round arrow at a slider's end.
/// The round button at a slider's end: a chevron that swings from
/// pointing right to pointing down as its list (`open`, 0-1) drops.
fn paint_arrow(canvas: &mut Canvas<'_>, icons: &mut Icons, theme: &Theme, rect: Rect, open: f32) {
    canvas.rounded_rect(
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        theme.qs_tile_radius,
        theme.qs_slider_track,
    );
    canvas.icon_rotated(
        icons,
        "go-next-symbolic",
        rect.x + (rect.w - 16.0) / 2.0,
        rect.y + (rect.h - 16.0) / 2.0,
        16.0,
        theme.popup_text,
        open * std::f32::consts::FRAC_PI_2,
    );
}

/// The Bluetooth devices tile: "Devices" and an arrow, then the rows as far
/// as the list has opened: each device, and whether it's connected.
fn paint_devices(
    canvas: &mut Canvas<'_>,
    list: &DeviceList,
    pending: Option<(&str, bool)>,
    fonts: &mut Fonts,
    icons: &mut Icons,
    theme: &Theme,
    s: f32,
) {
    let header = list.header;
    let clip = (list.tile.x, list.tile.y, list.tile.w, list.tile.h);
    let arrow = if list.open > 0.5 {
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
    // One line: an icon, a name, and something on the right. `alpha` fades
    // the whole line; the right-hand text has its own too.
    let mut line = |canvas: &mut Canvas<'_>,
                    rect: Rect,
                    alpha: f32,
                    icon: &str,
                    name: &str,
                    right: Option<(&str, Color, f32)>| {
        let fade = |c: Color, a: f32| c.with_alpha(c.a * alpha * a);
        let iy = rect.y + (rect.h - icon_size) / 2.0;
        if !canvas.icon_clipped(
            icons,
            icon,
            rect.x + 4.0,
            iy,
            icon_size,
            fade(theme.popup_text, 1.0),
            clip,
        ) {
            canvas.icon_clipped(
                icons,
                "bluetooth-active-symbolic",
                rect.x + 4.0,
                iy,
                icon_size,
                fade(theme.popup_text, 1.0),
                clip,
            );
        }
        let mut right_w = 4.0;
        if let Some((text, color, a)) = right {
            let mut text = fonts.layout(text, theme.qs_label_size, Weight::Normal, s);
            right_w += text.width + 8.0;
            let ty = (rect.y + (rect.h - text.height) / 2.0).round();
            let tx = (rect.x + rect.w - 4.0 - text.width).round();
            canvas.text_clipped(fonts, &mut text, tx, ty, fade(color, a), clip);
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
        canvas.text_clipped(fonts, &mut text, x.round(), ty, fade(theme.popup_text, 1.0), clip);
    };

    line(canvas, header, 1.0, "bluetooth-active-symbolic", "Devices", None);
    for row in &list.rows {
        if row.alpha <= 0.0 {
            continue;
        }
        let rect = row.rect;
        canvas.rounded_rect_clipped(
            rect.x,
            rect.y,
            rect.w,
            1.0,
            0.0,
            theme.card_separator.with_alpha(theme.card_separator.a * row.alpha),
            clip,
        );
        let device = &row.device;
        // Connecting or disconnecting says so; otherwise the connected line
        // cross-fades in and out as the device connects and disconnects.
        let connected = match device.battery {
            Some(percent) => format!("Connected · {percent}%"),
            None => "Connected".into(),
        };
        let state = match pending {
            Some((address, true)) if address == device.address => {
                ("Connecting…", theme.qs_dim, 1.0)
            }
            Some((address, false)) if address == device.address => {
                ("Disconnecting…", theme.qs_dim, 1.0)
            }
            _ => (connected.as_str(), theme.qs_toggle_on, row.link),
        };
        let icon = format!("{}-symbolic", device.icon);
        line(
            canvas,
            rect,
            row.alpha,
            &icon,
            &device.name,
            Some((state.0, state.1, state.2)),
        );
    }
}

fn positioner(
    xdg_shell: &XdgShell,
    layout: &Layout,
    anchor: (i32, i32, i32, i32),
) -> Option<XdgPositioner> {
    sized_positioner(xdg_shell, (layout.width, layout.height), anchor)
}

fn sized_positioner(
    xdg_shell: &XdgShell,
    (width, height): (f32, f32),
    (ax, ay, aw, ah): (i32, i32, i32, i32),
) -> Option<XdgPositioner> {
    let positioner = XdgPositioner::new(xdg_shell)
        .map_err(|err| warn!(?err, "no xdg positioner"))
        .ok()?;
    positioner.set_size(width as i32, height as i32);
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
        let l = layout(&theme, false, &DeviceRows::default(), Features::default());
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

    fn lists(output: f32, input: f32) -> [ListState; 2] {
        let rows = |n| (0..n).map(audio_list::Row::Device).collect();
        [
            ListState {
                rows: rows(3),
                progress: output,
            },
            ListState {
                rows: rows(2),
                progress: input,
            },
        ]
    }

    #[test]
    fn audio_lists_push_the_rows_below_down() {
        let theme = Theme::dark();
        let features = Features {
            input: true,
            ..Features::default()
        };
        let at = |output, input| layout_with(&theme, false, &DeviceRows::default(), features, &lists(output, input));
        let closed = at(0.0, 0.0);
        assert_eq!(closed, layout(&theme, false, &DeviceRows::default(), features));
        assert!(closed.lists.iter().all(Option::is_none));
        let full = audio_list::full_height(&lists(1.0, 0.0)[0].rows);
        let (half, open) = (at(0.5, 0.0), at(1.0, 0.0));
        let grew = |l: &Layout| l.height - closed.height;
        assert!((grew(&half) - (full / 2.0 + theme.qs_gap / 2.0)).abs() < 0.01);
        assert!((grew(&open) - (full + theme.qs_gap)).abs() < 0.01);
        // The input slider rides down with it; the output one stays.
        assert_eq!(open.volume, closed.volume);
        assert_eq!(
            open.input.unwrap().0.y - closed.input.unwrap().0.y,
            grew(&open)
        );
        // Both open: the input list hangs under the moved input slider.
        let both = at(1.0, 1.0);
        let input_list = both.lists[1].as_ref().unwrap().list;
        assert_eq!(
            input_list.y,
            both.input.unwrap().0.y + SLIDER_HEIGHT + theme.qs_gap
        );
    }

    #[test]
    fn audio_list_rows_are_hit_inside_the_open_part() {
        let theme = Theme::dark();
        let features = Features {
            input: true,
            ..Features::default()
        };
        let open = layout_with(&theme, false, &DeviceRows::default(), features, &lists(1.0, 0.0));
        let list = open.lists[0].as_ref().unwrap();
        let (rect, row) = list.rows[1];
        let centre = (
            (rect.x + rect.w / 2.0) as f64,
            (rect.y + rect.h / 2.0) as f64,
        );
        assert_eq!(open.hit(centre), Some(Hit::AudioRow(Kind::Output, row)));
        // The arrows still toggle their lists.
        let arrow = open.volume_arrow;
        let at = ((arrow.x + 4.0) as f64, (arrow.y + 4.0) as f64);
        assert_eq!(open.hit(at), Some(Hit::AudioList(Kind::Output)));
        let (_, input_arrow) = open.input.unwrap();
        let at = ((input_arrow.x + 4.0) as f64, (input_arrow.y + 4.0) as f64);
        assert_eq!(open.hit(at), Some(Hit::AudioList(Kind::Input)));
        // Half open, the last row isn't there yet, so it can't be hit.
        let half = layout_with(&theme, false, &DeviceRows::default(), features, &lists(0.4, 0.0));
        let (last, _) = half.lists[0].as_ref().unwrap().rows[2];
        let at = ((last.x + 4.0) as f64, (last.y + last.h - 4.0) as f64);
        assert!(!matches!(half.hit(at), Some(Hit::AudioRow(..))));
    }

    #[test]
    fn only_one_list_is_down_at_a_time() {
        assert_eq!(next_open(None, Kind::Output), Some(Kind::Output));
        assert_eq!(next_open(Some(Kind::Output), Kind::Output), None);
        assert_eq!(
            next_open(Some(Kind::Output), Kind::Input),
            Some(Kind::Input)
        );
        assert_eq!(
            next_open(Some(Kind::Input), Kind::Output),
            Some(Kind::Output)
        );
    }

    #[test]
    fn power_menu_grows_the_card() {
        let theme = Theme::dark();
        let closed = layout(&theme, false, &DeviceRows::default(), Features::default());
        let open = layout(&theme, true, &DeviceRows::default(), Features::default());
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
        let l = layout(&Theme::dark(), false, &DeviceRows::default(), features);
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
        assert!(l.height > layout(&Theme::dark(), false, &DeviceRows::default(), Features::default()).height);
    }

    #[test]
    fn sections_open_their_settings_pages() {
        let features = Features {
            brightness: true,
            ..Features::default()
        };
        let l = layout(&Theme::dark(), false, &DeviceRows::default(), features);
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
        let l = layout(&Theme::dark(), false, &DeviceRows::default(), Features::default());
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
        let device = |address: &str| crate::status::BluetoothDevice {
            address: address.into(),
            name: address.into(),
            icon: String::new(),
            connected: false,
            battery: None,
        };
        let three = [device("a"), device("b"), device("c")];
        // No paired devices (or Bluetooth's off): no list.
        let none = layout(&theme, false, &DeviceRows::default(), Features::default());
        assert!(none.devices.is_none());
        let features = Features::default();
        let closed = layout(&theme, false, &DeviceRows::settled(&three, false), features);
        let list = closed.devices.as_ref().unwrap();
        assert_eq!(list.tile.h, list.header.h);
        assert_eq!(closed.hit(centre(list.header)), Some(Hit::BluetoothDevices));
        assert!(closed.volume.y > list.tile.y + list.tile.h);

        let open = layout(&theme, false, &DeviceRows::settled(&three, true), features);
        let list = open.devices.as_ref().unwrap();
        assert_eq!(list.rows.len(), 3);
        assert_eq!(
            open.hit(centre(list.rows[2].rect)),
            Some(Hit::BluetoothDevice(2))
        );
        assert!(open.height > closed.height);
        // The rest of the card moves down out of the way.
        assert!(open.volume.y > list.rows[2].rect.y + list.rows[2].rect.h);
        assert_eq!(open.hit(centre(open.volume)), Some(Hit::Volume));
    }

    #[test]
    fn slider_maps_x_to_percent() {
        let l = layout(&Theme::dark(), false, &DeviceRows::default(), Features::default());
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
        let devices = DeviceRows::settled(
            devices::shown(status.bluetooth.as_ref()),
            devices_open,
        );
        let layout = layout(&theme, power_open, &devices, Features::of(&status));
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
                Some((80, false)),
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
