# Known issues and polish backlog

Things we know about and chose to defer. Fix them in a polish pass; delete
entries when they're done.

## Bugs

- **Stale frame flash when dragging a window off fullscreen/maximized.**
  Rare (about 1 in 20 tries, seen with alacritty, real hardware). The window is
  shown squeezed to its new size until the client acks the configure *and*
  changes size, or 500 ms pass (`PendingSize` in `effects.rs`). Something still
  lets one full-size frame through. Suspects: a commit that changes size to an
  intermediate value; the rect briefly using the live geometry between
  `map_element` and the first render; multi-output render ordering.

## Rough edges

- **Maximized windows crossing monitors of different resolution** visibly
  change as the app redraws at the new size when a workspace slide settles.
  The squeeze (`PendingSize`) keeps it from overflowing, but a crossfade from
  a snapshot (like the maximize animation's) would hide the redraw entirely.

- **Overview polish:** windows closing while it's open animate at their
  unzoomed spot; windows on hidden workspaces don't update live; the search
  box is only on the main monitor; no app grid button yet; no hover
  highlight on windows.
- **Wallpaper changes** swap instantly instead of crossfading.

- **Screen capture:**
  - Any client can capture the screen (as with wlroots compositors); a
    permission prompt and a "screen is shared" indicator should come with
    our own portal.
  - A captured window on a hidden workspace doesn't get frame callbacks,
    so most apps stop drawing and the share freezes until it's shown.
  - Captures always report full damage, so encoders redo the whole frame.
  - The GPU (dma-buf) capture path hasn't been seen working yet; the shm
    path was checked end to end with a test client.
  - With GNOME running in another session on the same systemd user bus,
    start cnde under `dbus-run-session`, or apps get GNOME's portal.

- **Monitor names:** make and model are "Unknown"; `smithay-drm-extras`'s EDID
  feature needs an older libdisplay-info than Arch ships.
- **Multi-GPU laptops** (render on one GPU, display on another) aren't supported;
  only the primary GPU is driven.
- **Animated cursors** (e.g. "wait") only advance when something else redraws.
- **Popups of layer surfaces** (waybar tray menus) aren't constrained to the screen.
- **Cursor images** use buffer scale 1, so they're blurry on scaled outputs.
- **Frame pacing:** we render right at vblank rather than as late as is safe
  before the next one (ARCHITECTURE.md §4.2's "frame clock"). Fine at 165/180 Hz;
  revisit with Tracy data.
- **Config errors** are only logged; they should show as a notification once
  the shell exists.
