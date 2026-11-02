# Known issues

What we know is broken, missing or untested. Delete entries when they're
fixed; add one when you defer something.

## Beta caveats

- **Tested on one setup.** Daily use is on AMD (Mesa radeonsi) with a
  2560x1440 180 Hz and a 1920x1080 165 Hz monitor. Intel was last tried on
  an older build; **NVIDIA hasn't been tried at all**.
- **Multi-GPU laptops** (render on one GPU, display on another) aren't
  supported; only the primary GPU is driven.
- **No screen reader support.** cnde's own surfaces (bar, dock, Settings,
  Files) aren't exposed to AT-SPI yet. Zoom and high contrast don't exist.
- **No input methods** (text-input / input-method), so IMEs for Chinese,
  Japanese, Korean and other languages don't work in Wayland apps yet.
- **No primary selection** (middle-click paste).

## Security (from the November 2026 audit, still open)

The serious findings were fixed. These are lower risk and known:

- **Lock screen:** no brute-force lockout beyond pam_unix's delay (add
  `pam_faillock` to `/etc/pam.d/cnde-lock` if you want one). The locker is
  told "locked" after the first monitor shows a locked frame, so another
  monitor may show the desktop for a frame or two longer.
- **Native apps are trusted:** any unsandboxed app of yours can use the IPC
  socket (window list, lock, log out, activation tokens) and capture the
  screen. Sandboxed (Flatpak) apps can't.
- **X11:** the abstract X11 socket's access control is xwayland-satellite's
  and Xwayland's defaults; not yet checked on multi-user machines.
- **Polkit prompt** shows polkit's message and the user, not the action id
  or details, and doesn't check that the request came from polkitd.
- **Bluetooth pairing** accepts without showing the confirmation number
  (no protection against a nearby device impersonating the one you picked).
- **Limits not yet set** on tray items registered and on concurrent file
  pickers a sandboxed app can open.
- **install.sh** builds as root if run with sudo; build as yourself first and
  run `sudo sh packaging/install.sh --no-build`.

## Bugs

- **Stale frame flash when dragging a window off fullscreen/maximized.**
  Rare (about 1 in 20 tries, seen with alacritty). The window is shown
  squeezed to its new size until the client acks the configure *and*
  changes size, or 500 ms pass (`PendingSize` in `effects.rs`). Something
  still lets one full-size frame through.
- **Fullscreen X11 games may not get direct scanout**, so they're
  composited (a little more latency and GPU time). The compositor logs
  `direct scanout` per monitor when it changes; a Proton game was seen
  never reaching it.
- **Some Proton games unmap themselves when they lose focus** (Wine
  minimizes; xwayland-satellite has no iconify), so they drop off the dock
  until clicked back.

## Rough edges

- **Screen capture:**
  - Native (unsandboxed) apps can capture the screen without asking, as
    with wlroots compositors. Sandboxed apps can't and must go through the
    portal.
  - A captured window on a hidden workspace doesn't get frame callbacks, so
    most apps stop drawing and the share freezes until it's shown.
  - Captures always report full damage, so encoders redo the whole frame.
  - With GNOME running in another session on the same systemd user bus,
    start cnde under `dbus-run-session`, or apps get GNOME's portal.
- **Wi-Fi:** joining has been tried with NetworkManager. iwd and enterprise
  (802.1X) joining are only covered by tests of the settings sent. With iwd,
  enterprise networks must first be set up as root in /var/lib/iwd, and WEP
  isn't supported.
- **Maximized windows crossing monitors of different resolution** visibly
  redraw at the new size when a workspace slide settles. A crossfade from a
  snapshot would hide it.
- **Screenshots:** a region can't span two monitors (it's cut to the one
  holding its centre).
- **Monitor names:** make and model are "Unknown". `smithay-drm-extras`'s
  EDID feature needs an older libdisplay-info than Arch ships.
- **Animated cursors** (e.g. "wait") only advance when something else redraws.
- **Popups of layer surfaces** (waybar tray menus) aren't constrained to the screen.
- **Cursor images** use buffer scale 1, so they're blurry on scaled outputs.
- **Frame pacing:** we render right at vblank rather than as late as is safe
  before the next one (ARCHITECTURE.md §4.2's "frame clock"). Fine at
  165/180 Hz; worth revisiting for latency.
- **Vertically stacked monitors** get a fallback workspace mapping, not a
  designed one (ARCHITECTURE.md §5.2).
