//! The on-screen display: a pill near the bottom of the screen showing the
//! volume or brightness after its key is pressed, gone after a moment.

use std::time::{Duration, Instant};

use cnde_ipc::MediaKey;

use crate::media::{Op, Status};
use cnde_ui::{
    Canvas, Painter, Weight,
    tiny_skia::{self, Pixmap, PixmapPaint, Transform},
};
use smithay_client_toolkit::{
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        client::protocol::{wl_shm, wl_surface::WlSurface},
    },
    compositor::FrameCallbackData,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
    },
};
use tracing::warn;

use crate::{App, actions::Action, panel::volume_icon};

const SIZE: (u32, u32) = (260, 52);
/// Above where the dock usually sits.
const BOTTOM_MARGIN: i32 = 120;
/// How far the pill rises as it fades in (and sinks as it goes); the
/// surface is this much taller, below the pill.
const RISE: f32 = 12.0;
/// Fading in and out takes this long.
const ANIM: Duration = Duration::from_millis(200);
const NAMESPACE: &str = "cnde-osd";
const SHOWN_FOR: Duration = Duration::from_millis(1500);
/// Each press moves the level this much (percent).
const STEP: u32 = 5;

pub struct Osd {
    pub layer: LayerSurface,
    icon: String,
    /// `None`: no level to show (just the icon and a word).
    level: Option<u32>,
    label: String,
    scale: i32,
    configured: bool,
    hide: Option<RegistrationToken>,
    /// For the frost behind it.
    output: Option<String>,
    frost_sent: bool,
    shown: Instant,
    leaving: Option<Instant>,
    frame_pending: bool,
}

impl Osd {
    /// How far in it is (0-1).
    fn presence(&self, now: Instant, duration: Duration) -> f32 {
        use cnde_ui::anim::{ease_out, progress};
        match self.leaving {
            Some(at) => 1.0 - ease_out(progress(at, duration, now)),
            None => ease_out(progress(self.shown, duration, now)),
        }
    }
}

impl App {
    /// A volume or brightness key: change it and show where it's got to.
    /// A playback key goes to the player on show and names the track.
    pub fn media_key(&mut self, key: MediaKey) {
        match key {
            MediaKey::PlayPause | MediaKey::Next | MediaKey::Previous | MediaKey::Stop => {
                let Some(player) = self.media.key_target() else {
                    return;
                };
                let (op, icon) = match key {
                    MediaKey::Next => (Op::Next, "media-skip-forward-symbolic"),
                    MediaKey::Previous => (Op::Previous, "media-skip-backward-symbolic"),
                    MediaKey::Stop => (Op::Stop, "media-playback-stop-symbolic"),
                    // The icon for what it's about to do.
                    _ if player.status == Status::Playing => {
                        (Op::PlayPause, "media-playback-pause-symbolic")
                    }
                    _ => (Op::PlayPause, "media-playback-start-symbolic"),
                };
                let (bus, title) = (player.bus.clone(), player.title.clone());
                self.media.control(&bus, op);
                self.show_osd(icon, None, &title);
            }
            MediaKey::VolumeUp | MediaKey::VolumeDown | MediaKey::Mute => {
                let Some(mut volume) = self.status.volume else {
                    return;
                };
                match key {
                    MediaKey::Mute => {
                        volume.muted = !volume.muted;
                        self.actions.run(Action::ToggleMute);
                    }
                    _ => {
                        volume.percent = if key == MediaKey::VolumeUp {
                            (volume.percent + STEP).min(100)
                        } else {
                            volume.percent.saturating_sub(STEP)
                        };
                        // Turning it up means wanting to hear it.
                        if volume.muted && key == MediaKey::VolumeUp {
                            volume.muted = false;
                            self.actions.run(Action::ToggleMute);
                        }
                        self.actions.run(Action::SetVolume(volume.percent));
                    }
                }
                if !volume.muted {
                    self.event_sound("audio-volume-change");
                }
                // Ours until PipeWire reports back, so quick presses add up.
                self.status.volume = Some(volume);
                let level = (!volume.muted).then_some(volume.percent);
                self.show_osd(volume_icon(volume), level, "Muted");
                self.redraw_all();
            }
            MediaKey::MicMute => {
                self.actions.run(Action::ToggleMicMute);
                self.show_osd("microphone-sensitivity-muted-symbolic", None, "Microphone");
            }
            MediaKey::BrightnessUp | MediaKey::BrightnessDown => {
                let Some(backlight) = crate::status::read_backlight() else {
                    return;
                };
                // Small steps near the bottom, where they show most.
                let current = backlight.percent();
                let step = if current <= 10 { 2 } else { STEP };
                let percent = if key == MediaKey::BrightnessUp {
                    (current + step).min(100)
                } else {
                    current.saturating_sub(step).max(1)
                };
                self.actions.run(Action::SetBrightness(percent));
                self.show_osd("display-brightness-symbolic", Some(percent), "");
            }
        }
    }

    fn show_osd(&mut self, icon: &str, level: Option<u32>, label: &str) {
        if self.osd.is_none() {
            let Some(panel) = self.panels.first() else {
                return;
            };
            let (output, scale) = (panel.output.clone(), panel.scale());
            let output_name = self.outputs.info(&output).and_then(|i| i.name);
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(
                &self.qh,
                surface,
                Layer::Overlay,
                Some(NAMESPACE),
                Some(&output),
            );
            layer.set_anchor(Anchor::BOTTOM);
            layer.set_margin(0, 0, BOTTOM_MARGIN - RISE as i32, 0);
            layer.set_size(SIZE.0, SIZE.1 + RISE as u32);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            if let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
                // Never in the way of the pointer.
                layer
                    .wl_surface()
                    .set_input_region(Some(region.wl_region()));
            }
            layer.wl_surface().set_buffer_scale(scale);
            layer.commit();
            self.osd = Some(Osd {
                layer,
                icon: icon.to_string(),
                level,
                label: label.to_string(),
                scale,
                configured: false,
                hide: None,
                output: output_name,
                frost_sent: false,
                shown: Instant::now(),
                leaving: None,
                frame_pending: false,
            });
        }
        let Some(osd) = &mut self.osd else {
            return;
        };
        osd.icon = icon.to_string();
        osd.level = level;
        osd.label = label.to_string();
        // Pressed again on its way out: come back from where it's got to.
        if osd.leaving.is_some() {
            let now = Instant::now();
            let duration = cnde_ui::anim::scaled(ANIM);
            let presence = osd.presence(now, duration);
            osd.leaving = None;
            osd.shown = now - duration.mul_f32(presence);
        }
        if let Some(timer) = osd.hide.take() {
            self.loop_handle.remove(timer);
        }
        osd.hide = self
            .loop_handle
            .insert_source(Timer::from_duration(SHOWN_FOR), |_, _, app| {
                if let Some(osd) = &mut app.osd {
                    osd.hide = None;
                    osd.leaving = Some(Instant::now());
                }
                app.draw_osd();
                TimeoutAction::Drop
            })
            .ok();
        self.draw_osd();
    }

    /// A theme sound, if `sound.event_sounds` is on.
    pub fn event_sound(&self, name: &str) {
        if self.config.sound.event_sounds {
            crate::sounds::play(name);
        }
    }

    /// The first theme sound of `names` that is installed, if
    /// `sound.event_sounds` is on.
    pub fn event_sound_first(&self, names: &[&str]) {
        if self.config.sound.event_sounds {
            crate::sounds::play_first(names);
        }
    }

    /// Said once each time the battery drops below `power.low_battery`.
    pub fn check_low_battery(&mut self) {
        let low = self
            .status
            .battery
            .filter(|b| b.is_low(self.config.power.low_battery));
        if crate::status::low_battery_edge(&mut self.low_battery_warned, low.is_some())
            && let Some(battery) = low
        {
            self.event_sound("dialog-warning");
            let text = format!("Battery low \u{2014} {:.0}%", battery.percent);
            self.show_osd(&crate::panel::battery_icon(battery), None, &text);
        }
    }

    /// Take the level display down, and the frost behind it.
    pub fn drop_osd(&mut self) {
        let Some(osd) = self.osd.take() else {
            return;
        };
        if let Some(timer) = osd.hide {
            self.loop_handle.remove(timer);
        }
        if let Some(output) = osd.output.filter(|_| osd.frost_sent) {
            self.ipc.send(&cnde_ipc::Request::NotificationFrost {
                output,
                cards: Vec::new(),
                layer: NAMESPACE.to_string(),
            });
        }
    }

    /// The compositor is ready for a new frame.
    pub fn osd_frame(&mut self, surface: &WlSurface) {
        if let Some(osd) = &mut self.osd
            && osd.layer.wl_surface() == surface
        {
            osd.frame_pending = false;
            self.draw_osd();
        }
    }

    pub fn osd_configured(&mut self) {
        if let Some(osd) = &mut self.osd {
            osd.configured = true;
        }
        self.draw_osd();
    }

    fn draw_osd(&mut self) {
        let Some(osd) = &self.osd else {
            return;
        };
        if !osd.configured {
            return;
        }
        let now = Instant::now();
        let duration = cnde_ui::anim::scaled(ANIM);
        if osd
            .leaving
            .is_some_and(|at| cnde_ui::anim::progress(at, duration, now) >= 1.0)
        {
            self.drop_osd();
            return;
        }
        let presence = osd.presence(now, duration);
        let animating = match osd.leaving {
            Some(_) => true,
            None => now < osd.shown + duration,
        };
        let drop = RISE * (1.0 - presence);
        let scale = osd.scale;
        let (w, h) = (SIZE.0 as i32 * scale, (SIZE.1 as f32 + RISE) as i32 * scale);
        let Some(mut scratch) = Pixmap::new(SIZE.0 * scale as u32, SIZE.1 * scale as u32) else {
            return;
        };
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate the level display");
                return;
            }
        };
        pixels.fill(0);
        let (sw, sh) = (scratch.width(), scratch.height());
        if let Some(mut canvas) = Canvas::new(scratch.data_mut(), sw, sh, scale as f32) {
            let t = &self.theme;
            let (lw, lh) = (SIZE.0 as f32, SIZE.1 as f32);
            canvas.rounded_rect(0.0, 0.0, lw, lh, lh / 2.0, t.qs_background);
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: t,
            };
            let cy = lh / 2.0;
            p.icon(&osd.icon, 30.0, cy, 20.0, t.popup_text);
            let left = 54.0;
            let right = lw - 22.0;
            match osd.level {
                Some(level) => {
                    // The number, then the bar between it and the icon.
                    let text = level.to_string();
                    let tw = p.measure("100", t.font_size, Weight::Normal);
                    p.label_v(
                        &text,
                        right - tw,
                        cy,
                        tw + 1.0,
                        t.font_size,
                        Weight::Normal,
                        t.qs_dim,
                    );
                    let bar_w = right - tw - 12.0 - left;
                    let fill = bar_w * level.min(100) as f32 / 100.0;
                    p.canvas
                        .rounded_rect(left, cy - 3.0, bar_w, 6.0, 3.0, t.qs_slider_track);
                    if fill > 0.0 {
                        p.canvas.rounded_rect(
                            left,
                            cy - 3.0,
                            fill.max(6.0),
                            6.0,
                            3.0,
                            t.qs_slider_fill,
                        );
                    }
                }
                None => {
                    p.label_v(
                        &osd.label,
                        left,
                        cy,
                        right - left,
                        t.font_size,
                        Weight::Normal,
                        t.popup_text,
                    );
                }
            }
        }
        // Laid over the surface with its fade, rising into place.
        if let Some(mut target) = tiny_skia::PixmapMut::from_bytes(pixels, w as u32, h as u32) {
            target.draw_pixmap(
                0,
                (drop * scale as f32).round() as i32,
                scratch.as_ref(),
                &PixmapPaint {
                    opacity: presence,
                    ..PixmapPaint::default()
                },
                Transform::identity(),
                None,
            );
        }
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let Some(osd) = &mut self.osd else {
            return;
        };
        let surface = osd.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            if animating && !osd.frame_pending {
                surface.frame(&self.qh, FrameCallbackData(surface.clone()));
                osd.frame_pending = true;
            }
            surface.commit();
        }
        osd.frost_sent = true;
        if let Some(output) = osd.output.clone() {
            self.ipc.send(&cnde_ipc::Request::NotificationFrost {
                output,
                cards: vec![cnde_ipc::CardFrost {
                    rect: [0.0, drop.into(), SIZE.0.into(), SIZE.1.into()],
                    alpha: presence,
                    radius: Some(SIZE.1 as f64 / 2.0),
                }],
                layer: NAMESPACE.to_string(),
            });
        }
    }
}
