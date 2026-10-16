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
