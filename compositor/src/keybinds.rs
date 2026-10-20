//! Default key bindings (ARCHITECTURE.md §5.2). Hard-coded for now; they move
//! into the config crate later.

use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

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
    Switcher { back: bool },
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

/// The volume and brightness keys, with or without modifiers.
pub fn media_key(sym: Keysym) -> Option<cnde_ipc::MediaKey> {
    use cnde_ipc::MediaKey;
    Some(match sym.raw() {
        keysyms::KEY_XF86AudioRaiseVolume => MediaKey::VolumeUp,
        keysyms::KEY_XF86AudioLowerVolume => MediaKey::VolumeDown,
        keysyms::KEY_XF86AudioMute => MediaKey::Mute,
        keysyms::KEY_XF86AudioMicMute => MediaKey::MicMute,
        keysyms::KEY_XF86MonBrightnessUp => MediaKey::BrightnessUp,
        keysyms::KEY_XF86MonBrightnessDown => MediaKey::BrightnessDown,
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

pub fn action_for(mod_key: ModKey, mods: &ModifiersState, sym: Keysym) -> Option<Action> {
    let sym = sym.raw();

    // Escape hatch (ARCHITECTURE.md §10): hard-coded, never rebindable.
    if mods.ctrl && mods.alt && sym == keysyms::KEY_BackSpace {
        return Some(Action::Quit);
    }
    if mods.alt && !mods.ctrl && !mods.logo && sym == keysyms::KEY_F4 {
        return Some(Action::CloseWindow);
    }
    if sym == keysyms::KEY_Print && !mods.ctrl && !mods.alt {
        return Some(Action::Screenshot);
    }

    // Alt+Tab and Super+Tab, on real hardware and nested alike (nested the
    // mod key is Alt, which would otherwise be the only way to reach it).
    if matches!(sym, keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab)
        && (mods.alt || mods.logo)
        && !mods.ctrl
    {
        return Some(Action::Switcher {
            back: mods.shift || sym == keysyms::KEY_ISO_Left_Tab,
        });
    }

    // Workspaces: Super+Alt+Left/Right (+Shift carries the window). Nested,
    // the mod is Alt, so Alt+Ctrl stands in for Super+Alt.
    let workspace_mods = match mod_key {
        ModKey::Super => mods.logo && mods.alt && !mods.ctrl,
        ModKey::Alt => mods.alt && mods.ctrl && !mods.logo,
    };
    if workspace_mods {
        // Right goes to the next workspace; the caller flips it for
        // natural keys (content moves with the arrow, so Left reveals next).
        let step = match sym {
            keysyms::KEY_Left => -1,
            keysyms::KEY_Right => 1,
            _ => return None,
        };
        return Some(Action::Workspace {
            step,
            carry: mods.shift,
        });
    }

    // Super+Alt+… is reserved for workspaces, so the window bindings require
    // the *other* modifier to be up.
    let held = match mod_key {
        ModKey::Super => mods.logo && !mods.alt,
        ModKey::Alt => mods.alt && !mods.logo,
    };
    if !held || mods.ctrl {
        return None;
    }

    let action = match (mods.shift, sym) {
        (false, keysyms::KEY_q) => Action::SpawnTerminal,
        (false, keysyms::KEY_f) => Action::SpawnBrowser,
        (false, keysyms::KEY_c) => Action::CloseWindow,
        (false, keysyms::KEY_a) => Action::AppGrid,
        (false, keysyms::KEY_l) => Action::Lock,
        (false, keysyms::KEY_Up) => Action::Maximize,
        (false, keysyms::KEY_Down) => Action::Restore,
        (false, keysyms::KEY_Left) => Action::SnapLeft,
        (false, keysyms::KEY_Right) => Action::SnapRight,
        (false, keysyms::KEY_t) => Action::ToggleTiling,
        (false, keysyms::KEY_Return | keysyms::KEY_KP_Enter) => Action::SwapMaster,
        (false, keysyms::KEY_bracketleft) => Action::MasterRatio(-1),
        (false, keysyms::KEY_bracketright) => Action::MasterRatio(1),
        (true, keysyms::KEY_f) => Action::ToggleFullscreen,
        (true, keysyms::KEY_s) => Action::Screenshot,
        (true, keysyms::KEY_t) => Action::ToggleFloating,
        (true, keysyms::KEY_Left) => Action::MoveTile(Direction::Left),
        (true, keysyms::KEY_Right) => Action::MoveTile(Direction::Right),
        (true, keysyms::KEY_Up) => Action::MoveTile(Direction::Up),
        (true, keysyms::KEY_Down) => Action::MoveTile(Direction::Down),
        _ => return None,
    };
    Some(action)
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

    #[test]
    fn super_bindings() {
        let m = mods(true, false, false, false);
        assert_eq!(
            action_for(ModKey::Super, &m, Keysym::from(keysyms::KEY_q)),
            Some(Action::SpawnTerminal)
        );
        let shifted = mods(true, false, false, true);
        assert_eq!(
            action_for(ModKey::Super, &shifted, Keysym::from(keysyms::KEY_f)),
            Some(Action::ToggleFullscreen)
        );
    }

    #[test]
    fn super_alt_arrows_slide_workspaces() {
        let m = mods(true, true, false, false);
        assert_eq!(
            action_for(ModKey::Super, &m, Keysym::from(keysyms::KEY_Left)),
            Some(Action::Workspace {
                step: -1,
                carry: false
            })
        );
        let carry = mods(true, true, false, true);
        assert_eq!(
            action_for(ModKey::Super, &carry, Keysym::from(keysyms::KEY_Right)),
            Some(Action::Workspace {
                step: 1,
                carry: true
            })
        );
        // Other keys with Super+Alt don't fall through to window bindings.
        assert_eq!(
            action_for(ModKey::Super, &m, Keysym::from(keysyms::KEY_q)),
            None
        );
    }

    #[test]
    fn tab_opens_the_switcher() {
        let at = |mod_key, m: &ModifiersState, key| action_for(mod_key, m, Keysym::from(key));
        let alt = mods(false, true, false, false);
        assert_eq!(
            at(ModKey::Super, &alt, keysyms::KEY_Tab),
            Some(Action::Switcher { back: false })
        );
        assert_eq!(
            at(ModKey::Alt, &mods(false, true, false, true), keysyms::KEY_Tab),
            Some(Action::Switcher { back: true })
        );
        assert_eq!(
            at(ModKey::Super, &mods(true, false, false, false), keysyms::KEY_Tab),
            Some(Action::Switcher { back: false })
        );
        assert_eq!(at(ModKey::Super, &mods(false, false, false, false), keysyms::KEY_Tab), None);
    }

    #[test]
    fn tiling_bindings() {
        let m = mods(true, false, false, false);
        let at = |m: &ModifiersState, key| action_for(ModKey::Super, m, Keysym::from(key));
        let shifted = mods(true, false, false, true);
        assert_eq!(
            action_for(ModKey::Super, &shifted, Keysym::from(keysyms::KEY_Left)),
            Some(Action::MoveTile(Direction::Left))
        );
        assert_eq!(
            action_for(ModKey::Super, &shifted, Keysym::from(keysyms::KEY_Down)),
            Some(Action::MoveTile(Direction::Down))
        );
        assert_eq!(at(&m, keysyms::KEY_t), Some(Action::ToggleTiling));
        assert_eq!(at(&m, keysyms::KEY_Return), Some(Action::SwapMaster));
        assert_eq!(
            at(&m, keysyms::KEY_bracketleft),
            Some(Action::MasterRatio(-1))
        );
        assert_eq!(
            at(&m, keysyms::KEY_bracketright),
            Some(Action::MasterRatio(1))
        );
        let shifted = mods(true, false, false, true);
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
    fn escape_hatch_ignores_mod_key() {
        let m = mods(false, true, true, false);
        assert_eq!(
            action_for(ModKey::Super, &m, Keysym::from(keysyms::KEY_BackSpace)),
            Some(Action::Quit)
        );
    }
}
