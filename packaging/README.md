# Installing cnde without a package manager

`install.sh` builds the workspace and installs what the Arch PKGBUILD
installs (binaries, session entry, `.desktop` files, portal and D-Bus
files, icons, the lock screen's PAM file). Arch users can use the pacman
repository below, or `packaging/arch/PKGBUILD`.

## Arch Linux: the pacman repository

Each release is also published as a small signed pacman repository, so
cnde installs and updates like any other package. Trust its signing key
once (the key is `packaging/cnde-repo.asc` in this repository):

```sh
curl -LO https://raw.githubusercontent.com/ChrisNippert/cnde/main/packaging/cnde-repo.asc
sudo pacman-key --add cnde-repo.asc
sudo pacman-key --lsign-key <fingerprint printed by the line above>
```

Then add to `/etc/pacman.conf`:

```ini
[cnde]
Server = https://github.com/ChrisNippert/cnde/releases/download/repo
```

and install with `sudo pacman -Syu cnde` (or just the parts you want,
such as `cnde-compositor cnde-shell`). Later releases arrive with
`pacman -Syu`. The packages are built against the Arch of release day; if
an Arch library update breaks one before the next release, build from
source (`packaging/arch`) or use the AUR's `cnde-git`.

### Setting up the signing key (maintainer)

```sh
gpg --quick-gen-key "cnde repository" ed25519 sign never   # empty passphrase
gpg --armor --export "cnde repository" > packaging/cnde-repo.asc
gpg --armor --export-secret-keys "cnde repository"         # -> secret REPO_GPG_KEY
gpg --list-keys --with-colons "cnde repository" | awk -F: '/^fpr/ {print $10; exit}'
                                                           # -> secret REPO_GPG_KEY_ID
```

Add both secrets under the GitHub repository's Settings → Secrets and
variables → Actions, and commit `cnde-repo.asc`. The release workflow
skips the repository until the secrets are there.

## Build dependencies

- Rust 1.85 or newer (the workspace uses edition 2024), with cargo.
- A C toolchain and `pkg-config`.
- Development files for: libinput, libxkbcommon, libudev (systemd or eudev),
  libseat (seatd), libdrm, gbm and EGL (Mesa).

| Generic       | Debian/Ubuntu                                  | Fedora                                                                              |
| ------------- | ---------------------------------------------- | ----------------------------------------------------------------------------------- |
| build tools   | `build-essential pkg-config`                   | `gcc pkgconf-pkg-config`                                                            |
| dev libraries | `libinput-dev libxkbcommon-dev libudev-dev libseat-dev libdrm-dev libgbm-dev libegl-dev` | `libinput-devel libxkbcommon-devel systemd-devel libseat-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel` |

## Runtime dependencies

Required: libinput, libxkbcommon, Mesa (GL and GBM), libdrm, udev, and a seat
manager (seatd, or systemd-logind). The lock screen needs PAM.

Optional (what each gives you):

| Generic                      | Debian/Ubuntu                  | Fedora                        | For                                   |
| ---------------------------- | ------------------------------ | ----------------------------- | ------------------------------------- |
| Xwayland, xwayland-satellite | `xwayland xwayland-satellite`  | `xorg-x11-server-Xwayland xwayland-satellite` | X11 apps and most games  |
| xdg-desktop-portal, -gtk     | `xdg-desktop-portal xdg-desktop-portal-gtk` | `xdg-desktop-portal xdg-desktop-portal-gtk` | app settings, file pickers |
| xdg-desktop-portal-wlr       | `xdg-desktop-portal-wlr`       | `xdg-desktop-portal-wlr`      | screen sharing                        |
| freedesktop sound theme      | `sound-theme-freedesktop`      | `sound-theme-freedesktop`     | event sounds                          |
| udisks2                      | `udisks2`                      | `udisks2`                     | mount and eject drives in cnde-files  |
| ffmpegthumbnailer            | `ffmpegthumbnailer`            | `ffmpegthumbnailer`           | video thumbnails                      |
| libarchive tools (bsdtar)    | `libarchive-tools`             | `bsdtar`                      | archives in cnde-files                |
| PipeWire, WirePlumber        | `pipewire wireplumber`         | `pipewire wireplumber`        | audio                                 |
| polkit                       | `polkitd`                      | `polkit`                      | the shell's password prompts          |

Package names vary between releases; check yours if one is missing.

## Build and install

```sh
sh packaging/install.sh                 # builds, installs under /usr/local (as root)
sudo sh packaging/install.sh --no-build # install what you already built
PREFIX=$HOME/.local sh packaging/install.sh --no-pam   # per-user, no root
PREFIX=/usr DESTDIR=/tmp/stage sh packaging/install.sh # stage for a package
```

Options: `--no-build`, `--no-pam` (skip `/etc/pam.d/cnde-lock`; the lock
screen then uses the `login` PAM service), `--no-icons`. `PREFIX` is baked
into the binaries as `CNDE_PREFIX` so they find their own icons, and into the
session entry and D-Bus service as absolute paths. `SYSCONFDIR` (default
`/etc`) moves the PAM file.

Files outside `PREFIX` that need root: `/etc/pam.d/cnde-lock`, and the
session entry if your display manager only reads `/usr/share/wayland-sessions`.
With a prefix other than `/usr` the script prints the `ln -s` to run for that.
Apps, portals and the session bus must also see `PREFIX/share` in
`XDG_DATA_DIRS` (`/usr/local/share` is in the default; `~/.local/share` is too).

## Starting cnde

- From a display manager: pick "cnde" in the session list.
- From a TTY (not inside another graphical session): run
  `cnde-compositor` (use its full path if `PREFIX/bin` isn't in `PATH`).

The compositor starts the shell, and finds the other cnde programs next to
its own binary first, then in `PATH`.

## Uninstall

```sh
sh packaging/install.sh --uninstall     # same PREFIX and DESTDIR as the install
```

This removes exactly the files listed in `PREFIX/share/cnde/install-manifest`,
and any directories that leave empty. Your config in `~/.config/cnde` is left
alone, and so is a symlink you made into `/usr/share/wayland-sessions`
(remove it by hand).
