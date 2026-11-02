# NixOS: `programs.cnde.enable = true;` adds the cnde session to the login
# screen with what it needs around it (portals, PAM for the lock screen,
# polkit, fonts, XWayland).
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.cnde;
in
{
  options.programs.cnde = {
    enable = lib.mkEnableOption "cnde, a light Wayland desktop";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The cnde package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [
      cfg.package
      # X11 apps and most games.
      pkgs.xwayland-satellite
      pkgs.xwayland
      # Event sounds (screenshots, notifications).
      pkgs.sound-theme-freedesktop
    ];

    services.displayManager.sessionPackages = [ cfg.package ];

    # The lock screen checks passwords with its own PAM service.
    security.pam.services.cnde-lock = { };
    security.polkit.enable = true;
    hardware.graphics.enable = true;

    # cnde's own file picker, screen sharing through portal-wlr, the rest
    # from the GTK portal; cnde-portals.conf picks between them.
    xdg.portal = {
      enable = true;
      extraPortals = [
        cfg.package
        pkgs.xdg-desktop-portal-wlr
        pkgs.xdg-desktop-portal-gtk
      ];
      configPackages = [ cfg.package ];
    };
    services.dbus.packages = [ cfg.package ];

    # cnde draws its text with Adwaita Sans; fonts.fontDir puts installed
    # fonts where it looks (it doesn't use fontconfig).
    fonts.packages = lib.optional (pkgs ? adwaita-fonts) pkgs.adwaita-fonts;
    fonts.fontDir.enable = true;

    # Drives in Files.
    services.udisks2.enable = lib.mkDefault true;
  };
}
