# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: UPower-compatible BAT0 bridge for the COSMIC battery applet.
# The shim claims its bus name only when BAT0 is readable. A real system bus
# uses a Type=simple unit; otherwise the session launcher starts the shim.

{ config, lib, pkgs, options, ... }:

let
  cfg = config.aster_nixos.desktop.upower-shim;
  # Use the overlaid rustPlatform without requiring a GPU overlay package.
  upower-shim = pkgs.callPackage ../packages/upower-shim { };
  # Check declared options before reading config; an absent bus uses the launcher.
  dbusSystem = builtins.hasAttr "dbus" options.aster_nixos
    && config.aster_nixos.dbus.system;

in {
  options.aster_nixos.desktop.upower-shim = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Whether to install the minimal `org.freedesktop.UPower` bridge for
      COSMIC battery state from `/sys/class/power_supply/BAT0`.

      When aster_nixos.dbus.system is enabled, systemd starts the shim.
      Otherwise k1-cosmic-session starts it on the fallback bus.

      The shim retries until battery data and the bus are available,
      without delaying desktop startup. It publishes its interfaces only
      after a successful battery read and retains the last valid state
      during temporary read failures. Independent of
      [](#opt-aster_nixos.desktop.session).
    '';
  };

  config = lib.mkIf cfg (lib.mkMerge [
    { environment.systemPackages = [ upower-shim ]; }

    (lib.mkIf dbusSystem {
      systemd.services.upower-shim = {
        # Use dbus.nix's socket instead of zbus's /var/run fallback.
        environment.DBUS_SYSTEM_BUS_ADDRESS =
          "unix:path=/run/dbus/system_bus_socket";
        description =
          "Minimal org.freedesktop.UPower (BAT0; no claim if unreadable)";
        after = [ "dbus.service" ];
        wants = [ "dbus.service" ];
        wantedBy = [ "multi-user.target" ];
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          # Type=dbus requires dbus.socket, which this custom bus does not provide.
          # Battery and bus readiness are retried inside the process.
          Type = "simple";
          ExecStart = "${upower-shim}/bin/upower-shim";
          Restart = "no";
          StandardOutput = "append:/run/upower-shim.stderr";
          StandardError = "append:/run/upower-shim.stderr";
        };
      };
    })
  ]);
}
