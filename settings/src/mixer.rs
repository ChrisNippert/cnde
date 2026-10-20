//! The Sound page's mixer: a section for each output device with the apps
//! playing to it indented below, then the inputs. [`Layout`] works out
//! where everything goes (and what a drop lands on) from the audio state
//! alone; [`paint`] draws it and records the [`Hit`]s for the pointer.

use cnde_ui::{Color, Painter, Rect, Weight, anim::SHORT};

use crate::{
    sound::{App, Command, Device, Kind, State},
    view::{Anim, AnimKey, Painted, Target, Ui},
};

const PAD: f32 = 14.0;
const HEAD_H: f32 = 62.0;
const APP_H: f32 = 46.0;
const SECTION_GAP: f32 = 10.0;
const HEADING_H: f32 = 28.0;
const EMPTY_H: f32 = 32.0;
const PCT_W: f32 = 40.0;
/// The "⋯" button's column, kept free in device rows so the sliders line up.
const MENU_W: f32 = 28.0;
const DEVICE_ICON: f32 = 36.0;
const APP_ICON: f32 = 28.0;
const DEVICE_SLIDER: f32 = 170.0;
const APP_SLIDER: f32 = 140.0;
const DEFAULT_BUTTON_W: f32 = 96.0;
/// Where the guide line sits, from the section's left: under the device icon.
const GUIDE: f32 = PAD + DEVICE_ICON / 2.0;
const MENU_PANEL_W: f32 = 240.0;
const MENU_ROW: f32 = 34.0;
const MENU_TITLE: f32 = 30.0;
/// A pointer this far from where it went down is dragging, not clicking.
pub const DRAG_SLOP: f64 = 6.0;

const RECORDING: Color = Color::rgb(0xe8, 0xa3, 0x3d);

/// Something on the mixer the pointer can hit. Indices are into the
/// [`State`]: `apps`, `outputs` or `inputs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    /// An app's whole row: press and drag it to another output.
    AppRow(usize),
    /// The app's icon: click to mute.
    AppIcon(usize),
    AppSlider(usize),
    /// The "⋯" button, opening the list of outputs to play on.
    AppMenu(usize),
    /// An output in the open menu.
    MenuItem(usize),
    /// The open menu's panel, so clicks on it don't fall through.
    MenuBody,
    OutputSlider(usize),
    InputSlider(usize),
    /// "Make default" on an input.
    MakeDefault(usize),
}

impl Hit {
    /// The app a hit is about.
    pub fn app(self) -> Option<usize> {
        match self {
            Hit::AppRow(i) | Hit::AppIcon(i) | Hit::AppSlider(i) | Hit::AppMenu(i) => Some(i),
            _ => None,
        }
    }

    pub fn slider(self, state: &State) -> Option<SliderId> {
        match self {
            Hit::AppSlider(i) => Some(SliderId::App(state.apps.get(i)?.id())),
            Hit::OutputSlider(i) => Some(SliderId::Output(state.outputs.get(i)?.name.clone())),
            Hit::InputSlider(i) => Some(SliderId::Input(state.inputs.get(i)?.name.clone())),
            _ => None,
        }
    }
}

/// A slider by what it sets (not by position: the list changes under it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliderId {
    /// An app row, by [`App::id`].
    App(String),
    Output(String),
    Input(String),
}

impl SliderId {
    fn knob(&self) -> f32 {
        match self {
            SliderId::App(_) => 12.0,
            _ => 16.0,
        }
    }
}

/// An app row being pressed on: it becomes a drag once the pointer moves.
#[derive(Debug, Clone, PartialEq)]
pub struct Drag {
    /// [`App::id`].
    pub app: String,
    pub origin: (f64, f64),
    pub pos: (f64, f64),
    pub moved: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionRect {
    /// Index into `State::outputs`.
    pub output: usize,
    /// The header and the apps, which a drop lands on.
    pub rect: Rect,
    pub head: Rect,
    /// Index into `State::apps`, and where its row is.
    pub apps: Vec<(usize, Rect)>,
}

/// The open "Play on" menu: the app it's for (an index into `State::apps`),
/// its panel, and its entries (an output, and where it is).
pub type Menu = (usize, Rect, Vec<(usize, Rect)>);

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub height: f32,
    /// In paint order; the pointer is tested from the end.
    pub hits: Vec<(Rect, Hit)>,
    pub sections: Vec<SectionRect>,
    /// Index into `State::inputs`, and where its row is.
    pub inputs: Vec<(usize, Rect)>,
    pub output_heading: f32,
    pub input_heading: f32,
    /// Where "Apps appear here while they play sound" goes, if it shows.
    pub empty_line: Option<f32>,
    /// The open "Play on" menu.
    pub menu: Option<Menu>,
}

/// The outputs worth a section, default first: not an unplugged jack
/// (unless it's the default or something plays to it).
pub fn shown_outputs(state: &State) -> Vec<usize> {
    let mut shown: Vec<usize> = (0..state.outputs.len())
        .filter(|&i| {
            let d = &state.outputs[i];
            d.available || d.default || state.apps.iter().any(|a| a.sink == d.name)
        })
        .collect();
    shown.sort_by_key(|&i| !state.outputs[i].default);
    shown
}

/// The apps playing to `output`, as indices into `State::apps`.
pub fn apps_on(state: &State, output: usize) -> Vec<usize> {
    let Some(device) = state.outputs.get(output) else {
        return Vec::new();
    };
    (0..state.apps.len())
        .filter(|&i| state.apps[i].sink == device.name)
        .collect()
}

/// Who records from `input`, comma-separated.
pub fn recording(state: &State, input: &Device) -> String {
    let apps: Vec<&str> = state
        .recorders
        .iter()
        .filter(|r| r.source == input.name)
        .map(|r| r.app.as_str())
        .collect();
    apps.join(", ")
}

/// Where a device row's parts go; `x`, `y`, `w` are its section's.
struct Row {
    icon: Rect,
    text_x: f32,
    text_w: f32,
    slider: Rect,
    pct: Rect,
}

#[allow(clippy::too_many_arguments)]
fn row_rects(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    indent: f32,
    icon: f32,
    slider_w: f32,
    extra: f32,
) -> Row {
    let cy = y + h / 2.0;
    let pct_x = x + w - PAD - MENU_W - 6.0 - PCT_W;
    let slider = Rect::new(pct_x - 8.0 - slider_w, cy - 14.0, slider_w, 28.0);
    let icon_rect = Rect::new(x + indent, cy - icon / 2.0, icon, icon);
    let text_x = icon_rect.right() + 12.0;
    Row {
        icon: icon_rect,
        text_x,
        text_w: (slider.x - 12.0 - extra - text_x).max(40.0),
        slider,
        pct: Rect::new(pct_x, cy - 10.0, PCT_W, 20.0),
    }
}

fn head_row(x: f32, y: f32, w: f32, extra: f32) -> Row {
    row_rects(x, y, w, HEAD_H, PAD, DEVICE_ICON, DEVICE_SLIDER, extra)
}

fn app_row(x: f32, y: f32, w: f32) -> Row {
    row_rects(x, y, w, APP_H, GUIDE + 14.0, APP_ICON, APP_SLIDER, 0.0)
}

fn menu_button(x: f32, y: f32, w: f32) -> Rect {
    Rect::new(x + w - PAD - MENU_W, y + APP_H / 2.0 - 14.0, MENU_W, 28.0)
}

fn default_button(slider: Rect, y: f32) -> Rect {
    Rect::new(
        slider.x - 8.0 - DEFAULT_BUTTON_W,
        y + HEAD_H / 2.0 - 13.0,
        DEFAULT_BUTTON_W,
        26.0,
    )
}

impl Layout {
    /// The mixer for `state` with its top-left at (`x`, `y`), `w` wide;
    /// `menu` is the [`App::id`] whose "Play on" menu is open.
    pub fn new(state: &State, x: f32, y: f32, w: f32, menu: Option<&str>) -> Layout {
        let mut l = Layout::default();
        let mut cy = y;
        l.output_heading = cy;
        cy += HEADING_H;
        let mut menu_anchor = None;
        for output in shown_outputs(state) {
            let apps = apps_on(state, output);
            let h = HEAD_H
                + if apps.is_empty() {
                    0.0
                } else {
                    apps.len() as f32 * APP_H + 8.0
                };
            let rect = Rect::new(x, cy, w, h);
            let head = Rect::new(x, cy, w, HEAD_H);
            let row = head_row(x, cy, w, 0.0);
            l.hits.push((row.slider, Hit::OutputSlider(output)));
            let mut app_rects = Vec::new();
            let mut ay = cy + HEAD_H;
            for &app in &apps {
                let row_rect = Rect::new(x + GUIDE + 6.0, ay, w - GUIDE - 6.0 - 6.0, APP_H);
                let r = app_row(x, ay, w);
                let button = menu_button(x, ay, w);
                l.hits.push((row_rect, Hit::AppRow(app)));
                l.hits.push((r.icon, Hit::AppIcon(app)));
                l.hits.push((r.slider, Hit::AppSlider(app)));
                l.hits.push((button, Hit::AppMenu(app)));
                if menu.is_some_and(|id| state.apps[app].id() == id) {
                    menu_anchor = Some((app, button));
                }
                app_rects.push((app, row_rect));
                ay += APP_H;
            }
            l.sections.push(SectionRect {
                output,
                rect,
                head,
                apps: app_rects,
            });
            cy += h + SECTION_GAP;
        }
        if state.apps.is_empty() {
            l.empty_line = Some(cy);
            cy += EMPTY_H;
        }
        cy += 8.0;
        l.input_heading = cy;
        cy += HEADING_H;
        for (i, device) in state.inputs.iter().enumerate() {
            let rect = Rect::new(x, cy, w, HEAD_H);
            let row = head_row(x, cy, w, DEFAULT_BUTTON_W + 8.0);
            l.hits.push((row.slider, Hit::InputSlider(i)));
            if !device.default {
                l.hits
                    .push((default_button(row.slider, cy), Hit::MakeDefault(i)));
            }
            l.inputs.push((i, rect));
            cy += HEAD_H + SECTION_GAP;
        }
        l.height = (cy - SECTION_GAP - y).max(0.0);
        if let Some((app, anchor)) = menu_anchor {
            let outputs = shown_outputs(state);
            let h = MENU_TITLE + outputs.len() as f32 * MENU_ROW + 8.0;
            let panel = Rect::new(
                anchor.right() - MENU_PANEL_W,
                anchor.bottom() + 4.0,
                MENU_PANEL_W,
                h,
            );
            let mut entries = Vec::new();
            for (n, &o) in outputs.iter().enumerate() {
                entries.push((
                    o,
                    Rect::new(
                        panel.x + 6.0,
                        panel.y + MENU_TITLE + n as f32 * MENU_ROW,
                        panel.w - 12.0,
                        MENU_ROW,
                    ),
                ));
            }
            l.hits.push((panel, Hit::MenuBody));
            for &(o, r) in &entries {
                if state.outputs[o].name != state.apps[app].sink {
                    l.hits.push((r, Hit::MenuItem(o)));
                }
            }
            l.menu = Some((app, panel, entries));
        }
        l
    }

    /// The topmost thing under `point`.
    #[cfg(test)]
    pub fn hit(&self, point: (f64, f64)) -> Option<Hit> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains(point))
            .map(|(_, h)| *h)
    }

    pub fn rect(&self, hit: Hit) -> Option<Rect> {
        self.hits.iter().find(|(_, h)| *h == hit).map(|(r, _)| *r)
    }

    /// The output a drop at `point` lands on.
    pub fn drop_target(&self, point: (f64, f64)) -> Option<usize> {
        self.sections
            .iter()
            .find(|s| s.rect.contains(point))
            .map(|s| s.output)
    }
}

/// Where along a slider in `rect` (whose knob is `knob` wide) x is, in percent.
pub fn slider_percent(rect: Rect, knob: f32, x: f64) -> u32 {
    let (x0, x1) = (rect.x + knob / 2.0, rect.right() - knob / 2.0);
    (((x as f32 - x0) / (x1 - x0)).clamp(0.0, 1.0) * 100.0).round() as u32
}

/// Move a slider to the pointer at `x`: the new value shows at once, and
/// the command goes to the worker (which confirms it shortly).
pub fn set_level(state: &mut State, id: &SliderId, rect: Rect, x: f64) -> Option<Command> {
    let percent = slider_percent(rect, id.knob(), x);
    match id {
        SliderId::App(app) => {
            let entry = state.apps.iter_mut().find(|a| a.id() == *app)?;
            if entry.volume == percent {
                return None;
            }
            entry.volume = percent;
            Some(Command::Volume(entry.ids.clone(), percent))
        }
        SliderId::Output(name) | SliderId::Input(name) => {
            let (kind, devices) = match id {
                SliderId::Output(_) => (Kind::Output, &mut state.outputs),
                _ => (Kind::Input, &mut state.inputs),
            };
            let device = devices.iter_mut().find(|d| d.name == *name)?;
            if device.percent == percent {
                return None;
            }
            device.percent = percent;
            Some(Command::DeviceVolume(kind, name.clone(), percent))
        }
    }
}

/// Keep the slider being dragged where it is through a fresh report.
pub fn keep_level(old: &State, new: &mut State, id: &SliderId) {
    match id {
        SliderId::App(app) => {
            if let (Some(o), Some(n)) = (
                old.apps.iter().find(|a| a.id() == *app),
                new.apps.iter_mut().find(|a| a.id() == *app),
            ) {
                n.volume = o.volume;
            }
        }
        SliderId::Output(name) | SliderId::Input(name) => {
            let pick = |s: &State| -> Vec<Device> {
                match id {
                    SliderId::Output(_) => s.outputs.clone(),
                    _ => s.inputs.clone(),
                }
            };
            let old_devices = pick(old);
            let new_devices = match id {
                SliderId::Output(_) => &mut new.outputs,
                _ => &mut new.inputs,
            };
            if let (Some(o), Some(n)) = (
                old_devices.iter().find(|d| d.name == *name),
                new_devices.iter_mut().find(|d| d.name == *name),
            ) {
                n.percent = o.percent;
            }
        }
    }
}

pub fn toggle_mute(state: &mut State, app: usize) -> Option<Command> {
    let entry = state.apps.get_mut(app)?;
    entry.muted = !entry.muted;
    Some(Command::Mute(entry.ids.clone(), entry.muted))
}

/// Play the app (by [`App::id`]) on `output`; `None` when it already does.
pub fn move_app(state: &mut State, app: &str, output: usize) -> Option<Command> {
    let sink = state.outputs.get(output)?.name.clone();
    let entry = state.apps.iter_mut().find(|a| a.id() == app)?;
    if entry.sink == sink {
        return None;
    }
    entry.sink = sink.clone();
    Some(Command::Output(entry.ids.clone(), sink))
}

// --- Icons

/// The icon names to try for an app, best first: what the app says, then
/// its desktop entry's.
pub fn icon_candidates(app: &App, entries: &[cnde_apps::App]) -> Vec<String> {
    let mut names = Vec::new();
    if !app.icon.is_empty() {
        names.push(app.icon.clone());
    }
    if let Some(icon) = desktop_icon(app, entries)
        && !names.contains(&icon)
    {
        names.push(icon);
    }
    names
}

/// The icon of the desktop entry that launches the app: matched by the
/// binary (the entry's ID or its command), else by name.
fn desktop_icon(app: &App, entries: &[cnde_apps::App]) -> Option<String> {
    let binary = app.binary.to_lowercase();
    let by_binary = |e: &cnde_apps::App| {
        if binary.is_empty() {
            return false;
        }
        let stem = e.id.trim_end_matches(".desktop").to_lowercase();
        let program = cnde_apps::split_exec(&e.exec)
            .into_iter()
            .next()
            .and_then(|c| c.rsplit('/').next().map(str::to_lowercase));
        stem == binary
            || stem.ends_with(&format!(".{binary}"))
            || program.as_deref() == Some(&binary)
    };
    let by_name = |e: &cnde_apps::App| e.name.eq_ignore_ascii_case(&app.name);
    entries
        .iter()
        .find(|e| e.icon.is_some() && by_binary(e))
        .or_else(|| entries.iter().find(|e| e.icon.is_some() && by_name(e)))
        .and_then(|e| e.icon.clone())
}

/// The letter on an app's stand-in icon.
pub fn initial(name: &str) -> char {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or('?', |c| c.to_uppercase().next().unwrap_or(c))
}

/// The stand-in icon's colour: the same for the same name.
pub fn fallback_color(name: &str) -> Color {
    let hash = name.bytes().fold(2166136261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16777619)
    });
    let hue = (hash % 360) as f32;
    // HSL at 50% saturation, 42% lightness.
    let (s, l) = (0.5f32, 0.42f32);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (hue / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let byte = |v: f32| ((v + m) * 255.0).round() as u8;
    Color::rgb(byte(r), byte(g), byte(b))
}

// --- Painting

pub fn paint(ui: &Ui, a: &mut Anim, p: &mut Painter<'_, '_>, out: &mut Painted, rect: Rect) {
    let Some(state) = &ui.sound else {
        return;
    };
    let t = p.theme;
    let layout = Layout::new(state, rect.x, rect.y, rect.w, ui.sound_menu.as_deref());
    let entries = cnde_apps::index().map_or(&[][..], |i| i.apps.as_slice());
    let boxed = crate::view::boxed(t);
    let drag = ui.sound_drag.as_ref().filter(|d| d.moved);
    let dragged = drag.and_then(|d| state.apps.iter().find(|a| a.id() == d.app));
    let target = drag
        .and_then(|d| layout.drop_target(d.pos))
        .filter(|&o| dragged.is_some_and(|app| state.outputs[o].name != app.sink));

    p.label(
        "Output",
        rect.x + PAD,
        layout.output_heading + 6.0,
        12.0,
        Weight::Bold,
        t.window_dim,
    );
    for section in &layout.sections {
        let device = &state.outputs[section.output];
        let r = section.rect;
        let dropping = target == Some(section.output);
        if boxed {
            p.canvas
                .rounded_rect(r.x, r.y, r.w, r.h, t.card_radius, t.card_background);
        } else {
            p.canvas
                .stroke_rounded_rect(r.x, r.y, r.w, r.h, t.card_radius, 1.0, t.card_separator);
        }
        if dropping {
            p.canvas
                .rounded_rect(r.x, r.y, r.w, r.h, t.card_radius, t.accent.with_alpha(0.14));
            p.canvas
                .stroke_rounded_rect(r.x, r.y, r.w, r.h, t.card_radius, 2.0, t.accent);
        }
        let row = head_row(r.x, r.y, r.w, 0.0);
        let (sub, sub_color) = if dropping {
            ("Release to play here".to_string(), t.accent)
        } else if device.default {
            ("Default output".to_string(), t.accent)
        } else {
            ("Drag an app here to move it".to_string(), t.window_dim)
        };
        device_head(p, t, device, &row, &sub, sub_color);
        slider(
            ui,
            a,
            p,
            SliderId::Output(device.name.clone()),
            row.slider,
            device.percent,
            1.0,
        );
        pct(p, &row, device.percent, 1.0);

        // The guide line down the apps.
        if let Some((_, last)) = section.apps.last() {
            let x = r.x + GUIDE - 0.75;
            let (y0, y1) = (r.y + HEAD_H - 4.0, last.y + last.h / 2.0);
            p.canvas
                .rounded_rect(x, y0, 1.5, y1 - y0, 0.75, t.window_dim.with_alpha(0.35));
        }
        for &(app_i, row_rect) in &section.apps {
            let app = &state.apps[app_i];
            let being_dragged = drag.is_some_and(|d| d.app == app.id());
            let dim = if app.muted || being_dragged { 0.5 } else { 1.0 };
            let lit = crate::view::hover(a, ui, Target::Mixer(Hit::AppRow(app_i)));
            if lit > 0.0 && drag.is_none() {
                let fill = t.hover.with_alpha(t.hover.a * lit);
                p.canvas.rounded_rect(
                    row_rect.x,
                    row_rect.y,
                    row_rect.w,
                    row_rect.h,
                    t.button_radius,
                    fill,
                );
            }
            let ar = app_row(r.x, row_rect.y, r.w);
            app_icon(p, a, ui, app, entries, ar.icon, Hit::AppIcon(app_i));
            let cy = row_rect.y + APP_H / 2.0;
            let name = p.label_v(
                &app.name,
                ar.text_x,
                cy,
                ar.text_w,
                t.font_size,
                Weight::Normal,
                t.window_text.with_alpha(dim),
            );
            if app.muted && name < ar.text_w - 50.0 {
                p.label_v(
                    " · muted",
                    ar.text_x + name,
                    cy,
                    ar.text_w - name,
                    t.subtitle_size,
                    Weight::Normal,
                    t.window_dim,
                );
            }
            slider(
                ui,
                a,
                p,
                SliderId::App(app.id()),
                ar.slider,
                app.volume,
                dim,
            );
            pct(p, &ar, app.volume, dim);
            let button = menu_button(r.x, row_rect.y, r.w);
            let lit = crate::view::hover(a, ui, Target::Mixer(Hit::AppMenu(app_i)));
            let open = ui.sound_menu.as_deref() == Some(app.id().as_str());
            let fill = t
                .button_background
                .mix(t.button_hover, lit.max(if open { 1.0 } else { 0.0 }));
            p.canvas
                .circle(button.x + 14.0, button.y + 14.0, 14.0, fill);
            p.icon(
                "view-more-symbolic",
                button.x + 14.0,
                button.y + 14.0,
                16.0,
                t.window_text,
            );
        }
    }
    if let Some(y) = layout.empty_line {
        p.label_v(
            "Apps appear here while they play sound",
            rect.x + PAD,
            y + EMPTY_H / 2.0 - 2.0,
            rect.w - 2.0 * PAD,
            t.subtitle_size,
            Weight::Normal,
            t.window_dim,
        );
    }

    p.label(
        "Input",
        rect.x + PAD,
        layout.input_heading + 6.0,
        12.0,
        Weight::Bold,
        t.window_dim,
    );
    for &(i, r) in &layout.inputs {
        let device = &state.inputs[i];
        if boxed {
            p.canvas
                .rounded_rect(r.x, r.y, r.w, r.h, t.card_radius, t.card_background);
        } else {
            p.canvas
                .stroke_rounded_rect(r.x, r.y, r.w, r.h, t.card_radius, 1.0, t.card_separator);
        }
        let row = head_row(
            r.x,
            r.y,
            r.w,
            if device.default {
                0.0
            } else {
                DEFAULT_BUTTON_W + 8.0
            },
        );
        let who = recording(state, device);
        let (sub, color) = if !who.is_empty() {
            (format!("Recording: {who}"), RECORDING)
        } else if device.default {
            ("Default input".to_string(), t.accent)
        } else {
            ("Input".to_string(), t.window_dim)
        };
        device_head(p, t, device, &row, &sub, color);
        slider(
            ui,
            a,
            p,
            SliderId::Input(device.name.clone()),
            row.slider,
            device.percent,
            1.0,
        );
        pct(p, &row, device.percent, 1.0);
        if !device.default {
            let b = default_button(row.slider, r.y);
            let lit = crate::view::hover(a, ui, Target::Mixer(Hit::MakeDefault(i)));
            let fill = t.button_background.mix(t.button_hover, lit);
            p.canvas
                .rounded_rect(b.x, b.y, b.w, b.h, t.button_radius, fill);
            let label = "Make default";
            let w = p.measure(label, t.subtitle_size, Weight::Normal);
            p.label_v(
                label,
                b.x + (b.w - w) / 2.0,
                b.y + b.h / 2.0,
                b.w,
                t.subtitle_size,
                Weight::Normal,
                t.window_text,
            );
        }
    }

    if let Some(d) = drag
        && let Some(app) = dragged
    {
        ghost(p, app, entries, d.pos);
    }
    if let Some((app_i, panel, items)) = &layout.menu {
        menu(ui, a, p, state, *app_i, *panel, items);
    }
    for (r, hit) in &layout.hits {
        out.hits.push((*r, Target::Mixer(*hit)));
    }
    out.mixer = Some(layout);
}

/// A device's icon, name and sub-line.
fn device_head(
    p: &mut Painter<'_, '_>,
    t: &cnde_ui::Theme,
    d: &Device,
    row: &Row,
    sub: &str,
    sub_color: Color,
) {
    let c = row.icon;
    let (cx, cy) = (c.x + c.w / 2.0, c.y + c.h / 2.0);
    let (fill, ink) = if d.default {
        (t.accent.with_alpha(0.2), t.accent)
    } else {
        (t.button_background, t.window_text)
    };
    p.canvas.circle(cx, cy, c.w / 2.0, fill);
    p.icon(d.icon, cx, cy, 20.0, ink);
    p.label_v(
        &d.description,
        row.text_x,
        cy - 9.0,
        row.text_w,
        t.font_size,
        Weight::Bold,
        t.window_text,
    );
    p.label_v(
        sub,
        row.text_x,
        cy + 10.0,
        row.text_w,
        t.subtitle_size,
        Weight::Normal,
        sub_color,
    );
}

fn pct(p: &mut Painter<'_, '_>, row: &Row, percent: u32, dim: f32) {
    let t = p.theme;
    let text = format!("{}%", percent.min(100));
    let w = p.measure(&text, t.subtitle_size, Weight::Normal);
    p.label_v(
        &text,
        row.pct.right() - w,
        row.pct.y + row.pct.h / 2.0,
        row.pct.w,
        t.subtitle_size,
        Weight::Normal,
        t.window_dim.with_alpha(t.window_dim.a * dim),
    );
}

/// A thin slider in the hit area `rect`, registering itself for dragging.
fn slider(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    id: SliderId,
    rect: Rect,
    percent: u32,
    dim: f32,
) {
    let t = p.theme;
    let fraction = percent.min(100) as f32 / 100.0;
    let key = AnimKey::Level(format!("{id:?}"));
    let dragging = ui.sound_slider.as_ref().is_some_and(|(s, _)| *s == id);
    let shown = if dragging {
        a.set(key, fraction)
    } else {
        a.get(key, fraction, SHORT)
    };
    let knob = id.knob();
    let track = if knob > 14.0 { 6.0 } else { 4.0 };
    let (x0, x1) = (rect.x + knob / 2.0, rect.right() - knob / 2.0);
    let cy = rect.y + rect.h / 2.0;
    let fade = |c: Color| c.with_alpha(c.a * dim);
    p.canvas.rounded_rect(
        x0,
        cy - track / 2.0,
        x1 - x0,
        track,
        track / 2.0,
        fade(t.switch_off),
    );
    let kx = x0 + (x1 - x0) * shown.clamp(0.0, 1.0);
    p.canvas.rounded_rect(
        x0,
        cy - track / 2.0,
        kx - x0,
        track,
        track / 2.0,
        fade(t.accent),
    );
    p.canvas.circle(
        kx,
        cy + 1.0,
        knob / 2.0 + 0.5,
        Color::rgba(0, 0, 0, 0.25 * dim),
    );
    p.canvas.circle(kx, cy, knob / 2.0, fade(t.switch_knob));
}

/// An app's icon, or a coloured square with its initial; muted ones are
/// veiled with a speaker-off badge.
fn app_icon(
    p: &mut Painter<'_, '_>,
    a: &mut Anim,
    ui: &Ui,
    app: &App,
    entries: &[cnde_apps::App],
    r: Rect,
    hit: Hit,
) {
    let t = p.theme;
    let found = icon_candidates(app, entries)
        .iter()
        .any(|name| p.canvas.app_icon(&mut *p.icons, name, r.x, r.y, r.w));
    if !found {
        let color = fallback_color(&app.name);
        p.canvas.rounded_rect(r.x, r.y, r.w, r.h, 8.0, color);
        let letter = initial(&app.name).to_string();
        let w = p.measure(&letter, 15.0, Weight::Bold);
        p.label_v(
            &letter,
            r.x + (r.w - w) / 2.0,
            r.y + r.h / 2.0,
            r.w,
            15.0,
            Weight::Bold,
            Color::rgb(255, 255, 255),
        );
    }
    let lit = crate::view::hover(a, ui, Target::Mixer(hit));
    if lit > 0.0 && !app.muted {
        p.canvas
            .rounded_rect(r.x, r.y, r.w, r.h, 8.0, Color::rgba(0, 0, 0, 0.25 * lit));
    }
    if app.muted {
        p.canvas
            .rounded_rect(r.x, r.y, r.w, r.h, 8.0, t.window_background.with_alpha(0.7));
        p.icon(
            "audio-volume-muted-symbolic",
            r.x + r.w / 2.0,
            r.y + r.h / 2.0,
            18.0,
            t.window_text,
        );
    }
}

/// The row being dragged, following the pointer.
fn ghost(p: &mut Painter<'_, '_>, app: &App, entries: &[cnde_apps::App], pos: (f64, f64)) {
    let t = p.theme;
    let text_w = p.measure(&app.name, t.font_size, Weight::Normal).min(220.0);
    let (w, h) = (APP_ICON + 10.0 + text_w + 24.0, 40.0);
    let (x, y) = (pos.0 as f32 + 14.0, pos.1 as f32 - h / 2.0);
    for i in 1..=3 {
        let grow = i as f32 * 2.0;
        p.canvas.rounded_rect(
            x - grow,
            y - grow + 2.0,
            w + grow * 2.0,
            h + grow * 2.0,
            12.0 + grow,
            Color::rgba(0, 0, 0, 0.06),
        );
    }
    p.canvas
        .rounded_rect(x, y, w, h, 12.0, t.popover_background);
    p.canvas
        .stroke_rounded_rect(x, y, w, h, 12.0, 1.0, t.card_separator);
    let icon = Rect::new(x + 12.0, y + (h - APP_ICON) / 2.0, APP_ICON, APP_ICON);
    let found = icon_candidates(app, entries).iter().any(|name| {
        p.canvas
            .app_icon(&mut *p.icons, name, icon.x, icon.y, icon.w)
    });
    if !found {
        p.canvas.rounded_rect(
            icon.x,
            icon.y,
            icon.w,
            icon.h,
            8.0,
            fallback_color(&app.name),
        );
    }
    p.label_v(
        &app.name,
        icon.right() + 10.0,
        y + h / 2.0,
        text_w,
        t.font_size,
        Weight::Normal,
        t.window_text,
    );
}

/// The "Play on" list.
fn menu(
    ui: &Ui,
    a: &mut Anim,
    p: &mut Painter<'_, '_>,
    state: &State,
    app: usize,
    panel: Rect,
    items: &[(usize, Rect)],
) {
    let t = p.theme;
    for i in 1..=4 {
        let grow = i as f32 * 2.0;
        p.canvas.rounded_rect(
            panel.x - grow,
            panel.y - grow + 2.0,
            panel.w + grow * 2.0,
            panel.h + grow * 2.0,
            t.popover_radius + grow,
            Color::rgba(0, 0, 0, 0.05),
        );
    }
    p.canvas.rounded_rect(
        panel.x,
        panel.y,
        panel.w,
        panel.h,
        t.popover_radius,
        t.popover_background,
    );
    p.canvas.stroke_rounded_rect(
        panel.x,
        panel.y,
        panel.w,
        panel.h,
        t.popover_radius,
        1.0,
        t.card_separator,
    );
    p.label(
        "Play on",
        panel.x + 16.0,
        panel.y + 10.0,
        11.0,
        Weight::Bold,
        t.window_dim,
    );
    for &(o, r) in items {
        let device = &state.outputs[o];
        let current = device.name == state.apps[app].sink;
        let lit = if current {
            0.0
        } else {
            crate::view::hover(a, ui, Target::Mixer(Hit::MenuItem(o)))
        };
        if lit > 0.0 {
            p.canvas
                .rounded_rect(r.x, r.y, r.w, r.h, 6.0, t.hover.with_alpha(t.hover.a * lit));
        }
        let ink = if current { t.window_dim } else { t.window_text };
        p.icon(device.icon, r.x + 20.0, r.y + r.h / 2.0, 16.0, ink);
        let tag_w = if current { 56.0 } else { 0.0 };
        p.label_v(
            &device.description,
            r.x + 40.0,
            r.y + r.h / 2.0,
            r.w - 48.0 - tag_w,
            t.font_size,
            Weight::Normal,
            ink,
        );
        if current {
            let w = p.measure("Current", 11.0, Weight::Normal);
            p.label_v(
                "Current",
                r.right() - 10.0 - w,
                r.y + r.h / 2.0,
                w + 1.0,
                11.0,
                Weight::Normal,
                t.window_dim,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sound::{Availability, Recorder};

    pub fn device(index: u32, name: &str, default: bool) -> Device {
        Device {
            index,
            name: name.into(),
            description: name.into(),
            icon: "audio-speakers-symbolic",
            percent: 50,
            muted: false,
            default,
            available: true,
            monitor: false,
        }
    }

    pub fn app(key: &str, sink: &str, ids: Vec<u32>) -> App {
        App {
            key: key.into(),
            name: key.into(),
            ids,
            volume: 80,
            muted: false,
            sink: sink.into(),
            icon: String::new(),
            binary: key.into(),
        }
    }

    fn state() -> State {
        State {
            availability: Availability::Ready,
            outputs: vec![
                device(1, "usb", false),
                device(2, "hdmi", true),
                device(3, "bt", false),
            ],
            inputs: vec![device(10, "mic", true), device(11, "cam", false)],
            apps: vec![
                app("mpv", "usb", vec![5]),
                app("firefox", "usb", vec![6, 7]),
            ],
            recorders: vec![Recorder {
                source: "mic".into(),
                app: "discord".into(),
            }],
        }
    }

    #[test]
    fn default_output_comes_first() {
        let st = state();
        assert_eq!(shown_outputs(&st), [1, 0, 2]);
        assert_eq!(apps_on(&st, 0), [0, 1]);
        assert!(apps_on(&st, 1).is_empty());
    }

    #[test]
    fn unplugged_outputs_are_hidden_unless_in_use() {
        let mut st = state();
        st.outputs[2].available = false;
        assert_eq!(shown_outputs(&st), [1, 0]);
        st.apps.push(app("vlc", "bt", vec![9]));
        assert_eq!(shown_outputs(&st), [1, 0, 2]);
    }

    #[test]
    fn who_is_recording() {
        let st = state();
        assert_eq!(recording(&st, &st.inputs[0]), "discord");
        assert_eq!(recording(&st, &st.inputs[1]), "");
    }

    #[test]
    fn layout_stacks_sections_and_resolves_drops() {
        let st = state();
        let l = Layout::new(&st, 10.0, 100.0, 600.0, None);
        // Default (hdmi) first, empty: just a header. Then usb with two apps.
        assert_eq!(l.sections.len(), 3);
        assert_eq!(l.sections[0].output, 1);
        assert_eq!(l.sections[0].rect.h, HEAD_H);
        assert_eq!(l.sections[1].output, 0);
        assert_eq!(l.sections[1].apps.len(), 2);
        let (_, first_app) = l.sections[1].apps[0];
        let (_, second_app) = l.sections[1].apps[1];
        assert_eq!(second_app.y, first_app.y + APP_H);
        // Sections don't overlap, and the whole thing fits the height.
        assert!(l.sections[1].rect.y >= l.sections[0].rect.bottom());
        assert!(l.height >= l.inputs[1].1.bottom() - 100.0);
        // A drop on an app row lands on its section; on the gap, nowhere.
        let on = |r: Rect| (f64::from(r.x + r.w / 2.0), f64::from(r.y + r.h / 2.0));
        assert_eq!(l.drop_target(on(l.sections[0].head)), Some(1));
        assert_eq!(l.drop_target(on(first_app)), Some(0));
        let gap = (
            300.0,
            f64::from(l.sections[0].rect.bottom() + SECTION_GAP / 2.0),
        );
        assert_eq!(l.drop_target(gap), None);
        assert_eq!(l.drop_target((5.0, 110.0)), None);
        // Inputs aren't drop targets.
        assert_eq!(l.drop_target(on(l.inputs[0].1)), None);
    }

    #[test]
    fn hits_find_the_right_control() {
        let st = state();
        let l = Layout::new(&st, 0.0, 0.0, 600.0, None);
        let (_, row) = l.sections[1].apps[0];
        let r = app_row(0.0, row.y, 600.0);
        let at = |r: Rect| (f64::from(r.x + r.w / 2.0), f64::from(r.y + r.h / 2.0));
        assert_eq!(l.hit(at(r.icon)), Some(Hit::AppIcon(0)));
        assert_eq!(l.hit(at(r.slider)), Some(Hit::AppSlider(0)));
        assert_eq!(
            l.hit(at(menu_button(0.0, row.y, 600.0))),
            Some(Hit::AppMenu(0))
        );
        // The row's empty part drags the app.
        assert_eq!(
            l.hit((f64::from(r.text_x + 4.0), f64::from(row.y + 20.0))),
            Some(Hit::AppRow(0))
        );
        // Non-default inputs offer "Make default"; the default doesn't.
        let (_, cam) = l.inputs[1];
        let button = default_button(
            head_row(0.0, cam.y, 600.0, DEFAULT_BUTTON_W + 8.0).slider,
            cam.y,
        );
        assert_eq!(l.hit(at(button)), Some(Hit::MakeDefault(1)));
        assert!(!l.hits.iter().any(|(_, h)| *h == Hit::MakeDefault(0)));
        let head = head_row(0.0, l.sections[0].head.y, 600.0, 0.0);
        assert_eq!(l.hit(at(head.slider)), Some(Hit::OutputSlider(1)));
    }

    #[test]
    fn menu_lists_the_other_outputs_on_top() {
        let st = state();
        let id = st.apps[0].id();
        let l = Layout::new(&st, 0.0, 0.0, 600.0, Some(&id));
        let (app, panel, items) = l.menu.clone().unwrap();
        assert_eq!(app, 0);
        assert_eq!(items.len(), 3);
        // The app's own output is listed but can't be picked.
        let usb = items.iter().find(|(o, _)| *o == 0).unwrap().1;
        let at = |r: Rect| (f64::from(r.x + r.w / 2.0), f64::from(r.y + r.h / 2.0));
        assert_eq!(l.hit(at(usb)), Some(Hit::MenuBody));
        let hdmi = items.iter().find(|(o, _)| *o == 1).unwrap().1;
        assert_eq!(l.hit(at(hdmi)), Some(Hit::MenuItem(1)));
        // The panel absorbs clicks over what's under it.
        assert_eq!(l.hit(at(panel)), Some(Hit::MenuBody));
        assert!(
            Layout::new(&st, 0.0, 0.0, 600.0, Some("nope"))
                .menu
                .is_none()
        );
    }

    #[test]
    fn empty_state_still_has_headers_and_a_hint() {
        let mut st = state();
        st.apps.clear();
        let l = Layout::new(&st, 0.0, 0.0, 600.0, None);
        assert_eq!(l.sections.len(), 3);
        assert!(l.empty_line.is_some());
        assert!(
            Layout::new(&state(), 0.0, 0.0, 600.0, None)
                .empty_line
                .is_none()
        );
    }

    #[test]
    fn slider_maps_pointer_to_percent() {
        let r = Rect::new(100.0, 0.0, 140.0, 28.0);
        assert_eq!(slider_percent(r, 12.0, 0.0), 0);
        assert_eq!(slider_percent(r, 12.0, 1000.0), 100);
        assert_eq!(slider_percent(r, 12.0, 170.0), 50);
    }

    #[test]
    fn commands_update_the_state_at_once() {
        let mut st = state();
        let id = st.apps[1].id();
        let r = Rect::new(0.0, 0.0, 140.0, 28.0);
        assert_eq!(
            set_level(&mut st, &SliderId::App(id.clone()), r, 1000.0),
            Some(Command::Volume(vec![6, 7], 100))
        );
        assert_eq!(st.apps[1].volume, 100);
        // Same value again: nothing to send.
        assert_eq!(set_level(&mut st, &SliderId::App(id), r, 1000.0), None);
        assert_eq!(
            set_level(&mut st, &SliderId::Input("mic".into()), r, 0.0),
            Some(Command::DeviceVolume(Kind::Input, "mic".into(), 0))
        );
        assert_eq!(st.inputs[0].percent, 0);
        assert_eq!(toggle_mute(&mut st, 0), Some(Command::Mute(vec![5], true)));
        assert!(st.apps[0].muted);
        assert_eq!(toggle_mute(&mut st, 9), None);
    }

    #[test]
    fn dropping_moves_to_another_output_only() {
        let mut st = state();
        let id = st.apps[0].id();
        assert_eq!(move_app(&mut st, &id, 0), None);
        assert_eq!(
            move_app(&mut st, &id, 2),
            Some(Command::Output(vec![5], "bt".into()))
        );
        assert_eq!(st.apps[0].sink, "bt");
        assert_eq!(move_app(&mut st, "gone", 1), None);
    }

    #[test]
    fn a_dragged_slider_survives_a_refresh() {
        let mut old = state();
        old.apps[0].volume = 33;
        let mut new = state();
        let id = SliderId::App(old.apps[0].id());
        keep_level(&old, &mut new, &id);
        assert_eq!(new.apps[0].volume, 33);
        assert_eq!(new.apps[1].volume, 80);
        old.outputs[1].percent = 7;
        keep_level(&old, &mut new, &SliderId::Output("hdmi".into()));
        assert_eq!(new.outputs[1].percent, 7);
    }

    fn entry(id: &str, name: &str, exec: &str, icon: Option<&str>) -> cnde_apps::App {
        cnde_apps::App {
            id: id.into(),
            name: name.into(),
            exec: exec.into(),
            icon: icon.map(Into::into),
            mimes: Vec::new(),
            categories: Vec::new(),
            terminal: false,
            no_display: false,
            path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn icon_comes_from_the_app_then_its_desktop_entry() {
        let entries = [
            entry(
                "org.mozilla.firefox.desktop",
                "Firefox",
                "/usr/bin/firefox %u",
                Some("firefox-icon"),
            ),
            entry(
                "mpv.desktop",
                "mpv Media Player",
                "mpv --player-operation-mode=pseudo-gui -- %U",
                Some("mpv"),
            ),
            entry("noicon.desktop", "Plain", "plain", None),
        ];
        let mut a = app("firefox", "usb", vec![1]);
        // The app's own icon name goes first.
        a.icon = "web-browser".into();
        assert_eq!(
            icon_candidates(&a, &entries),
            ["web-browser", "firefox-icon"]
        );
        // By binary (the entry's ID suffix or command).
        a.icon.clear();
        assert_eq!(icon_candidates(&a, &entries), ["firefox-icon"]);
        // By name when there is no binary.
        let mut b = app("x", "usb", vec![2]);
        b.name = "MPV media player".into();
        b.binary.clear();
        assert_eq!(icon_candidates(&b, &entries), ["mpv"]);
        // No match: nothing to try, so the stand-in is drawn.
        let mut c = app("plain", "usb", vec![3]);
        c.binary = "plain".into();
        assert!(icon_candidates(&c, &entries).is_empty());
    }

    #[test]
    fn stand_in_icon_is_stable() {
        assert_eq!(initial("spotify"), 'S');
        assert_eq!(initial("  42 apps"), '4');
        assert_eq!(initial("…"), '?');
        assert_eq!(fallback_color("Spotify"), fallback_color("Spotify"));
        assert_ne!(fallback_color("Spotify"), fallback_color("Firefox"));
    }
}
