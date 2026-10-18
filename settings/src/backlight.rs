//! The laptop panel's backlight (`/sys/class/backlight`), the same way the
//! shell's quick settings reads and sets it. No change notification: it's
//! read when the window opens, when Displays is shown, and after we set it.

use std::path::PathBuf;

struct Backlight {
    path: PathBuf,
    max: u32,
    current: u32,
}

/// The GPU's own backlight in preference to firmware ones.
fn find() -> Option<Backlight> {
    let mut devices: Vec<PathBuf> = std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    let kind = |path: &std::path::Path| {
        std::fs::read_to_string(path.join("type"))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    devices.sort_by_key(|p| match kind(p).as_str() {
        "raw" => 0,
        "platform" => 1,
        _ => 2,
    });
    let path = devices.into_iter().next()?;
    let number = |file: &str| -> Option<u32> {
        std::fs::read_to_string(path.join(file))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    Some(Backlight {
        max: number("max_brightness")?,
        current: number("brightness")?,
        path,
    })
}

/// Percent of the panel's maximum, or `None` with no backlight (desktops).
pub fn read() -> Option<u32> {
    let b = find()?;
    Some(((b.current as f64 / b.max.max(1) as f64) * 100.0).round() as u32)
}

/// Write the sysfs file directly (the video group can on cnde's laptop
/// setup), else ask logind, which allows it for the active session.
pub fn set(percent: u32) -> Result<(), String> {
    let b = find().ok_or("no backlight")?;
    // Never all the way to black: a 0% panel looks like a dead screen.
    let raw = ((percent.clamp(1, 100) as f64 / 100.0) * b.max as f64).round() as u32;
    let raw = raw.max(1);
    let direct = std::fs::write(b.path.join("brightness"), raw.to_string());
    if direct.is_ok() {
        return Ok(());
    }
    let device = b
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let conn = zbus::blocking::Connection::system().map_err(|err| err.to_string())?;
    conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1/session/auto",
        Some("org.freedesktop.login1.Session"),
        "SetBrightness",
        &("backlight", device.as_str(), raw),
    )
    .map(|_| ())
    .map_err(|err| format!("can't set the brightness ({err})"))
}
