//! Opening files with installed apps: launching on top of the shared
//! `cnde-apps` index. Until the index is built files go to `xdg-open`.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use cnde_apps::mime;
pub use cnde_apps::*;

/// Open `files` with `app`, detached, from `cwd`.
pub fn launch(app: &App, files: &[PathBuf], cwd: &Path) -> Result<(), String> {
    for argv in commands(app, files) {
        let argv = if app.terminal {
            let term = crate::ops::terminal_argv().ok_or("No terminal found (set $TERMINAL)")?;
            crate::ops::in_terminal(term, argv)
        } else {
            argv
        };
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(cwd);
        crate::ops::run_detached(cmd).map_err(|e| format!("Can't run {}: {e}", argv[0]))?;
    }
    Ok(())
}

/// Open files with their default apps (call on a thread); what has none
/// goes to `xdg-open`. Files of one app open together.
pub fn open_default(files: &[PathBuf]) -> Result<(), String> {
    let mut groups: Vec<(Option<usize>, Vec<PathBuf>)> = Vec::new();
    for f in files {
        let default = index().and_then(|ix| ix.default_for(&mime::of_path(f, false)));
        match groups
            .iter_mut()
            .find(|(a, _)| *a == default && default.is_some())
        {
            Some((_, list)) => list.push(f.clone()),
            None => groups.push((default, vec![f.clone()])),
        }
    }
    for (app, list) in groups {
        let cwd = list[0].parent().unwrap_or(Path::new("/")).to_path_buf();
        match (app, index()) {
            (Some(i), Some(ix)) => launch(&ix.apps[i], &list, &cwd)?,
            _ => {
                let mut cmd = Command::new("xdg-open");
                cmd.arg(&list[0]).current_dir(&cwd);
                crate::ops::run_detached(cmd).map_err(|e| format!("Can't run xdg-open: {e}"))?;
                for f in &list[1..] {
                    let mut cmd = Command::new("xdg-open");
                    cmd.arg(f).current_dir(&cwd);
                    crate::ops::run_detached(cmd)
                        .map_err(|e| format!("Can't run xdg-open: {e}"))?;
                }
            }
        }
    }
    Ok(())
}
