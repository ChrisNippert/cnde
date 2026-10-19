//! Which right-click menu shows where, what's in it, and doing what was
//! chosen. The menu itself (drawing, input) is `menu`.

use cnde_config::{DockPosition, toml_edit};
use smithay_client_toolkit::{
    reexports::{
        client::protocol::wl_shm,
        protocols::xdg::shell::client::xdg_positioner::{Anchor, Gravity},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{self, KeyboardInteractivity, Layer},
    },
};
use tracing::warn;

use crate::{
    App,
    dock::Item,
    menu::{Action, Entry, Menu, Placement, Value},
};

/// The desktop menu's invisible anchor surface, pixels square.
const HOLDER: i32 = 9;

impl App {
    /// Right-click on the dock at `point` (dock-surface coordinates).
    pub fn dock_menu(&mut self, point: (f64, f64), serial: u32) {
        let Some(dock) = &self.dock else {
            return;
        };
        let icon = self.config.dock.icon_size as f32;
        let position = dock.position();
        let (entries, rect) = match dock.cell_at(point, icon) {
            Some((i, cell)) if i < dock.items().len() => {
                (self.app_entries(&dock.items()[i]), cell_rect(cell))
            }
            Some((_, cell)) => (self.dock_entries(), cell_rect(cell)),
            None => (self.dock_entries(), (point.0 as i32, point.1 as i32, 1, 1)),
        };
        // Open away from the screen edge the dock is on.
        let (anchor, gravity) = match position {
            DockPosition::Left => (Anchor::Right, Gravity::Right),
            DockPosition::Right => (Anchor::Left, Gravity::Left),
            _ => (Anchor::Top, Gravity::Top),
        };
        let parent = dock.layer.clone();
        let scale = dock.scale();
        self.open_menu(
            &parent,
            serial,
            Placement {
                rect,
                anchor,
                gravity,
            },
            entries,
            scale,
        );
    }

    /// Right-click on the top bar.
    pub fn panel_menu(&mut self, index: usize, point: (f64, f64), serial: u32) {
        let Some(panel) = self.panels.get(index) else {
            return;
        };
        let parent = panel.layer.clone();
        let rect = (point.0 as i32, panel_height(&self.theme), 1, 1);
        let entries = self.panel_entries();
        let scale = panel.scale();
        self.open_menu(
            &parent,
            serial,
            Placement {
                rect,
                anchor: Anchor::BottomRight,
                gravity: Gravity::BottomRight,
            },
            entries,
            scale,
        );
    }

    /// The compositor says the wallpaper was right-clicked: put a 1x1
    /// surface there to hang the menu from (see `menu`).
    pub fn desktop_menu(&mut self, output_name: &str, x: i32, y: i32, serial: u32) {
        let Some((output, scale)) = self.outputs.outputs().find_map(|o| {
            let info = self.outputs.info(&o)?;
            (info.name.as_deref() == Some(output_name)).then_some((o, info.scale_factor))
        }) else {
            return;
        };
        self.menu = None;
        let surface = self.compositor.create_surface(&self.qh);
        let holder = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Top,
            Some("cnde-menu"),
            Some(&output),
        );
        holder.set_anchor(wlr_layer::Anchor::TOP | wlr_layer::Anchor::LEFT);
        // A few pixels around the click, so the pointer is surely over it
        // when the button comes up (else the grab takes the release for a
        // click elsewhere and closes the menu at once).
        holder.set_margin((y - HOLDER / 2).max(0), 0, 0, (x - HOLDER / 2).max(0));
        holder.set_size(HOLDER as u32, HOLDER as u32);
        holder.set_exclusive_zone(-1);
        holder.set_keyboard_interactivity(KeyboardInteractivity::None);
        holder.wl_surface().set_buffer_scale(1);
        holder.commit();
        // The menu opens once the compositor configures this.
        self.desktop_holder = Some((holder, scale, serial));
    }

    /// The desktop menu's holder was configured: show it, then the menu.
    pub fn desktop_holder_configured(&mut self) {
        let Some((holder, scale, serial)) = self.desktop_holder.take() else {
            return;
        };
        match self
            .pool
            .create_buffer(HOLDER, HOLDER, HOLDER * 4, wl_shm::Format::Argb8888)
        {
            Ok((buffer, pixels)) => {
                pixels.fill(0);
                if buffer.attach_to(holder.wl_surface()).is_err() {
                    return;
                }
            }
            Err(err) => {
                warn!(?err, "can't allocate menu holder");
                return;
            }
        }
        holder.commit();
        let entries = vec![
            Entry::item("Change Background…", Action::Settings(Some("appearance"))),
            Entry::item("Display Settings", Action::Settings(Some("displays"))),
            Entry::Separator,
            Entry::item("Edit Top Bar…", Action::EditPanel),
            Entry::item("Dock Settings", Action::Settings(Some("dock"))),
            Entry::item("Appearance", Action::Settings(Some("appearance"))),
            Entry::Separator,
            Entry::item("Settings", Action::Settings(None)),
        ];
        self.open_menu(
            &holder,
            serial,
            Placement {
                rect: (HOLDER / 2, HOLDER / 2, 1, 1),
                anchor: Anchor::BottomRight,
                gravity: Gravity::BottomRight,
            },
            entries,
            scale,
        );
        if let Some(menu) = &mut self.menu {
            menu.holder = Some(holder);
        }
    }

    fn open_menu(
        &mut self,
        parent: &wlr_layer::LayerSurface,
        serial: u32,
        placement: Placement,
        entries: Vec<Entry>,
        scale: i32,
    ) {
        let Some(seat) = self.seat_handle.clone() else {
            return;
        };
        let entries = without_settings_if_missing(entries);
        // Close any open menu first: a grabbing popup that isn't the
        // child of the topmost one gets the shell disconnected.
        self.quick_settings = None;
        self.calendar = None;
        self.menu = None;
        self.menu = Menu::open(
            &self.compositor,
            &self.xdg_shell,
            &self.qh,
            parent,
            &seat,
            serial,
            placement,
            entries,
            &mut self.fonts,
            &self.theme,
            scale,
        );
    }

    pub fn draw_menu(&mut self) {
        let theme = self.popup_theme();
        if let Some(menu) = &mut self.menu {
            menu.draw(
                &self.qh,
                &mut self.pool,
                self.shm_format,
                &mut self.fonts,
                &mut self.icons,
                &theme,
            );
        }
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
        // An autohide dock the menu held out lets go after a moment.
        if let Some(dock) = &mut self.dock {
            dock.linger();
        }
        self.sync_dock();
        self.sync_dock_later();
    }

    /// An app icon's menu: its windows, a new one, pin, quit.
    fn app_entries(&self, item: &Item) -> Vec<Entry> {
        let entry = item
            .app
            .as_ref()
            .and_then(|id| self.apps.iter().find(|a| &a.id == id));
        let name = entry.map_or_else(|| item.icon.clone(), |a| a.name.clone());
        let mut entries = vec![Entry::Header(name)];
        let titles: Vec<(u64, String)> = item
            .windows
            .iter()
            .filter_map(|id| {
                let window = self.windows.iter().find(|w| w.id == *id)?;
                let title = if window.title.is_empty() {
                    window.app_id.clone()
                } else {
                    window.title.clone()
                };
                Some((*id, ellipsize(&title, 40)))
            })
            .collect();
        if titles.len() > 1 {
            for (id, title) in &titles {
                entries.push(Entry::item(title.clone(), Action::Activate(*id)));
            }
            entries.push(Entry::Separator);
        }
        if let Some(id) = &item.app {
            entries.push(Entry::item("New Window", Action::Launch(id.clone())));
            let favorites = &self.config.dock.favorites;
            let (label, list) = if item.favorite {
                let list = favorites.iter().filter(|f| *f != id).cloned().collect();
                ("Unpin from Dock", list)
            } else {
                let mut list = favorites.clone();
                list.push(id.clone());
                ("Pin to Dock", list)
            };
            entries.push(Entry::item(
                label,
                Action::Config(vec![("dock.favorites", Value::List(list))]),
            ));
        }
        if !item.windows.is_empty() {
            entries.push(Entry::Separator);
            let label = if item.windows.len() > 1 {
                format!("Quit ({} windows)", item.windows.len())
            } else {
                "Quit".into()
            };
            entries.push(Entry::item(label, Action::Close(item.windows.clone())));
        }
        entries
    }

    /// The dock's own menu: edit it in place, or the rest in Settings.
    fn dock_entries(&self) -> Vec<Entry> {
        let edit = if self.dock_editing {
            "Done Editing"
        } else {
            "Edit Dock…"
        };
        let mut entries = Vec::new();
        if !self.config.dock.locked {
            entries.push(Entry::item(edit, Action::EditDock));
        }
        let monitor = self.config.dock.system_monitor.trim();
        if !monitor.is_empty() {
            entries.push(Entry::item("System Monitor", Action::Run(monitor.into())));
        }
        entries.push(Entry::item("Dock Settings", Action::Settings(Some("dock"))));
        entries
    }

    fn panel_entries(&self) -> Vec<Entry> {
        let panel = &self.config.panel;
        let format = &panel.clock_format;
        let twelve = format.contains("%I") || format.contains("%-I") || format.contains("%l");
        let dated = has_date(format);
        vec![
            Entry::Section("Clock".into()),
            Entry::check(
                "24-Hour",
                !twelve,
                Action::Config(vec![(
                    "panel.clock_format",
                    Value::Str(clock_format(false, dated)),
                )]),
            ),
            Entry::check(
                "12-Hour",
                twelve,
                Action::Config(vec![(
                    "panel.clock_format",
                    Value::Str(clock_format(true, dated)),
                )]),
            ),
            Entry::check(
                "Show the Date",
                dated,
                Action::Config(vec![(
                    "panel.clock_format",
                    Value::Str(clock_format(twelve, !dated)),
                )]),
            ),
            Entry::Separator,
            Entry::check(
                "On Every Display",
                panel.all_monitors,
                Action::Config(vec![(
                    "panel.all_monitors",
                    Value::Bool(!panel.all_monitors),
                )]),
            ),
            Entry::check(
                "Frosted",
                panel.frosted,
                Action::Config(vec![("panel.frosted", Value::Bool(!panel.frosted))]),
            ),
            Entry::Separator,
            Entry::item(
                if self.panel_editing() {
                    "Done Editing"
                } else {
                    "Edit Top Bar…"
                },
                Action::EditPanel,
            ),
            Entry::item("Top Bar Settings", Action::Settings(Some("top"))),
        ]
    }

    pub fn menu_action(&mut self, action: Action) {
        self.close_menu();
        match action {
            Action::Settings(page) => crate::apps::open_settings(page),
            Action::Config(changes) => write_config(&changes),
            Action::Launch(id) => self.launch(&id),
            Action::Run(command) => crate::apps::run_command(&command),
            Action::Activate(id) => self.ipc.send(&cnde_ipc::Request::ActivateWindow { id }),
            Action::Close(ids) => {
                for id in ids {
                    self.ipc.send(&cnde_ipc::Request::CloseWindow { id });
                }
            }
            Action::EditDock if self.dock_editing => self.finish_dock_edit(),
            Action::EditDock => self.start_dock_edit(),
            Action::EditPanel if self.panel_editing() => self.finish_panel_edit(),
            Action::EditPanel => self.start_panel_edit(),
        }
    }
}

/// Drop "… Settings" items when cnde-settings isn't installed, and any
/// separators left doubled or dangling.
fn without_settings_if_missing(entries: Vec<Entry>) -> Vec<Entry> {
    if crate::apps::settings_installed() {
        return entries;
    }
    let mut out: Vec<Entry> = Vec::new();
    for entry in entries {
        match entry {
            Entry::Item {
                action: Action::Settings(_),
                ..
            } => {}
            Entry::Separator if out.is_empty() || matches!(out.last(), Some(Entry::Separator)) => {}
            other => out.push(other),
        }
    }
    while matches!(out.last(), Some(Entry::Separator)) {
        out.pop();
    }
    out
}

fn panel_height(theme: &cnde_ui::Theme) -> i32 {
    theme.panel_height as i32
}

fn cell_rect(cell: crate::dock::Rect) -> (i32, i32, i32, i32) {
    (
        cell.x as i32,
        cell.y as i32,
        cell.w.round() as i32,
        cell.h.round() as i32,
    )
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}

/// Whether a clock format shows the day or date.
fn has_date(format: &str) -> bool {
    ["%a", "%A", "%b", "%B", "%d", "%-d", "%e", "%m"]
        .iter()
        .any(|code| format.contains(code))
}

/// The standard clock formats (as in Settings' clock dropdown).
fn clock_format(twelve: bool, date: bool) -> String {
    let time = if twelve { "%-I:%M %p" } else { "%H:%M" };
    if date {
        format!("%a %b %-d  {time}")
    } else {
        time.into()
    }
}

/// Write `changes` to config.toml; the reload that follows applies them.
pub fn write_config(changes: &[(&str, Value)]) {
    let Some(path) = cnde_config::default_path() else {
        return;
    };
    let result = cnde_config::ConfigFile::open(&path).and_then(|mut file| {
        for (key, value) in changes {
            let value: toml_edit::Value = match value {
                Value::Str(v) => v.as_str().into(),
                Value::Int(v) => (*v).into(),
                Value::Bool(v) => (*v).into(),
                Value::List(items) => toml_edit::Value::Array(items.iter().collect()),
            };
            file.set(key, value);
        }
        file.save()
    });
    if let Err(err) = result {
        warn!(%err, "can't save the change");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_round_trip() {
        assert_eq!(clock_format(false, true), "%a %b %-d  %H:%M");
        assert_eq!(clock_format(true, false), "%-I:%M %p");
        assert!(has_date(&clock_format(true, true)));
        assert!(!has_date(&clock_format(false, false)));
        // The default config's format is one of ours.
        let default = cnde_config::Config::default().panel.clock_format;
        assert_eq!(default, clock_format(false, true));
    }
}
