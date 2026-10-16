#!/usr/bin/env python3
# Regenerates icons/cnde/symbolic: python3 icons/make_icons.py icons/cnde
"""Writes cnde's symbolic icon set: 16x16, round-capped 1.75px outlines with
a few solid accents. Symbolic icons are drawn as masks, so only shape and
opacity matter (colour is the tint the shell chooses)."""
import os, sys

OUT = sys.argv[1]
SW = 1.75
DIM = 0.35


def svg(body):
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">\n'
        f'<g fill="none" stroke="#000" stroke-width="{SW}" stroke-linecap="round" stroke-linejoin="round">\n'
        f"{body}\n</g>\n</svg>\n"
    )


def fill(d, opacity=1.0):
    return f'<path d="{d}" fill="#000" stroke="none" opacity="{opacity}"/>'


icons = {}


def icon(context, name, body):
    icons[(context, name)] = svg(body)


# Window buttons
icon("actions", "window-close", '<path d="M4.75 4.75l6.5 6.5M11.25 4.75l-6.5 6.5"/>')
icon("actions", "window-maximize", '<rect x="3.75" y="3.75" width="8.5" height="8.5" rx="2.5"/>')
icon("actions", "window-restore",
     '<rect x="3" y="5.5" width="7.5" height="7.5" rx="2.25"/><path d="M5.75 3h5a2.25 2.25 0 0 1 2.25 2.25v5"/>')
icon("actions", "window-minimize", '<path d="M4.5 10.5h7"/>')

# Navigation and editing
icon("actions", "pan-up", '<path d="M4.5 10l3.5-3.5 3.5 3.5"/>')
icon("actions", "pan-down", '<path d="M4.5 6l3.5 3.5 3.5-3.5"/>')
icon("actions", "pan-start", '<path d="M10 4.5L6.5 8l3.5 3.5"/>')
icon("actions", "pan-end", '<path d="M6 4.5L9.5 8 6 11.5"/>')
icon("actions", "object-select", '<path d="M3.5 8.5l3 3 6-7"/>')
icon("actions", "edit-undo", '<path d="M5.5 3.5L2.75 6.25 5.5 9"/><path d="M3 6.25h6.25a3.75 3.75 0 0 1 0 7.5H7"/>')
icon("actions", "zoom-in", '<circle cx="7" cy="7" r="4.25"/><path d="M10.25 10.25l3 3M5 7h4M7 5v4"/>')
icon("actions", "zoom-out", '<circle cx="7" cy="7" r="4.25"/><path d="M10.25 10.25l3 3M5 7h4"/>')
icon("actions", "system-search", '<circle cx="7" cy="7" r="4.25"/><path d="M10.25 10.25l3 3"/>')
icon("actions", "media-playback-pause", '<path d="M5.5 4v8M10.5 4v8"/>')
icon("actions", "system-shutdown", '<path d="M8 2.5v5"/><path d="M4.6 4.6a5 5 0 1 0 6.8 0"/>')
icon("actions", "system-reboot", '<path d="M12.5 8a4.5 4.5 0 1 1-1.3-3.2"/><path d="M11.75 2v3h-3"/>')
# Four rounded tiles rather than GNOME's nine dots.
icon("actions", "view-app-grid",
     fill("M3 4.5A1.5 1.5 0 0 1 4.5 3h1A1.5 1.5 0 0 1 7 4.5v1A1.5 1.5 0 0 1 5.5 7h-1A1.5 1.5 0 0 1 3 5.5z"
          "M9 4.5A1.5 1.5 0 0 1 10.5 3h1A1.5 1.5 0 0 1 13 4.5v1A1.5 1.5 0 0 1 11.5 7h-1A1.5 1.5 0 0 1 9 5.5z"
          "M3 10.5A1.5 1.5 0 0 1 4.5 9h1A1.5 1.5 0 0 1 7 10.5v1A1.5 1.5 0 0 1 5.5 13h-1A1.5 1.5 0 0 1 3 11.5z"
          "M9 10.5A1.5 1.5 0 0 1 10.5 9h1A1.5 1.5 0 0 1 13 10.5v1A1.5 1.5 0 0 1 11.5 13h-1A1.5 1.5 0 0 1 9 11.5z"))
# Add a picture: a plus in a rounded square (an empty slot to fill).
icon("actions", "insert-image", '<rect x="2.5" y="2.5" width="11" height="11" rx="3"/><path d="M8 5.5v5M5.5 8h5"/>')
icon("actions", "list-add", '<path d="M8 3.5v9M3.5 8h9"/>')
icon("apps", "application-x-executable",
     '<rect x="2" y="2.5" width="12" height="11" rx="3"/><path d="M5 6.5l2 1.75-2 1.75M8.5 10.5h2.5"/>')

# Settings: sliders instead of a gear.
icon("categories", "emblem-system",
     '<path d="M2.5 4.5h4M9.5 4.5h4M2.5 11.5h7M12.5 11.5h1"/><circle cx="8" cy="4.5" r="1.5"/><circle cx="11" cy="11.5" r="1.5"/>')
icon("categories", "preferences-desktop-appearance",
     '<circle cx="8" cy="8" r="5.5"/>' + fill("M8 3.25a4.75 4.75 0 0 1 0 9.5z"))
icon("categories", "preferences-desktop-accessibility",
     fill("M8 1.75a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3z") + '<path d="M3.5 6.25h9M8 6.25v3.5M8 9.75l-2.25 3.75M8 9.75l2.25 3.75"/>')
icon("devices", "video-display", '<rect x="2" y="2.75" width="12" height="8.5" rx="2"/><path d="M6 13.5h4"/>')
icon("devices", "input-mouse", '<rect x="4.25" y="2" width="7.5" height="12" rx="3.75"/><path d="M8 4.75v1.75"/>')
icon("devices", "input-keyboard",
     '<rect x="1.75" y="4" width="12.5" height="8" rx="2.25"/><path d="M5.5 9.5h5"/>'
     + fill("M4.5 6.25h1v1h-1zM7.5 6.25h1v1h-1zM10.5 6.25h1v1h-1z"))
icon("categories", "focus-top-bar",
     '<rect x="2" y="2.75" width="12" height="10.5" rx="2.5"/>' + fill("M4.5 2.75h7A2.5 2.5 0 0 1 14 5.25V6H2v-.75a2.5 2.5 0 0 1 2.5-2.5z"))

# Status: volume
SPEAKER = fill("M2.25 6.25a1 1 0 0 1 1-1h1.75L8 2.75v10.5L5 10.75H3.25a1 1 0 0 1-1-1z")
WAVES = ['<path d="M10.25 6.25a2.5 2.5 0 0 1 0 3.5"/>', '<path d="M12 4.5a5 5 0 0 1 0 7"/>']
icon("status", "audio-volume-muted", SPEAKER + '<path d="M10.5 6.25l3.25 3.5M13.75 6.25l-3.25 3.5"/>')
icon("status", "audio-volume-low", SPEAKER + WAVES[0])
icon("status", "audio-volume-medium", SPEAKER + WAVES[0] + WAVES[1].replace("/>", f' opacity="{DIM}"/>'))
icon("status", "audio-volume-high", SPEAKER + WAVES[0] + WAVES[1])

# Status: wifi, as three arcs over a dot; weaker signals dim the outer arcs.
ARCS = [
    "M5.75 10a3.2 3.2 0 0 1 4.5 0",
    "M3.75 7.75a6 6 0 0 1 8.5 0",
    "M1.75 5.5a8.75 8.75 0 0 1 12.5 0",
]
DOT = fill("M8 11.25a1.25 1.25 0 1 1 0 2.5 1.25 1.25 0 0 1 0-2.5z")


def wifi(lit, slash=False, dot_dim=False):
    body = DOT if not dot_dim else DOT.replace('opacity="1.0"', f'opacity="{DIM}"')
    for i, d in enumerate(ARCS):
        opacity = 1.0 if i < lit else DIM
        body += f'<path d="{d}" opacity="{opacity}"/>'
    if slash:
        body += '<path d="M3 2.5l10 11"/>'
    return body


icon("status", "network-wireless-signal-excellent", wifi(3))
icon("status", "network-wireless", wifi(3))
icon("status", "network-wireless-signal-good", wifi(2))
icon("status", "network-wireless-signal-ok", wifi(1))
icon("status", "network-wireless-signal-weak", wifi(0))
icon("status", "network-wireless-signal-none", wifi(0, dot_dim=True))
icon("status", "network-wireless-acquiring", wifi(0, dot_dim=True))
icon("status", "network-wireless-offline", wifi(0, slash=True, dot_dim=True))
icon("status", "network-wireless-disabled", wifi(0, slash=True, dot_dim=True))
icon("status", "network-wired",
     '<rect x="5.5" y="2" width="5" height="4" rx="1.25"/><rect x="1.75" y="10" width="5" height="4" rx="1.25"/>'
     '<rect x="9.25" y="10" width="5" height="4" rx="1.25"/><path d="M8 6v2M4.25 10V8h7.5v2"/>')

# Status: battery. A rounded body with a filled level; charging swaps the
# level for a bolt.
BODY = '<rect x="1.5" y="4.5" width="11.5" height="7" rx="2.25"/>' + fill("M14 6.5h.25a.75.75 0 0 1 .75.75v1.5a.75.75 0 0 1-.75.75H14z")
BOLT = fill("M8.25 5.25L5.25 8.5h2.5l-.5 2.25 3-3.25h-2.5z")


def battery(level, charging=False):
    width = 7.5 * level / 100
    body = BODY
    if charging:
        if width > 0:
            body += f'<rect x="3.5" y="6.5" width="{width:.2f}" height="3" rx="0.75" fill="#000" stroke="none" opacity="{DIM}"/>'
        body += BOLT
    elif width > 0:
        body += f'<rect x="3.5" y="6.5" width="{width:.2f}" height="3" rx="0.75" fill="#000" stroke="none"/>'
    return body


for level in range(0, 101, 10):
    icon("status", f"battery-level-{level}", battery(level))
    icon("status", f"battery-level-{level}-charging", battery(level, charging=True))
icon("status", "battery-level-100-charged", battery(100, charging=True))

# A padlock: secured networks.
icon("status", "changes-prevent",
     '<rect x="3.5" y="7" width="9" height="7" rx="2"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/>')

# Quick settings
icon("status", "display-brightness",
     '<circle cx="8" cy="8" r="2.75"/><path d="M8 1.75v1M8 13.25v1M1.75 8h1M13.25 8h1M3.6 3.6l.7.7M11.7 11.7l.7.7M3.6 12.4l.7-.7M11.7 4.3l.7-.7"/>')
icon("status", "weather-clear-night", fill("M10.5 2.25A5.75 5.75 0 1 0 13.75 10 4.5 4.5 0 0 1 10.5 2.25z"))
icon("status", "bluetooth-disabled", '<path d="M4.5 5.25L11 10.5 8 13V3l3 2.5-6.5 5.25" opacity="0.5"/><path d="M2.5 2.5l11 11"/>')
icon("status", "notifications-disabled",
     '<path d="M4.25 11.25V7.5a3.75 3.75 0 0 1 6.4-2.65M11.75 7.5v3.75H5.5M6.75 13.25h2.5"/><path d="M2.5 2.5l11 11"/>')
icon("status", "power-profile-balanced", '<path d="M8 2.5v11M3 5.5h10"/><path d="M3 5.5l-1.5 4a1.75 1.75 0 0 0 3 0zM13 5.5l-1.5 4a1.75 1.75 0 0 0 3 0z"/>')
icon("status", "power-profile-performance", fill("M9 1.75L3.5 9h4l-1 5.25L12.5 7h-4z"))
icon("status", "power-profile-power-saver", '<path d="M3 13c0-6 4-9.5 10-10 0 6-3.5 10-9 10"/><path d="M3 13l5-5"/>')

for (context, name), text in icons.items():
    directory = os.path.join(OUT, "symbolic", context)
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, f"{name}-symbolic.svg"), "w") as f:
        f.write(text)
print(len(icons), "icons")
