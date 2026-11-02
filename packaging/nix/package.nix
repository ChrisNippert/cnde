{
  lib,
  rustPlatform,
  pkg-config,
  libinput,
  libxkbcommon,
  udev,
  seatd,
  libdrm,
  libgbm,
  libGL,
  wayland,
  pam,
}:

let
  root = ../..;
in
rustPlatform.buildRustPackage {
  pname = "cnde";
  version = "0.1.0-beta";

  # Only what the build reads: not target/, built packages or perf runs.
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions (
      map (p: root + "/${p}") [
        "Cargo.toml"
        "Cargo.lock"
        "LICENSE"
        "apps"
        "compositor"
        "config"
        "data"
        "files"
        "icons"
        "ipc"
        "lock"
        "settings"
        "share-picker"
        "shell"
        "ui"
      ]
    );
  };

  cargoLock.lockFile = root + "/Cargo.lock";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [
    libinput
    libxkbcommon
    udev
    seatd
    libdrm
    libgbm
    libGL
    wayland
    pam
  ];

  # Lets the programs find cnde's own icons and data in the store.
  env.CNDE_PREFIX = placeholder "out";

  # Some tests write to temp dirs and read the environment; run them with
  # `cargo test` in the dev shell instead.
  doCheck = false;

  postInstall = ''
    install -Dm644 data/cnde.desktop $out/share/wayland-sessions/cnde.desktop
    substituteInPlace $out/share/wayland-sessions/cnde.desktop \
      --replace-fail "cnde-compositor" "$out/bin/cnde-compositor"

    install -Dm644 data/org.freedesktop.impl.portal.desktop.cnde.service \
      $out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.cnde.service
    substituteInPlace $out/share/dbus-1/services/org.freedesktop.impl.portal.desktop.cnde.service \
      --replace-fail "/usr/bin/cnde-files-portal" "$out/bin/cnde-files-portal"
    install -Dm644 data/cnde.portal $out/share/xdg-desktop-portal/portals/cnde.portal
    install -Dm644 data/cnde-portals.conf $out/share/xdg-desktop-portal/cnde-portals.conf

    install -Dm644 data/cnde-settings.desktop $out/share/applications/cnde-settings.desktop
    install -Dm644 files/cnde-files.desktop $out/share/applications/cnde-files.desktop
    install -Dm644 data/pam.d/cnde-lock $out/etc/pam.d/cnde-lock

    mkdir -p $out/share/icons
    cp -r icons/cnde $out/share/icons/cnde
  '';

  # The GL stack is loaded at run time (libEGL.so.1); NixOS finds the
  # drivers behind it through /run/opengl-driver.
  postFixup = ''
    for bin in $out/bin/*; do
      patchelf --add-rpath ${
        lib.makeLibraryPath [
          libGL
          libxkbcommon
          wayland
        ]
      } "$bin"
    done
  '';

  passthru.providedSessions = [ "cnde" ];

  meta = {
    description = "A light Wayland desktop with an adaptive, smooth design";
    license = lib.licenses.gpl3Plus;
    platforms = lib.platforms.linux;
    mainProgram = "cnde-compositor";
  };
}
