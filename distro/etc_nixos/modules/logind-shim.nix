# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: login1-compatible brightness and power bridge.
# With a system bus, systemd starts the shim; the session launcher supplies
# the fallback session-bus instance. Keep LIBSEAT_BACKEND=builtin because
# this deliberately small service does not implement logind TakeDevice.

{ config, options, lib, pkgs, ... }:

let
  cfg = config.aster_nixos.desktop.logind-shim;
  # Use the overlaid rustPlatform without requiring a GPU overlay package.
  logind-shim = pkgs.callPackage ../packages/logind-shim { };
  # Check the declared options: reading an undeclared option is an eval error.
  dbusSystem = builtins.hasAttr "dbus" options.aster_nixos
    && config.aster_nixos.dbus.system;

in {
  options.aster_nixos.desktop.logind-shim = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Whether to install the minimal `org.freedesktop.login1` bridge for
      COSMIC. `Session.SetBrightness` writes
      `/sys/class/<subsystem>/<name>/brightness`. `Manager.PowerOff` and
      `Manager.Reboot` verify that the caller is root and request systemd's
      orderly shutdown or reboot target.

      When aster_nixos.dbus.system is enabled, systemd starts the shim.
      Otherwise k1-cosmic-session starts it on the fallback bus.
      This service does not provide logind session or device management.

      Startup requires a backlight with a brightness file and a readable,
      valid max_brightness. If none is found, the shim exits without
      publishing either the brightness or power interfaces. Independent of
      [](#opt-aster_nixos.desktop.session).
    '';
  };

  config = lib.mkIf cfg (lib.mkMerge [
    { environment.systemPackages = [ logind-shim ]; }

    (lib.mkIf dbusSystem {
      # k1-cosmic-session supplies the shim only on the fallback session bus.
      # A failed backlight probe exits without claiming login1.
      systemd.services.logind-shim = {
        # Use dbus.nix's socket instead of zbus's /var/run fallback.
        environment.DBUS_SYSTEM_BUS_ADDRESS =
          "unix:path=/run/dbus/system_bus_socket";
        description = "Minimal org.freedesktop.login1 (brightness and power)";
        after = [ "dbus.service" ];
        wants = [ "dbus.service" ];
        wantedBy = [ "multi-user.target" ];
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          # Type=dbus requires dbus.socket, which this custom bus does not provide.
          Type = "simple";
          ExecStart = "${logind-shim}/bin/logind-shim";
          Restart = "no";
          StandardOutput = "append:/run/logind-shim.stderr";
          StandardError = "append:/run/logind-shim.stderr";
        };
      };
    })
  ]);
}
