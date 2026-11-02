#!/bin/sh
# Build and install cnde without a package manager.
#
#   sh packaging/install.sh [--no-build] [--no-pam] [--no-icons]   # install
#   sh packaging/install.sh --uninstall                            # remove
#
# Environment:
#   PREFIX      where to install (default /usr/local)
#   DESTDIR     staging root, prepended to every path (default none)
#   SYSCONFDIR  where the lock screen's PAM file goes (default /etc)
#
# What needs root: writing under /usr/local (or /usr), and /etc/pam.d. For a
# per-user install use PREFIX=$HOME/.local --no-pam (the lock screen then
# falls back to login's PAM service). The display manager reads session
# files from /usr/share/wayland-sessions (some also /usr/local/share), so
# with any other prefix the session entry may need linking there by hand;
# the script prints how.
#
# Everything installed is listed in $DESTDIR$PREFIX/share/cnde/install-manifest
# and --uninstall removes exactly that.

set -eu

PREFIX=${PREFIX:-/usr/local}
DESTDIR=${DESTDIR:-}
SYSCONFDIR=${SYSCONFDIR:-/etc}
build=1
pam=1
icons=1
uninstall=0

for arg in "$@"; do
    case $arg in
        --no-build) build=0 ;;
        --no-pam) pam=0 ;;
        --no-icons) icons=0 ;;
        --uninstall) uninstall=1 ;;
        -h | --help)
            sed -n '2,/^set -eu/p' "$0" | sed '$d'
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
manifest=$DESTDIR$PREFIX/share/cnde/install-manifest

if [ "$uninstall" = 1 ]; then
    if [ ! -f "$manifest" ]; then
        echo "no manifest at $manifest; nothing to uninstall" >&2
        exit 1
    fi
    # Files first, then the directories they leave empty (never ones that
    # still hold something).
    list=$(mktemp)
    trap 'rm -f "$list"' EXIT INT TERM
    cp "$manifest" "$list"
    while IFS= read -r path; do
        rm -f "$DESTDIR$path"
    done < "$list"
    rm -f "$manifest"
    rmdir "$DESTDIR$PREFIX/share/cnde" 2> /dev/null || true
    while IFS= read -r path; do
        dir=$(dirname "$path")
        while [ "$dir" != / ] && [ "$dir" != "$PREFIX" ] && [ "$dir" != . ] \
            && rmdir "$DESTDIR$dir" 2> /dev/null; do
            dir=$(dirname "$dir")
        done
    done < "$list"
    echo "cnde uninstalled from $DESTDIR$PREFIX"
    exit 0
fi

cd "$root"
if [ "$build" = 1 ]; then
    # CNDE_PREFIX lets the binaries find cnde's own icons and data under a
    # prefix that isn't in XDG_DATA_DIRS.
    CNDE_PREFIX=$PREFIX cargo build --release --locked --workspace
fi
for bin in cnde-compositor cnde-share-picker cnde-shell cnde-settings cnde-lock \
    cnde-files cnde-files-portal; do
    if [ ! -f "target/release/$bin" ]; then
        echo "target/release/$bin is missing; build first (drop --no-build)" >&2
        exit 1
    fi
done

new=$(mktemp)
trap 'rm -f "$new"' EXIT INT TERM

# Install one file and note its final path (without DESTDIR) in the manifest.
put() { # mode source destination
    install -Dm"$1" "$2" "$DESTDIR$3"
    echo "$3" >> "$new"
}

# Install a copy of a data file with the install prefix filled in.
put_sed() { # mode source destination sed-expression
    tmp=$(mktemp)
    sed "$4" "$2" > "$tmp"
    put "$1" "$tmp" "$3"
    rm -f "$tmp"
}

for bin in cnde-compositor cnde-share-picker cnde-shell cnde-settings cnde-lock \
    cnde-files cnde-files-portal; do
    put 755 "target/release/$bin" "$PREFIX/bin/$bin"
done

# The session entry and the D-Bus service name binaries by absolute path:
# a display manager and the bus don't have our prefix in their PATH.
put_sed 644 data/cnde.desktop "$PREFIX/share/wayland-sessions/cnde.desktop" \
    "s|^Exec=env RUST_BACKTRACE=1 cnde-compositor|Exec=env RUST_BACKTRACE=1 $PREFIX/bin/cnde-compositor|"
put_sed 644 data/org.freedesktop.impl.portal.desktop.cnde.service \
    "$PREFIX/share/dbus-1/services/org.freedesktop.impl.portal.desktop.cnde.service" \
    "s|^Exec=.*|Exec=$PREFIX/bin/cnde-files-portal|"
put 644 data/cnde.portal "$PREFIX/share/xdg-desktop-portal/portals/cnde.portal"

put 644 data/cnde-settings.desktop "$PREFIX/share/applications/cnde-settings.desktop"
put 644 files/cnde-files.desktop "$PREFIX/share/applications/cnde-files.desktop"
put 644 LICENSE "$PREFIX/share/licenses/cnde/LICENSE"

if [ "$icons" = 1 ]; then
    (cd icons && find cnde -type f) | sort | while IFS= read -r file; do
        put 644 "icons/$file" "$PREFIX/share/icons/$file"
    done
fi

# PAM only reads its own directory, whatever the prefix.
if [ "$pam" = 1 ]; then
    put 644 data/pam.d/cnde-lock "$SYSCONFDIR/pam.d/cnde-lock"
fi

mkdir -p "$(dirname "$manifest")"
if [ -f "$manifest" ]; then
    # A reinstall: keep what the old manifest listed that this one doesn't.
    cat "$manifest" >> "$new"
fi
sort -u "$new" > "$manifest"

echo "cnde installed to $DESTDIR$PREFIX"
case $PREFIX in
    /usr) ;;
    *)
        echo "note: display managers read /usr/share/wayland-sessions; to list cnde there run (as root):"
        echo "  ln -s $PREFIX/share/wayland-sessions/cnde.desktop /usr/share/wayland-sessions/cnde.desktop"
        echo "note: apps, portals and the bus must see $PREFIX/share in XDG_DATA_DIRS"
        ;;
esac
