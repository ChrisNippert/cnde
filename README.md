# cnde

*A light Wayland desktop with an adaptive, smooth design. Modular and
compact, but it should never feel cheap.*

> **Public beta.** cnde is one person's daily desktop, on one AMD machine
> with two monitors. It works well there; it hasn't met your hardware yet.
> Expect rough edges, and please [report them](#reporting-bugs).
> "cnde" is a working name.

<!-- Screenshots: desktop, overview, quick settings, files, settings -->

## What it's for

- **Smooth.** Animations run in the compositor at your monitor's refresh
  rate, can be interrupted midway, and always end softly. Every monitor keeps
  its own frame clock, so a 180 Hz and a 60 Hz screen never slow each other down.
- **Light.** At idle on two monitors, the compositor and the shell together use
  about 165 MB of memory and close to 0% CPU. Nothing redraws unless something
  changed.
- **Adaptive.** Colours can follow the wallpaper and the windows in front of
  them. Panels are frosted glass while they float and turn solid when
  something touches them.
- **Modular.** Each built-in piece (top bar, dock, lock screen, notifications,
  file manager, screen-share picker) can be swapped for another program
  through the config: waybar, swaylock, mako, fuzzel, nautilus and so on. When
  you swap one, cnde gets out of its way.
- **Few options, the right ones.** Good defaults first. Every option is in
  Settings in plain words and is also one line in a config file.
- **Games just work.** A fullscreen game gets its monitor to itself, the
  mouse stays inside it when it asks, and nothing pops over it.

## Features

**Windows and workspaces**
- Floating by default, with snapping to the edges. Two windows snapped side by
  side resize together.
- Optional tiling for every workspace or just one (dwindle or master-stack),
  toggled with Super+T.
- Dynamic workspaces as one strip that spans your monitors, or a separate set
  per monitor.
- Overview (tap Super) with live window thumbnails, and Alt+Tab grouped by app.
- Optional cnde title bars for apps that allow them; apps that draw their
  own keep theirs.

**Shell**
- Top bar with workspaces, clock, tray and status icons.
- Dock: in the overview only (the default), or at a screen edge, with window
  previews on hover.
- App grid and search.
- Quick settings: volume and output device, brightness, Bluetooth, Wi-Fi,
  power mode, dark mode, do not disturb.
- Notifications with a notification centre in the calendar, plus media controls.
- On-screen displays for volume and brightness.
- A polkit password prompt.
- A screenshot tool (Super+Shift+S): region, window or screen, saved to a
  file and copied to the clipboard.

**Apps**
- **Settings:** displays, appearance, dock, top bar, windows, keyboard, mouse
  and touchpad, sound (per-app volume), Bluetooth, Wi-Fi, power, default
  apps, accessibility.
- **Files:** a light file manager (cnde-files). It's also the file picker
  apps open, through the xdg-desktop-portal.
- **Lock screen:** cnde-lock, using PAM.
- **Screen-share picker:** chooses which monitor or window an app shares.

**Under the hood**
- Built on smithay; Rust throughout.
- X11 apps and Proton games run through xwayland-satellite, started on demand.
- Direct scanout for fullscreen apps, recovery from GPU resets, and VRR.
- Night light, dark-mode schedule, idle dim, screen-off, lock and suspend.
- Sandboxed apps (Flatpak) can't capture the screen, list your windows,
  draw panels or lock the session.
- Runs without systemd (seatd or elogind) as well as with it.

## What it isn't

- Not an X11 session. X11 *apps* work; the session itself is Wayland only.
- Not a full app suite. It ships a file manager, settings and a lock
  screen; you bring the terminal, browser and editor.
- Not tested on NVIDIA or Intel graphics yet, and it has no screen reader
  support yet.

## Install

**Arch Linux**

```sh
git clone <repo-url> cnde
cd cnde/packaging/arch
makepkg -si
```

Then pick **cnde** at your login screen. The PKGBUILD splits the desktop into
packages (compositor, shell, lock, settings, files, icons), so you can leave
pieces out.

**Other distributions**

`packaging/install.sh` builds and installs to `/usr/local` (or any
`PREFIX`) and can uninstall cleanly. See
[packaging/README.md](packaging/README.md) for the build and runtime
dependencies on Debian, Ubuntu and Fedora.

**Recommended extras:** `xwayland-satellite` and Xwayland (X11 apps and most
games), `xdg-desktop-portal-wlr` (screen sharing), `xdg-desktop-portal-gtk`,
PipeWire with WirePlumber, polkit, udisks2.

## Using it

| Keys | Action |
|---|---|
| Super (tap) | Overview; start typing to search |
| Super+A | App grid |
| Super+Q / Super+F | Terminal / browser |
| Super+C, Alt+F4 | Close window |
| Super+Left/Right/Up | Snap left or right / maximize |
| Super+T | Floating or tiling |
| Super+Alt+Left/Right | Previous / next workspace |
| Super+Shift+S | Screenshot |
| Super+L | Lock |
| Ctrl+Alt+Backspace | Quit the session (always works) |

Everything is in **Settings**, or in `~/.config/cnde/config.toml`. The
config file is reloaded as soon as you save it, and Settings edits the same
file without disturbing your comments. The defaults, with an explanation of
every option, are in
[config/src/default-config.toml](config/src/default-config.toml).

## How this was built

cnde's code was written by Claude (Anthropic's AI model) through Claude
Code, directed by me. I set the design and priorities, use it as my daily
desktop, and test every change on real hardware. Claude implemented,
debugged and profiled it. Bug reports are reviewed, and fixes are tested,
the same way.

## Reporting bugs

Please include:

- distro, GPU and driver, and your monitors (resolution, refresh, scale);
- what you did and what happened;
- the compositor's log from around that time:

  ```sh
  journalctl -b _COMM=cnde-compositor
  ```

  (Started from a TTY instead of a login screen, the log is wherever its
  stderr went.)

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): how the code is organised and why.
- [KNOWN_ISSUES.md](KNOWN_ISSUES.md): what's known to be broken or missing.
- [packaging/README.md](packaging/README.md): installing without a package
  manager.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
