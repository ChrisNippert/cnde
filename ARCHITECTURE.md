# Architecture

> Working name: **cnde** (placeholder until a real name is picked).
> This document is the source of truth for design decisions. Read it at the start
> of every coding session. If code and this doc disagree, fix one of them.

## 1. What we're building

A Wayland desktop environment that:

- **Feels like GNOME.** It's smooth, animations come from the compositor, and the defaults are minimal and look finished.
- **Weighs less than Xfce.** Target is **≤ 300 MB total idle RAM after login**.
- **Is extensible like KDE**, but only on request. Extensions cost nothing when none are installed.
- **Ships only what a desktop needs.** A settings app, a lock screen, a
  screen-share picker and a light file manager (which is also the file
  picker apps open). No terminal, text editor or software store; users bring
  their own, and every built-in piece can be swapped for another program.

Measured (November 2026, two monitors, idle): the compositor uses about
138 MB PSS (much of it the GL driver), the shell about 25 MB, and both close
to 0% CPU.

### Non-goals

- An X11 session. Wayland only. X11 *apps* still run through XWayland (see §7).
- A suite of bundled applications beyond the ones above.
- Supporting every niche Wayland protocol on day one.

## 2. Process model

Every resident process must justify its memory. The default session runs **two
long-lived processes**, plus helpers that start only when needed.

| Process | Lifetime | Job | RAM budget |
|---|---|---|---|
| `cnde-compositor` | whole session | Display server, window management, rendering, animations, input, lock enforcement | ≤ 120 MB |
| `cnde-shell` | whole session; auto-restarted | Panel, launcher, overview UI, notifications, OSDs, quick settings, polkit prompts | ≤ 80 MB |
| `cnde-lock` | while locked | Lock screen (ext-session-lock client) | small, exits on unlock |
| `cnde-settings` | on demand | Settings app | exits when closed |
| `cnde-files` | on demand | File manager; owns `org.freedesktop.FileManager1` while running | exits when closed |
| `cnde-files-portal` | started by D-Bus activation | FileChooser portal backend (opens `cnde-files --pick`) | small |
| `cnde-share-picker` | per screen-share request | Picks the monitor or window for xdg-desktop-portal-wlr | exits after picking |
| `xwayland-satellite` | started on first X11 client | XWayland bridge | only if X11 apps are used |

**Modular installs**

Only the compositor is required. The others ship as separate packages
(`packaging/arch/PKGBUILD` splits them) and everything copes with a part
being absent:

- `autostart.shell` picks the shell command: `cnde-shell` by default,
  another bar, or `""` for none.
- Without `cnde-settings`, the shell leaves out its Settings menu items and
  the quick-settings gear; `config.toml` still does everything.
- Without `cnde-lock`, Super+L and the quick-settings Lock button do
  nothing unless `lock.command` names another ext-session-lock locker
  (swaylock, ...). `cnde-lock` checks passwords through PAM: its own
  `/etc/pam.d/cnde-lock` if installed, else the `login` service; with
  neither, it refuses to lock rather than lock you out.
- Without `cnde-icons`, symbolic icons come from Adwaita (and an
  uninstalled build finds them in the source tree's `icons/`).

**Crash rules**

- If `cnde-shell` crashes, the compositor restarts it, and windows are unaffected.
- `cnde-lock` is separate from the shell so a shell crash can never unlock the
  screen. The ext-session-lock protocol already guarantees the compositor
  keeps the screen locked if the lock client dies, and we keep that behaviour:
  the compositor starts `lock.command` again after 2 s with no lock surface.
  A second locker is accepted only while none is alive, so locking again
  can't be used to take over and unlock.
- Only a crash in the compositor ends the session. Keep it boring and well tested.

## 3. Repository layout

One Cargo workspace:

```
cnde/
├── README.md
├── ARCHITECTURE.md
├── KNOWN_ISSUES.md
├── Cargo.toml              # workspace
├── compositor/             # cnde-compositor (smithay)
├── shell/                  # cnde-shell
├── lock/                   # cnde-lock
├── settings/               # cnde-settings
├── files/                  # cnde-files and cnde-files-portal
├── share-picker/           # cnde-share-picker
├── ui/                     # shared crate: our widget toolkit (see §5.1)
├── apps/                   # shared crate: .desktop entries, MIME defaults, launching
├── ipc/                    # shared crate: compositor <-> shell/settings protocol types
├── config/                 # shared crate: config schema, load/save, defaults, data paths
├── data/                   # session file, desktop entries, portal configs, PAM file
├── icons/                  # cnde's symbolic icon theme (and its generator)
├── tools/                  # development tools (tools/perf: performance sampling)
└── packaging/              # Arch split PKGBUILD; install.sh for other distros
```

## 4. Compositor

Built on **smithay**, with the **calloop** event loop and the **GLES renderer**.

References we read and borrow ideas from: smithay's `anvil` example and **niri**.
niri is GPL-3.0, so we either license compatibly or rewrite rather than copy (see §12).

### 4.1 Backends

- **winit**: nested in an existing desktop. Used for day-to-day development.
- **udev/DRM + libinput + libseat**: real hardware, launched from a TTY or a display manager.

Pick the backend automatically: use winit when `WAYLAND_DISPLAY` or `DISPLAY` is set, udev otherwise. A flag overrides it.

### 4.2 Rendering and frame pacing (the "feels like GNOME" part)

These are requirements, not polish:

1. **Render on demand.** Draw only when something changed (damage, an animation, a cursor move). Idle uses 0% CPU and GPU.
2. **Schedule by vblank.** Use DRM page-flip events and presentation timestamps to predict the next refresh. Start rendering as late as is safe before it. Mutter calls this its "frame clock". niri has an equivalent we can study.
3. **Damage tracking.** Redraw only changed regions, using smithay's `OutputDamageTracker`.
4. **Direct scanout.** Fullscreen clients (games, video) bypass composition when possible.
5. **Never block the main loop.** No synchronous disk, D-Bus or network calls on the compositor thread.
6. **Send frame callbacks and presentation feedback correctly**, so clients pace themselves to the display too.
7. **Measure everything.** Gate frame-time logging and Tracy profiling behind a feature flag from the start. Smoothness bugs get fixed with data, not vibes.

### 4.3 Animations

- All window, workspace and overview animations run **in the compositor**. Clients never animate their own open, close or move.
- Two primitives: **springs** (for interruptible motion like dragging, workspace swipes and the overview) and **eased timelines** (fades, open and close).
- Animations are **interruptible and retargetable**. Starting a new animation mid-flight picks up from the current position and velocity instead of jumping.
- Closing windows animate from a snapshot texture taken when the client unmaps.
- Durations and curves live in one module (`compositor/src/animation/presets.rs`) so the "feel" is tunable in one place.
- One global speed multiplier, including 0 for "reduce motion".

### 4.4 Window management

- **Floating by default.** Behaviour follows GNOME: edge-drag snapping (halves, maximize), double-click the titlebar to maximize, and Super+drag to move.
- **Optional tiling per workspace.** Each workspace has a layout mode: `floating` (default) or `tiling` (master-stack to start). Toggling it animates windows to their new positions.
- **Layout trait.** Floating and tiling both implement one `Layout` trait. Later, extensions (WASM, §9) can provide more layouts through the same trait.
- **Dynamic workspaces** per output, GNOME-style: there's always one empty workspace at the end.
- **Overview** (Super key): workspaces plus scaled window thumbnails, rendered by the compositor and animated with springs. The search box is drawn by the shell on a layer surface over it.
- **Decorations:** prefer client-side decorations (GTK and libadwaita apps draw their own). Draw minimal server-side titlebars for clients that ask for them (via `xdg-decoration`).

### 4.5 Wayland protocols

Core: `wl_compositor`, `wl_subcompositor`, `wl_shm`, `xdg_shell`, `wl_seat`, `wl_data_device`, `wl_output` plus `xdg_output`.

Implemented:
`linux-dmabuf`, `viewporter`, `fractional-scale`, `presentation-time`,
`xdg-decoration`, `xdg-dialog`, `wlr-layer-shell`, `ext-session-lock`,
`idle-notify`, `idle-inhibit`, `pointer-constraints`, `relative-pointer`,
`xdg-activation`, `ext-foreign-toplevel-list`, `ext-image-copy-capture`
(with `ext-image-capture-source`), `ext-workspace`, `cursor-shape`,
`security-context`.

Wanted, not yet implemented: `primary-selection`, `text-input` /
`input-method` (IMEs), `wlr-output-management`, `single-pixel-buffer`,
`tearing-control`.

**Sandboxed clients.** A client that connects through a
`security-context` socket (Flatpak) doesn't see the privileged globals:
layer-shell, session-lock, the image-capture and foreign-toplevel globals,
and ext-workspace (`State::client_is_unrestricted`).

Add others when a real app needs them, not speculatively.

**Screen capture** is `ext-image-copy-capture-v1` (with
`ext-image-capture-source-v1` and `ext-foreign-toplevel-list-v1` for picking
windows), not `wlr-screencopy`: it's the standardized successor, and
xdg-desktop-portal-wlr 0.8+, grim and OBS speak it. Apps reach it through
xdg-desktop-portal → xdg-desktop-portal-wlr → PipeWire (`data/cnde-portals.conf`).
The compositor exports its environment to the session bus at startup so
the portals, started on demand by D-Bus, can find it, and restarts the
portal systemd user services so ones left over from the last session don't
keep a dead display. Which monitor to share is chosen in `cnde-share-picker`
(its own small binary), which portal-wlr runs as its `simple` chooser via a
`cnde` config the compositor writes if the user has none (`[portal]
share_picker` in config.toml).

## 5. Shell (`cnde-shell`)

One process that draws all persistent desktop UI as **layer-shell surfaces**:

- Top panel: clock, workspace indicator, status icons (via StatusNotifierItem), quick-settings button
- Launcher / search (opened from the overview or Super)
- Notification server (`org.freedesktop.Notifications`) and its popups
- OSDs: volume, brightness, keyboard layout
- Quick settings: network, Bluetooth, audio, power, dark mode, do-not-disturb
- Polkit authentication agent

It talks to system services directly over D-Bus: NetworkManager, BlueZ,
UPower, logind, and PipeWire through `wireplumber` or `pactl`-equivalent APIs. No `gsd-*`-style helper daemons.

### 5.1 UI toolkit (`ui` crate)

We write our own small toolkit, used by the shell, the lock screen and the settings app,
so all three look like one product.

- **Look:** modelled on libadwaita. Flat palette, 6/12 px corner radii, soft
  shadows, Adwaita Sans font, Adwaita icon theme. All visual values are
  **theme tokens** (colors, radii, spacing, font sizes), never hard-coded, so
  user theming is just overriding tokens.
- **Rendering:** CPU drawing with `tiny-skia` into `wl_shm` buffers. No GPU
  context per process. That's fine because the shell mostly draws still frames.
  Motion (popups opening, panels sliding) is animated by the **compositor**,
  not the client.
- **Text:** `cosmic-text` for shaping, layout and editing, plus `text-input-v3`
  for input methods.
- **Accessibility:** `AccessKit` (AT-SPI on Linux) is the plan, but it is
  **not wired in yet**: no cnde surface is visible to a screen reader today.
  The longer this waits, the harder it gets.
- **Widget set:** only what we need, grown on demand. Label, button, toggle,
  slider, list row, text entry, scroll view, popover, icon.
- **Model:** immediate mode. A client repaints its whole surface when something
  changes; each widget call (`ui::widgets::Painter`) draws one control and
  returns its rect, which the client keeps for hit-testing until the next
  paint. Our surfaces are small and change rarely, so this is cheap.
  Keep it simple, and don't build a general-purpose toolkit.

### 5.2 Core features (user requirements)

These are the must-haves the default experience is built around.
Every default listed here can be rebound or reconfigured.

**Overview and navigation**

| Feature | Default | Owner | Notes |
|---|---|---|---|
| **Super key** | Tap Super → overview with open windows; typing immediately searches | compositor + shell | Opens on Super *release*, and only if no other key was pressed, so Super+X combos never flash the overview. Keystrokes go straight to the shell's search box. |
| **Hot corner** | Top-left → overview | compositor | Pressure threshold so a fast slam triggers and a brush-past doesn't. Any corner can map to any action (KDE-style). |
| **App grid** | Super+A, or a button in the overview | shell | |
| **Dock / dash** | **Off** by default; optional | shell | Favorites and running apps. Can sit in the overview only, or always visible. |
| **Alt+Tab** | Grouped by app; Alt+\` cycles windows of the same app | compositor + shell | |
| **Touchpad gestures** | 3-finger left/right → workspace, 3-finger up → overview | compositor | Must track the fingers 1:1 and settle with release velocity (springs, §4.3). This is the top priority for feel. |

**Default keybinds** (all rebindable)

| Keys | Action |
|---|---|
| Super+Up / Down | Maximize / restore |
| Super+Left / Right | Snap to left / right half (straight across if already snapped to the other side) |
| Super+Shift+F | Toggle fullscreen |
| Super+T | Switch the current workspace between floating and tiling (`[tiling]`) |
| Super+Shift+T | Float the focused tile above the others, or tile it again |
| Super+Enter | Swap the focused tile with the main one |
| Super+[ / ] | Narrow / widen the main tile on the current workspace |
| Super+Alt+Left / Right | Previous / next workspace (`natural_workspace_keys` swaps them; see Workspaces) |
| Super+Alt+Shift+Left / Right | Same, carrying the focused window along |
| Super+Shift+S | Screenshot UI |
| Super+Q | Launch default terminal (`$TERMINAL`, then `xdg-terminal-exec`, then a fallback list) |
| Super+F | Launch default browser (`xdg-settings get default-web-browser`) |
| Super+C | Close window |
| Alt+F4 | Close window (backup) |
| Ctrl+Alt+Backspace | Kill compositor (hard-coded, §10) |

**Workspaces**

- Dynamic (one empty workspace is always available at the end).
- **The workspace strip.** There is one global, ordered row of workspaces.
  Monitors are viewports onto *consecutive* workspaces, in their physical
  left-to-right order. With a main monitor and one to its right:

  ```
  [1] [2] [3] [4]          [1] [2] [3] [4]
   ↑   ↑        Super+Alt+Right   ↑   ↑
  main right       ───────►      main right
  ```

  - Super+Alt+Left/Right slides the whole strip by one workspace, on all
    monitors at once, animated as one continuous motion.
  - **Direction = the workspace you go to.** Right goes to the next workspace,
    as in GNOME. `[input.keyboard] natural_workspace_keys = true` makes the
    arrow the content's motion instead (Left reveals the next workspace),
    matching the 3-finger swipe, like natural scrolling.
  - **Gestures are "natural".** Content follows the fingers 1:1, like
    natural touchpad scrolling. Swiping fingers right-to-left reveals the next
    workspace. This holds regardless of the scroll-direction setting (as in
    GNOME), with a config-only option to invert it.
  - Single monitor: identical to normal sequential workspaces.
  - The *main* monitor (configurable, default primary) is the anchor. It's the
    one the "current workspace" refers to, for the panel indicator and new windows.
  - **Vertically stacked monitors: undefined for now.** No good model exists
    yet. A 2D workspace grid with all monitors moving together is the leading
    candidate if this is ever revisited. Until then, use a safe fallback, not a
    designed feature: monitors map to workspaces in reading order
    (left-to-right, then top-to-bottom), and only Left/Right do anything. It
    must never crash or lose windows. It just doesn't have to feel great.
  - Unplugging a monitor just narrows the viewport. Workspaces never move or
    merge, so nothing gets lost.

**Multi-monitor**

- **Mixed refresh rates:** every output has its **own frame clock** and is paced
  independently. A 144 Hz and a 60 Hz monitor never slow each other down.
- **Per-output fractional scaling** (`wp-fractional-scale`) with crisp rendering.
  Windows moved between monitors of different scale re-render at the new scale.
- **Free arrangement** with arbitrary offsets (monitors of different heights and
  positions). The cursor crosses edges only where monitors actually touch, and
  is clamped elsewhere.
- Remember the layout per set of connected monitors and restore it on hotplug.
- Optional VRR/adaptive sync per output.

**Look**

| Feature | Default | Owner | Notes |
|---|---|---|---|
| **Wallpaper** | Ships a default; separate light and dark variants | compositor | Drawn by the compositor, not a separate process. A blurred copy is cached for panels and the overview. |
| **Light / dark theme** | Follows user choice; optional sunset schedule | shell + portal | Exposed via `org.freedesktop.appearance color-scheme`. |
| **Translucency / blur** | "Wallpaper only" | compositor | Levels: *Off* / *Wallpaper only* (cached, ~free) / *Full* (live dual-Kawase blur of windows behind, which costs GPU per frame). Shell surfaces opt in; apps can request it via `ext-background-effect`. Must hold frame rate on UHD 620. |
| **Panel placement** | Primary monitor | shell | Option: primary only / all monitors / chosen list. One layer surface per output. |
| **Night light** | Off; schedule optional | compositor | Color temperature via the DRM gamma LUT / CTM. No extra process. |
| **Shake to find cursor** | Off; optional | compositor | Rapid back-and-forth motion temporarily enlarges the cursor (macOS-style). |

**Quick settings and status (shell)**

- Bluetooth (BlueZ), volume and output device (PipeWire), brightness (backlight
  via logind or elogind `SetBrightness`, so no setuid helper), power mode
  (`power-profiles-daemon` or `tuned-ppd`, hidden if absent), dark mode
  toggle, do-not-disturb.
- **Battery** percentage, time remaining and low-battery warning (UPower).
- **Look:** GNOME's restraint, iOS Control Center's visuals. A translucent
  (later frosted) card, big rounded tile groups, circular toggles that fill
  with the accent colour, and **fat sliders** with the icon inside. A small
  arrow on a tile expands it in place (Wi-Fi networks, audio devices).
- **Audio output switching:** expanding the volume slider lists output
  devices (speakers, headphones, DACs) to switch between.
- **Media controls** (MPRIS) in the notification area.
- **Privacy indicators:** a small, minimal icon in the panel while the mic,
  camera, or screen capture is active. Sources: PipeWire stream state and our
  own portal or screencopy sessions.
- **Network:** a Wi-Fi tile in quick settings and a Wi-Fi page in Settings.
  They talk to a `NetworkBackend` trait with one implementation per daemon:
  **NetworkManager** (desktop) and **iwd** (laptop; with
  `EnableNetworkConfiguration=true` so it handles DHCP itself). NM can itself
  run on iwd, so these two cover nearly everything. ConnMan and raw
  wpa_supplicant aren't planned. At startup the shell detects which daemon
  is on D-Bus and uses it. With none present, the network tile is hidden.

**Screenshots** (compositor + shell)

- Super+Shift+S opens a GNOME-style UI: **region**, **screen** or **window**.
  Copies to the clipboard and saves to `~/Pictures/Screenshots`.
- Captured by the compositor (exact pixels, no portal round-trip). The shell
  draws the selection UI.
- **No screen recording.** Users use OBS etc. via the ScreenCast portal (§7).

**Laptop and power**

- Idle: dim → lock → screen off → suspend, with configurable timeouts.
  Inhibited by `idle-inhibit` (video players) and the logind inhibitor.
- Lid close: suspend, or do nothing if an external monitor is connected
  (configurable).
- Low-battery actions.

**Lock screen**

- `cnde-lock` (ext-session-lock), authenticating via **PAM**.
- Shows time and (optionally) notification counts.
- Fingerprint: **not v1**. Because auth goes through PAM, fprintd can plug in
  later. Keep the lock UI able to show "touch the sensor" prompts from PAM
  conversation messages.

**Removable media**

- Notification when a USB drive is inserted (UDisks2), with "Open" (default
  file manager) and "Eject" actions. Optional auto-mount.

**Input**

- Keyboard layouts via xkb, with Super+Space to switch when more than one is configured.
  Low priority, but present.

### 5.3 Panel design: GNOME by default, KDE on request

- **Default (GNOME-like):** one thin top bar.
  - Left: workspace indicator (one dot or pill per workspace, the active one highlighted).
  - Centre: clock. Clicking it opens the calendar and notifications.
  - Right: status icons (network, volume, battery). Clicking them opens quick settings.
- **Opt-in (KDE-like), via settings:**
  - App buttons / taskbar for open windows, and pinned apps.
  - An app-menu button.
  - Bar at the bottom instead of the top, or a bar on every monitor (§5.2).
  - Extra applets (later, via extensions §9).
- These are **options on one panel**, not separate modes or layouts to switch between.
- **Right-click menus** (`shell/src/menu.rs`, xdg_popups with a grab): dock icons
  (windows, new window, pin, quit), the dock and the top bar (their common options),
  and the wallpaper. The wallpaper isn't a client surface, so the compositor reports
  right-clicks on bare desktop (`Event::DesktopMenu`) and the shell hangs the menu
  off a 1x1 layer surface placed there.
- **Edit mode** (from those menus, `shell/src/edit_mode.rs`): the dock and bar edit
  in place, writing config.toml as you go. Dock: drag pinned icons to reorder, × to
  unpin, drag the move grip to a screen edge, −/+ or the wheel for icon size. Bar:
  drag the bottom edge for height, drag the clock to the left, centre or right.

### 5.4 Explicitly not in the beta

- Accessibility *features* (zoom, high contrast, screen reader workflows),
  and AccessKit itself (§5.1).
- Fingerprint unlock (see the lock screen above).
- Screen recording (use OBS through the ScreenCast portal).
- Clipboard history.

## 6. Compositor ↔ shell IPC

- **Standard protocols first.** The shell uses layer-shell, foreign-toplevel and
  **ext-workspace** like any other bar would. Anything cnde-shell can show,
  third-party bars (waybar's `ext/workspaces`, etc.) can show too. cnde-shell is
  the default that ships with the desktop, not a requirement.
- **Private IPC for the rest.** This covers overview open/close (in use now), and later "open launcher" keybind events and config reload. It runs over a Unix socket at `$XDG_RUNTIME_DIR/cnde-<pid>.sock`, exported to every client as `$CNDE_SOCKET`. Messages are one JSON object per line, defined in the `ipc` crate (`cnde-ipc`). Both sides ignore messages they don't know, so either end can be newer.
- The same socket backs a `cnde msg ...` CLI for scripting, which doubles as the first extension surface.

## 7. Other system integration

- **XWayland:** run `xwayland-satellite` on demand, as niri does. This keeps X11 code out of the compositor.
- **Portals** (`data/cnde-portals.conf`): Screenshot and ScreenCast go to
  `xdg-desktop-portal-wlr`, which captures through `ext-image-copy-capture`
  and asks `cnde-share-picker` what to share. FileChooser goes to
  `cnde-files-portal`, falling back to GTK. Everything else (Settings,
  appearance) goes to `xdg-desktop-portal-gtk`.
- **Appearance:** publish `org.freedesktop.appearance` (`color-scheme`, `accent-color`) through the portal Settings interface, which libadwaita and Qt 6 apps follow. Set GTK and Qt themes and fonts so apps look consistent out of the box.
- **Session startup: no hard systemd dependency.** It must run on Alpine (OpenRC,
  musl, seatd or elogind) as well as systemd distros.
  - The compositor itself launches and supervises `cnde-shell` (restart on
    crash, with backoff) and starts the user's autostart entries.
  - Seat access goes through `libseat`, which works with seatd, elogind or logind.
  - When systemd is present, we additionally import the environment into it
    and D-Bus so activated services see `WAYLAND_DISPLAY`.
  - A `.desktop` session file is provided for display managers, and launching
    `cnde` from a TTY also works.
- **Secrets:** use the user's existing Secret Service provider (gnome-keyring or KeePassXC). We don't ship one.

## 8. Configuration

- One user config file at `~/.config/cnde/config.toml` (TOML, via `serde` + `toml`).
- The `config` crate owns the schema, defaults and validation. The compositor, shell and settings app all use it.
- The compositor watches the file and **hot-reloads** it. An invalid config keeps the last good one and shows an error notification.
- The settings app is a GUI over this same file, so hand-editing and the GUI never disagree.

### 8.2 The settings app (`cnde-settings`)

- A normal xdg-toplevel window with its own header bars and a page sidebar,
  libadwaita style. `cnde-settings <page>` opens on a page ("displays", "mouse", ...).
  The shell lists it in the app grid and search (from next to its own binary
  when it isn't installed) and opens it from the gear in quick settings.
- **Writing:** `config::ConfigFile` edits the file with `toml_edit`, so comments
  and layout survive. Every change is parsed as a `Config` first; an invalid one
  isn't written and the error shows in the window. Saves are a temp file renamed
  over the original (a symlinked config is followed), so watchers never see half
  a file. The compositor and shell apply it through their normal hot reload.
- **Reading:** it watches the file too, so hand edits show up while it's open.
- **Pages** are data (`settings/src/pages.rs`): rows of (config key, control,
  current value, default), rebuilt from the config each paint. A row that
  differs from its default gets a reset button that removes the key. Advanced
  rows (tier 2 in §8.1) show when that page's "Advanced settings" switch is on.

### 8.1 Configurability: GNOME defaults, KDE depth

- **Defaults are the product.** Out of the box it should look finished and
  minimal, with nothing to tweak.
- **Three tiers.** Every option lives in exactly one:
  1. *Basic*: shown on settings pages by default (wallpaper, dark mode, accent color).
  2. *Advanced*: in a collapsed "Advanced" section on the same page
     (corner radius, panel position, animation speed, focus behaviour).
  3. *Config-only*: rare or power-user options, documented but not in the GUI.
- **Theming** is overriding `ui` theme tokens (§5.1) and compositor values
  (border width, shadows, animation curves). It is never CSS-style patching of internals.
- New options need a tier and a sensible default before they're merged.

## 9. Extensions (post-v1)

Design goal: **zero cost when unused**, with a **stable API** that doesn't break every release.

| Extension kind | Mechanism |
|---|---|
| Panel applets, launcher search providers | Separate processes over the IPC socket (or D-Bus for search providers) |
| Keybind actions, automation | The IPC socket / `cnde msg` CLI |
| Window rules | Config file |
| Tiling layouts, compositor effects | **WASM plugins** (wasmtime), loaded only if configured, sandboxed, with a versioned ABI |

No native `.so` plugins, no monkey-patching, and no always-on scripting runtime.

## 10. Development workflow

- **Daily:** `cargo run -p cnde-compositor` runs nested in the current session via winit.
  `-c <command>` launches a client at startup. **Nested, every Super binding
  uses Alt instead**, because the host desktop grabs Super.
- **Real hardware:** run from a spare TTY (Ctrl+Alt+F3). Have SSH enabled so a frozen session can be killed remotely.
- **Escape hatch:** a hard-coded `Ctrl+Alt+Backspace` quits the compositor. It's not configurable, and nothing can block it.
- **VM:** only for testing the display-manager login flow and packaging on a clean system. Never for judging smoothness.
- **Logging:** `tracing`, with `RUST_LOG` filters and frame-timing spans.
- Target hardware during development is AMD (RX 6700-class, Mesa radeonsi). Test on Intel and NVIDIA before calling anything stable.
- **Second test machine:** an Alpine Linux laptop (i7-8665U, Intel UHD 620 on
  Mesa's `iris` driver; musl, OpenRC, no DE). This keeps us honest about systemd
  and glibc assumptions. It's the reference for low-end GPU smoothness and for
  laptop behaviour: battery, suspend and lid, touchpad gestures, backlight,
  external monitors.

## 11. Milestones

Each milestone ends in something you can use. Milestones 1–6 are done; cnde
is in **public beta** (November 2026).

1. **Nested compositor:** a winit window that shows a terminal, with floating window move/resize, focus, a keyboard layout, and basic open/close animations.
2. **Real hardware:** udev backend, multi-monitor, proper frame pacing, damage tracking, cursor, `wlr-layer-shell`.
3. **Shell v1:** panel, launcher, notifications, OSDs, a lock screen.
4. **Daily driver:** XWayland, portals (screenshot and screencast), session files, idle and suspend, fractional scaling.
5. **Workspaces, overview and tiling mode.**
6. **Settings app** (and, beyond the original plan, cnde-files and the share picker).
7. **Public beta:** security audit, README, testing on other people's hardware. *In progress.*
8. **Extension API** (IPC applets first, then WASM).

## 12. Conventions (for humans and AI sessions)

- Rust stable, edition 2024. Run `cargo fmt` and `cargo clippy -D warnings` before every commit.
- No `unwrap()` in the compositor outside tests and startup. A misbehaving client must never crash the session. Disconnect the client instead.
- Protocol handlers live in `compositor/src/handlers/`; a protocol with
  real logic of its own gets its own module (`ext_workspace.rs`, `capture.rs`).
- Aim for modules under ~800 lines. Several are past that (`layout.rs`,
  `switcher.rs`, `workspaces.rs`); split them when next touched.
- **How it's built.** The code is written by Claude (Anthropic) through
  Claude Code, directed and tested on real hardware by the owner (see
  README). Sessions read this file first.
- Every non-obvious decision gets a line in this doc or a `// Why:` comment.
- **License: GPL-3.0-or-later.** Chosen so we can adapt code from niri.
- **Track where code came from.** Any code adapted from niri gets a comment on
  the item: `// Adapted from niri (GPL-3.0-or-later): <path in niri repo>`.
  This keeps relicensing possible later. Remove every tagged item, and future
  versions can change license, as long as all other contributors agree. Code
  adapted from smithay or anvil (MIT) gets `// Adapted from smithay: <path>`.
