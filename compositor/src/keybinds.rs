//! Default key bindings (ARCHITECTURE.md §5.2). Hard-coded for now; they move
//! into the config crate later.

use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

/// The modifier our bindings use. Super on real hardware; Alt when nested,
/// because the host desktop grabs Super for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKey {
    #[allow(dead_code)] // used by the udev backend in Milestone 2
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
        (false, keysyms::KEY_Up) => Action::Maximize,
        (false, keysyms::KEY_Down) => Action::Restore,
        (false, keysyms::KEY_Left) => Action::SnapLeft,
        (false, keysyms::KEY_Right) => Action::SnapRight,
        (true, keysyms::KEY_f) => Action::ToggleFullscreen,
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
    fn super_alt_is_reserved_for_workspaces() {
        let m = mods(true, true, false, false);
        assert_eq!(
            action_for(ModKey::Super, &m, Keysym::from(keysyms::KEY_Left)),
            None
        );
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
