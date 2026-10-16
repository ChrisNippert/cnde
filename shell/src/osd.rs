//! The on-screen display: a pill near the bottom of the screen showing the
//! volume or brightness after its key is pressed, gone after a moment.

use std::time::Duration;

use cnde_ipc::MediaKey;
use cnde_ui::{Canvas, Color, Painter, Weight};
use smithay_client_toolkit::{
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        client::protocol::wl_shm,
    },
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
const SHOWN_FOR: Duration = Duration::from_millis(1500);
/// Each press moves the level this much (percent).
const STEP: u32 = 5;

pub struct Osd {
    pub layer: LayerSurface,
    icon: &'static str,
    /// `None`: no level to show (just the icon and a word).
    level: Option<u32>,
    label: &'static str,
    scale: i32,
    configured: bool,
    hide: Option<RegistrationToken>,
}

impl App {
    /// A volume or brightness key: change it and show where it's got to.
    pub fn media_key(&mut self, key: MediaKey) {
        match key {
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

    fn show_osd(&mut self, icon: &'static str, level: Option<u32>, label: &'static str) {
        if self.osd.is_none() {
            let Some(panel) = self.panels.first() else {
                return;
            };
            let (output, scale) = (panel.output.clone(), panel.scale());
            let surface = self.compositor.create_surface(&self.qh);
            let layer = self.layer_shell.create_layer_surface(
                &self.qh,
                surface,
                Layer::Overlay,
                Some("cnde-osd"),
                Some(&output),
            );
            layer.set_anchor(Anchor::BOTTOM);
            layer.set_margin(0, 0, BOTTOM_MARGIN, 0);
            layer.set_size(SIZE.0, SIZE.1);
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
                icon,
                level,
                label,
                scale,
                configured: false,
                hide: None,
            });
        }
        let Some(osd) = &mut self.osd else {
            return;
        };
        osd.icon = icon;
        osd.level = level;
        osd.label = label;
        if let Some(timer) = osd.hide.take() {
            self.loop_handle.remove(timer);
        }
        osd.hide = self
            .loop_handle
            .insert_source(Timer::from_duration(SHOWN_FOR), |_, _, app| {
                app.osd = None;
                TimeoutAction::Drop
            })
            .ok();
        self.draw_osd();
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
        let scale = osd.scale;
        let (w, h) = (SIZE.0 as i32 * scale, SIZE.1 as i32 * scale);
        let (buffer, pixels) = match self.pool.create_buffer(w, h, w * 4, self.shm_format) {
            Ok(created) => created,
            Err(err) => {
                warn!(?err, "can't allocate the level display");
                return;
            }
        };
        if let Some(mut canvas) = Canvas::new(pixels, w as u32, h as u32, scale as f32) {
            let t = &self.theme;
            let (lw, lh) = (SIZE.0 as f32, SIZE.1 as f32);
            canvas.clear(Color::rgba(0, 0, 0, 0.0));
            canvas.rounded_rect(0.0, 0.0, lw, lh, lh / 2.0, t.qs_background);
            let mut p = Painter {
                canvas: &mut canvas,
                fonts: &mut self.fonts,
                icons: &mut self.icons,
                theme: t,
            };
            let cy = lh / 2.0;
            p.icon(osd.icon, 30.0, cy, 20.0, t.popup_text);
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
                        osd.label,
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
        if self.shm_format == wl_shm::Format::Argb8888 {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }
        let surface = osd.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            surface.commit();
        }
    }
}
