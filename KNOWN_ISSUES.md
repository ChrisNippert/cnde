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
- **Right-click menus and edit mode** haven't been tried with a real mouse
  yet (only rendered nested). Moving the dock by its grip outlines where it'll
  land, but the dock itself jumps there on release rather than sliding. A clock
  dragged in edit mode follows the pointer but only lands in thirds of the bar.
- **App grid:** fades in, but going back from it is instant (the shell's
  surface shrinks to the search box at once); the workspace thumbnails do
  animate both ways.
- **Settings app:** no resize cursors at the window edges (the compositor
  lacks cursor-shape-v1); no keyboard navigation between controls (Tab) and
  no AccessKit yet; the light style isn't drawn (the switch only picks the
  wallpaper); dock favorites and autostart aren't editable in the GUI yet;
  no "add picture" for wallpapers (needs a file chooser); thumbnails are
  decoded on every launch; a second launch opens a second window.
- **Screenshots:** no "saved" notification yet (notifications don't exist);
  a region can't span two monitors (it's cut to the one holding its
  centre); no option to include the pointer; pasting from the clipboard
  hasn't been tested with a real app yet.
- **Dock:** no name tooltips yet, no right-click menu (pin/unpin, quit),
  no drag to reorder; favorites are edited in the config. The intellihide
  and autohide options are accepted but act as "always". Edge docks are on
  the main monitor only.
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
