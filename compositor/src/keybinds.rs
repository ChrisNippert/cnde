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
        (true, keysyms::KEY_f) => Action::ToggleFullscreen,
        (true, keysyms::KEY_s) => Action::Screenshot,
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
