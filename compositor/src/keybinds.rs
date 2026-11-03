//! Key bindings (ARCHITECTURE.md §5.2): `[shortcuts]` in the config, compiled
//! into a table of modifiers and keysym. The escape hatch, VT switching and
//! media keys stay hard-coded.

use smithay::input::keyboard::{Keysym, ModifiersState, keysyms, xkb};
use tracing::warn;

/// The modifier our bindings use. Super on real hardware; Alt when nested,
/// because the host desktop grabs Super for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKey {
    Super,
    Alt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    SpawnTerminal,
    SpawnBrowser,
    /// The default file manager.
    SpawnFiles,
    SpawnSystemMonitor,
    /// Minimize everything on this workspace, or bring it back.
    ShowDesktop,
    /// `[[shortcuts.custom]]` number n.
    Custom(usize),
    CloseWindow,
    Maximize,
    Restore,
    SnapLeft,
    SnapRight,
    ToggleFullscreen,
    /// Ctrl+Alt+F1..F12 on real hardware.
    SwitchVt(i32),
    /// Tapping the mod key alone: the overview, or a configured launcher.
    Launcher,
    CloseOverview,
    /// Escape while the screen is frozen for a screenshot.
    CancelScreenshot,
    /// Super+A: the overview's app grid.
    AppGrid,
    /// Super+Shift+S or Print Screen.
    Screenshot,
    /// Slide the workspace strip: +1 = next (content moves left).
    /// Right is +1; `[input.keyboard] natural_workspace_keys` flips it.
    /// `carry` brings the focused window along.
    Workspace {
        step: isize,
        carry: bool,
    },
    /// Volume and brightness keys, passed to the shell.
    Media(cnde_ipc::MediaKey),
    /// Super+L.
    Lock,
    /// Super+T: the current workspace floats or tiles.
    ToggleTiling,
    /// Super+Shift+T: float the focused tile, or tile it again.
    ToggleFloating,
    /// Super+Enter: swap the focused tile with the main one.
    SwapMaster,
    /// Super+] widens the main tile (+1), Super+[ narrows it (-1).
    MasterRatio(i8),
    /// Super+Shift+arrow on a tiling workspace: swap the focused tile with
    /// its neighbour that way, or move it to the next monitor's tiles.
    MoveTile(Direction),
    /// Alt+Tab / Super+Tab (Shift: backwards): the window switcher, which
    /// stays up while the modifier is held.
    Switcher {
        back: bool,
    },
    /// Escape while the switcher is up.
    CancelSwitcher,
}

/// An arrow key's direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The volume, brightness and playback keys, with or without modifiers.
pub fn media_key(sym: Keysym) -> Option<cnde_ipc::MediaKey> {
    use cnde_ipc::MediaKey;
    Some(match sym.raw() {
        keysyms::KEY_XF86AudioRaiseVolume => MediaKey::VolumeUp,
        keysyms::KEY_XF86AudioLowerVolume => MediaKey::VolumeDown,
        keysyms::KEY_XF86AudioMute => MediaKey::Mute,
        keysyms::KEY_XF86AudioMicMute => MediaKey::MicMute,
        keysyms::KEY_XF86MonBrightnessUp => MediaKey::BrightnessUp,
        keysyms::KEY_XF86MonBrightnessDown => MediaKey::BrightnessDown,
        keysyms::KEY_XF86AudioPlay | keysyms::KEY_XF86AudioPause => MediaKey::PlayPause,
        keysyms::KEY_XF86AudioNext => MediaKey::Next,
        keysyms::KEY_XF86AudioPrev => MediaKey::Previous,
        keysyms::KEY_XF86AudioStop => MediaKey::Stop,
        _ => return None,
    })
}

/// Whether `sym` is the mod key itself (left or right).
pub fn is_mod_key(mod_key: ModKey, sym: Keysym) -> bool {
    let raw = sym.raw();
    match mod_key {
        ModKey::Super => raw == keysyms::KEY_Super_L || raw == keysyms::KEY_Super_R,
        ModKey::Alt => raw == keysyms::KEY_Alt_L || raw == keysyms::KEY_Alt_R,
    }
}

/// Ctrl+Alt+Fn: the keymap turns these into XF86Switch_VT_n keysyms.
pub fn vt_switch(modified: Keysym) -> Option<Action> {
    let raw = modified.raw();
    (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
        .contains(&raw)
        .then(|| Action::SwitchVt((raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32))
}

/// Modifiers plus a (lower-cased) keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Combo {
    logo: bool,
    alt: bool,
    ctrl: bool,
    shift: bool,
    sym: u32,
}

fn lower(sym: u32) -> u32 {
    if (0x41..=0x5a).contains(&sym) {
        sym + 0x20
    } else {
        sym
    }
}

/// "Super+Shift+Q" as written in the config. "Super" (also Mod, Mod4, Logo,
/// Win) is the mod key; [`Combo::for_mod_key`] adapts it to the backend.
pub fn parse_combo(text: &str) -> Result<Combo, String> {
    let mut combo = Combo {
        logo: false,
        alt: false,
        ctrl: false,
        shift: false,
        sym: 0,
    };
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let Some((key, mods)) = parts.split_last() else {
        return Err("empty".into());
    };
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "super" | "mod" | "mod4" | "logo" | "win" => combo.logo = true,
            "alt" | "mod1" => combo.alt = true,
            "ctrl" | "control" | "ctl" => combo.ctrl = true,
            "shift" => combo.shift = true,
            other => return Err(format!("unknown modifier \"{other}\"")),
        }
    }
    let name = match key.to_ascii_lowercase().as_str() {
        "" => return Err("no key".into()),
        "enter" => "Return",
        "esc" => "Escape",
        _ => key,
    };
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE).raw();
    if sym == keysyms::KEY_NoSymbol {
        return Err(format!("unknown key \"{key}\""));
    }
    combo.sym = lower(sym);
    Ok(combo)
}

impl Combo {
    /// Nested, the mod key is Alt: Super becomes Alt, and a Super+Alt combo
    /// (workspaces) becomes Alt+Ctrl so the two don't collide.
    fn for_mod_key(mut self, mod_key: ModKey) -> Self {
        if mod_key == ModKey::Alt && self.logo {
            self.logo = false;
            if self.alt {
                self.ctrl = true;
            }
            self.alt = true;
        }
        self
    }
}

/// Whether `sym` is a modifier key (any of them), which isn't a combo yet.
pub fn is_modifier_sym(sym: Keysym) -> bool {
    (keysyms::KEY_Shift_L..=keysyms::KEY_Hyper_R).contains(&sym.raw())
        || sym.raw() == keysyms::KEY_ISO_Level3_Shift
}

/// The combo as the config writes it ("Super+Ctrl+Shift+S"), for Settings'
/// shortcut capture. The inverse of `for_mod_key`: nested, Alt stands for
/// Super and Alt+Ctrl for Super+Alt.
pub fn combo_string(mod_key: ModKey, mods: &ModifiersState, sym: Keysym) -> String {
    let (mut logo, mut alt, mut ctrl) = (mods.logo, mods.alt, mods.ctrl);
    if mod_key == ModKey::Alt && alt {
        alt = ctrl;
        ctrl = false;
        logo = true;
    }
    let mut shift = mods.shift;
    let mut raw = sym.raw();
    if raw == keysyms::KEY_ISO_Left_Tab {
        raw = keysyms::KEY_Tab;
        shift = true;
    }
    let mut out = String::new();
    for (on, name) in [(logo, "Super"), (ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift")] {
        if on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&xkb::keysym_get_name(Keysym::new(lower(raw))));
    out
}

/// What a bound key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    Action(Action),
    Custom(usize),
}

fn action_named(name: &str) -> Option<Action> {
    use Direction::*;
    Some(match name {
        "terminal" => Action::SpawnTerminal,
        "browser" => Action::SpawnBrowser,
        "files" => Action::SpawnFiles,
        "system_monitor" => Action::SpawnSystemMonitor,
        "close" => Action::CloseWindow,
        "lock" => Action::Lock,
        "app_grid" => Action::AppGrid,
        "show_desktop" => Action::ShowDesktop,
        "screenshot" => Action::Screenshot,
        "switcher" => Action::Switcher { back: false },
        "switcher_back" => Action::Switcher { back: true },
        "maximize" => Action::Maximize,
        "restore" => Action::Restore,
        "snap_left" => Action::SnapLeft,
        "snap_right" => Action::SnapRight,
        "fullscreen" => Action::ToggleFullscreen,
        "toggle_tiling" => Action::ToggleTiling,
        "toggle_floating" => Action::ToggleFloating,
        "swap_master" => Action::SwapMaster,
        "master_wider" => Action::MasterRatio(1),
        "master_narrower" => Action::MasterRatio(-1),
        "move_tile_left" => Action::MoveTile(Left),
        "move_tile_right" => Action::MoveTile(Right),
        "move_tile_up" => Action::MoveTile(Up),
        "move_tile_down" => Action::MoveTile(Down),
        "workspace_prev" => Action::Workspace {
            step: -1,
            carry: false,
        },
        "workspace_next" => Action::Workspace {
            step: 1,
            carry: false,
        },
        "workspace_prev_carry" => Action::Workspace {
            step: -1,
            carry: true,
        },
        "workspace_next_carry" => Action::Workspace {
            step: 1,
            carry: true,
        },
        _ => return None,
    })
}

/// The compiled `[shortcuts]`.
#[derive(Debug, Clone, Default)]
pub struct Shortcuts {
    binds: Vec<(Combo, Bound)>,
    commands: Vec<String>,
}

impl Shortcuts {
    /// Bad entries are logged and skipped. Built-in actions go in the order
    /// of `SHORTCUT_DEFAULTS`, then custom commands; on a clash the later
    /// one wins.
    pub fn build(config: &cnde_config::Shortcuts, mod_key: ModKey) -> Self {
        let mut out = Shortcuts::default();
        for name in config.bindings.keys() {
            if action_named(name).is_none() {
                warn!(action = %name, "unknown shortcut action; ignored");
            }
        }
        for (name, _, _) in cnde_config::SHORTCUT_DEFAULTS {
            let (Some(action), Some(keys)) = (action_named(name), config.bindings.get(*name))
            else {
                continue;
            };
            for key in keys {
                out.bind(name, key, Bound::Action(action), mod_key);
            }
        }
        for custom in &config.custom {
            if custom.command.trim().is_empty() {
                warn!(keys = ?custom.keys, "custom shortcut without a command; ignored");
                continue;
            }
            let index = out.commands.len();
            out.commands.push(custom.command.clone());
            for key in &custom.keys {
                out.bind("custom", key, Bound::Custom(index), mod_key);
            }
        }
        out
    }

    fn bind(&mut self, name: &str, text: &str, what: Bound, mod_key: ModKey) {
        let combo = match parse_combo(text) {
            Ok(combo) => combo.for_mod_key(mod_key),
            Err(err) => {
                warn!(action = name, key = text, %err, "bad shortcut; ignored");
                return;
            }
        };
        if let Some(i) = self.binds.iter().position(|(c, _)| *c == combo) {
            if self.binds[i].1 != what {
                warn!(
                    action = name,
                    key = text,
                    "shortcut already bound; the later one wins"
                );
            }
            self.binds.remove(i);
        }
        self.binds.push((combo, what));
    }

    /// The command of `Action::Custom(index)`.
    pub fn command(&self, index: usize) -> Option<&str> {
        self.commands.get(index).map(String::as_str)
    }
}

pub fn action_for(shortcuts: &Shortcuts, mods: &ModifiersState, sym: Keysym) -> Option<Action> {
    let mut sym = sym.raw();

    // Escape hatch (ARCHITECTURE.md §10): hard-coded, never rebindable.
    if mods.ctrl && mods.alt && sym == keysyms::KEY_BackSpace {
        return Some(Action::Quit);
    }

    // Shift+Tab arrives as ISO_Left_Tab.
    let mut shift = mods.shift;
    if sym == keysyms::KEY_ISO_Left_Tab {
        sym = keysyms::KEY_Tab;
        shift = true;
    }
    let want = Combo {
        logo: mods.logo,
        alt: mods.alt,
        ctrl: mods.ctrl,
        shift,
        sym: lower(sym),
    };
    shortcuts
        .binds
        .iter()
        .find(|(c, _)| *c == want)
        .map(|(_, b)| match *b {
            Bound::Action(action) => action,
            Bound::Custom(i) => Action::Custom(i),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(logo: bool, alt: bool, ctrl: bool, shift: bool) -> ModifiersState {
        ModifiersState {
            logo,
            alt,
            ctrl,
            shift,
            ..Default::default()
        }
    }

    fn defaults(mod_key: ModKey) -> Shortcuts {
        Shortcuts::build(&cnde_config::Shortcuts::default(), mod_key)
    }

    fn at(mod_key: ModKey, m: &ModifiersState, key: u32) -> Option<Action> {
        action_for(&defaults(mod_key), m, Keysym::from(key))
    }

    fn built(toml: &str, mod_key: ModKey) -> Shortcuts {
        let config = cnde_config::Config::parse(toml).unwrap();
        Shortcuts::build(&config.shortcuts, mod_key)
    }

    #[test]
    fn captured_combos_parse_back() {
        let m = mods(true, false, false, true);
        let text = combo_string(ModKey::Super, &m, Keysym::from(keysyms::KEY_S));
        assert_eq!(text, "Super+Shift+s");
        assert_eq!(parse_combo(&text), parse_combo("Super+Shift+S"));
        let tab = combo_string(ModKey::Super, &mods(false, true, false, true), Keysym::from(keysyms::KEY_ISO_Left_Tab));
        assert_eq!(tab, "Alt+Shift+Tab");
        // Nested, Alt is the mod key and Alt+Ctrl is Super+Alt.
        let nested = combo_string(ModKey::Alt, &mods(false, true, false, false), Keysym::from(keysyms::KEY_Left));
        assert_eq!(nested, "Super+Left");
        let nested = combo_string(ModKey::Alt, &mods(false, true, true, false), Keysym::from(keysyms::KEY_Left));
        assert_eq!(nested, "Super+Alt+Left");
        assert!(is_modifier_sym(Keysym::from(keysyms::KEY_Super_L)));
        assert!(!is_modifier_sym(Keysym::from(keysyms::KEY_a)));
    }

    #[test]
    fn super_bindings() {
        let m = mods(true, false, false, false);
        assert_eq!(
            at(ModKey::Super, &m, keysyms::KEY_q),
            Some(Action::SpawnTerminal)
        );
        assert_eq!(
            at(ModKey::Super, &m, keysyms::KEY_e),
            Some(Action::SpawnFiles)
        );
        assert_eq!(
            at(ModKey::Super, &m, keysyms::KEY_d),
            Some(Action::ShowDesktop)
        );
        let shifted = mods(true, false, false, true);
        assert_eq!(
            at(ModKey::Super, &shifted, keysyms::KEY_f),
            Some(Action::ToggleFullscreen)
        );
        assert_eq!(
            at(ModKey::Super, &shifted, keysyms::KEY_s),
            Some(Action::Screenshot)
        );
        let task = mods(false, false, true, true);
        assert_eq!(
            at(ModKey::Super, &task, keysyms::KEY_Escape),
            Some(Action::SpawnSystemMonitor)
        );
        // Close, and Print on its own.
        assert_eq!(
            at(
                ModKey::Super,
                &mods(false, true, false, false),
                keysyms::KEY_F4
            ),
            Some(Action::CloseWindow)
        );
        assert_eq!(
            at(
                ModKey::Super,
                &mods(false, false, false, false),
                keysyms::KEY_Print
            ),
            Some(Action::Screenshot)
        );
    }

    #[test]
    fn super_alt_arrows_slide_workspaces() {
        let m = mods(true, true, false, false);
        assert_eq!(
            at(ModKey::Super, &m, keysyms::KEY_Left),
            Some(Action::Workspace {
                step: -1,
                carry: false
            })
        );
        let carry = mods(true, true, false, true);
        assert_eq!(
            at(ModKey::Super, &carry, keysyms::KEY_Right),
            Some(Action::Workspace {
                step: 1,
                carry: true
            })
        );
        // Other keys with Super+Alt don't fall through to window bindings.
        assert_eq!(at(ModKey::Super, &m, keysyms::KEY_q), None);
    }

    #[test]
    fn nested_mod_is_alt_and_workspaces_use_alt_ctrl() {
        let alt = mods(false, true, false, false);
        assert_eq!(
            at(ModKey::Alt, &alt, keysyms::KEY_q),
            Some(Action::SpawnTerminal)
        );
        // Super is not the mod key there.
        assert_eq!(
            at(
                ModKey::Alt,
                &mods(true, false, false, false),
                keysyms::KEY_q
            ),
            None
        );
        let ws = mods(false, true, true, false);
        assert_eq!(
            at(ModKey::Alt, &ws, keysyms::KEY_Left),
            Some(Action::Workspace {
                step: -1,
                carry: false
            })
        );
        // Alt+Left is a snap, not a workspace.
        assert_eq!(
            at(ModKey::Alt, &alt, keysyms::KEY_Left),
            Some(Action::SnapLeft)
        );
    }

    #[test]
    fn tab_opens_the_switcher() {
        let alt = mods(false, true, false, false);
        assert_eq!(
            at(ModKey::Super, &alt, keysyms::KEY_Tab),
            Some(Action::Switcher { back: false })
        );
        assert_eq!(
            at(
                ModKey::Alt,
                &mods(false, true, false, true),
                keysyms::KEY_Tab
            ),
            Some(Action::Switcher { back: true })
        );
        assert_eq!(
            at(ModKey::Alt, &alt, keysyms::KEY_ISO_Left_Tab),
            Some(Action::Switcher { back: true })
        );
        assert_eq!(
            at(
                ModKey::Super,
                &mods(true, false, false, false),
                keysyms::KEY_Tab
            ),
            Some(Action::Switcher { back: false })
        );
        assert_eq!(
            at(
                ModKey::Super,
                &mods(false, false, false, false),
                keysyms::KEY_Tab
            ),
            None
        );
    }

    #[test]
    fn tiling_bindings() {
        let m = mods(true, false, false, false);
        let at = |m: &ModifiersState, key| at(ModKey::Super, m, key);
        let shifted = mods(true, false, false, true);
        assert_eq!(
            at(&shifted, keysyms::KEY_Left),
            Some(Action::MoveTile(Direction::Left))
        );
        assert_eq!(
            at(&shifted, keysyms::KEY_Down),
            Some(Action::MoveTile(Direction::Down))
        );
        assert_eq!(at(&m, keysyms::KEY_t), Some(Action::ToggleTiling));
        assert_eq!(at(&m, keysyms::KEY_Return), Some(Action::SwapMaster));
        assert_eq!(at(&m, keysyms::KEY_KP_Enter), Some(Action::SwapMaster));
        assert_eq!(
            at(&m, keysyms::KEY_bracketleft),
            Some(Action::MasterRatio(-1))
        );
        assert_eq!(
            at(&m, keysyms::KEY_bracketright),
            Some(Action::MasterRatio(1))
        );
        assert_eq!(at(&shifted, keysyms::KEY_t), Some(Action::ToggleFloating));
        // Plain Enter is the app's.
        assert_eq!(
            at(&mods(false, false, false, false), keysyms::KEY_Return),
            None
        );
    }

    #[test]
    fn vt_keysyms_map_to_vt_numbers() {
        assert_eq!(
            vt_switch(Keysym::from(keysyms::KEY_XF86Switch_VT_1)),
            Some(Action::SwitchVt(1))
        );
        assert_eq!(
            vt_switch(Keysym::from(keysyms::KEY_XF86Switch_VT_12)),
            Some(Action::SwitchVt(12))
        );
        assert_eq!(vt_switch(Keysym::from(keysyms::KEY_F1)), None);
    }

    #[test]
    fn escape_hatch_ignores_mod_key_and_config() {
        let m = mods(false, true, true, false);
        let off = built("[shortcuts]\nclose = \"\"\n", ModKey::Super);
        assert_eq!(
            action_for(&off, &m, Keysym::from(keysyms::KEY_BackSpace)),
            Some(Action::Quit)
        );
        assert_eq!(
            at(ModKey::Super, &m, keysyms::KEY_BackSpace),
            Some(Action::Quit)
        );
    }

    #[test]
    fn combos_parse_with_aliases_and_any_case() {
        let want = parse_combo("Super+Shift+Q").unwrap();
        for text in [
            "super+shift+q",
            "Mod4 + SHIFT + Q",
            "Logo+Shift+q",
            "Mod+Shift+Q",
        ] {
            assert_eq!(parse_combo(text), Ok(want), "{text}");
        }
        assert_eq!(
            parse_combo("Control+Alt+Delete"),
            parse_combo("ctrl+alt+delete")
        );
        assert_eq!(parse_combo("Super+Enter"), parse_combo("Super+Return"));
        assert_eq!(parse_combo("Super+Esc"), parse_combo("Super+Escape"));
        assert!(parse_combo("Super+Nonsense").is_err());
        assert!(parse_combo("Hyper+Q").is_err());
        assert!(parse_combo("Super+").is_err());
        assert!(parse_combo("").is_err());
    }

    #[test]
    fn config_rebinds_lists_and_disables() {
        let s = built(
            "[shortcuts]\nterminal = \"Super+Return\"\nbrowser = [\"Super+F\", \"Ctrl+Alt+B\"]\nlock = []\nfiles = \"\"\nbogus = \"Super+Z\"\nclose = \"Super+Garbage\"\n",
            ModKey::Super,
        );
        let at = |m: &ModifiersState, key| action_for(&s, m, Keysym::from(key));
        let sup = mods(true, false, false, false);
        // Return was swap_master (declared later than terminal in the
        // table, so it wins the clash).
        assert_eq!(at(&sup, keysyms::KEY_Return), Some(Action::SwapMaster));
        assert_eq!(at(&sup, keysyms::KEY_q), None);
        assert_eq!(at(&sup, keysyms::KEY_f), Some(Action::SpawnBrowser));
        assert_eq!(
            at(&mods(false, true, true, false), keysyms::KEY_b),
            Some(Action::SpawnBrowser)
        );
        assert_eq!(at(&sup, keysyms::KEY_l), None);
        assert_eq!(at(&sup, keysyms::KEY_e), None);
        assert_eq!(at(&sup, keysyms::KEY_z), None);
        // A bad combo disables only itself.
        assert_eq!(at(&sup, keysyms::KEY_c), None);
    }

    #[test]
    fn custom_commands_win_clashes() {
        let s = built(
            "[shortcuts]\n[[shortcuts.custom]]\nkeys = \"Super+B\"\ncommand = \"firefox --private-window\"\n\
             [[shortcuts.custom]]\nkeys = [\"Super+Q\", \"Mod+Shift+Z\"]\ncommand = \"foot\"\n",
            ModKey::Alt,
        );
        let at = |m: &ModifiersState, key| action_for(&s, m, Keysym::from(key));
        let alt = mods(false, true, false, false);
        assert_eq!(at(&alt, keysyms::KEY_b), Some(Action::Custom(0)));
        assert_eq!(s.command(0), Some("firefox --private-window"));
        assert_eq!(at(&alt, keysyms::KEY_q), Some(Action::Custom(1)));
        assert_eq!(
            at(&mods(false, true, false, true), keysyms::KEY_z),
            Some(Action::Custom(1))
        );
        assert_eq!(s.command(1), Some("foot"));
    }

    #[test]
    fn every_default_is_bindable() {
        for (name, keys, _) in cnde_config::SHORTCUT_DEFAULTS {
            assert!(action_named(name).is_some(), "{name}");
            for key in *keys {
                assert!(parse_combo(key).is_ok(), "{name}: {key}");
            }
        }
    }
}
