# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: populate evdev metadata before libinput enumeration.
# Records provide initialization and type properties. Device capabilities
# are still checked by libinput; reseeding does not emit hotplug notifications.

{ config, lib, pkgs, ... }:

let
  cfgDb = config.aster_nixos.input.udev-db;
  cfg = config.aster_nixos.input.udev;

  seed = pkgs.writeShellApplication {
    name = "seed-input-udev-db";
    runtimeInputs = [ pkgs.coreutils pkgs.gnused config.systemd.package ];
    text = builtins.readFile ./input-udev-seed.sh;
  };

  # A timer needs the oneshot inactive between runs so each tick executes it.
  seedUnit = {
    description = "Seed /run/udev/data for evdev (libinput udev backend)";
    after = [ "local-fs.target" ];
    wantedBy = lib.optional cfg.service "multi-user.target";
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = !cfg.reseed;
      # Keep device metadata failures non-fatal to system startup.
      ExecStart = "-${lib.getExe seed}";
    };
  };

in {
  options.aster_nixos.input.udev-db = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Install `seed-input-udev-db` and populate evdev initialization and
      type properties, by default during stage-2 activation. Libinput
      still checks device capabilities.

      Independent of [](#opt-aster_nixos.desktop.cosmic-comp); this option
      does not enable udevd.
    '';
  };

  options.aster_nixos.input.udev = {
    activation = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Run `seed-input-udev-db` from a stage-2 activation script.
        Runs whether or not systemd is PID 1
        (`aster_nixos.disable-systemd`). Disable only when another
        seed path populates the database before libinput starts.
      '';
    };

    service = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Install `seed-input-udev-db.service`, a systemd oneshot wanted
        by `multi-user.target`. Same binary as activation; idempotent
        if both are on. Off by default; requires systemd as PID 1.
      '';
    };

    reseed = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Install `seed-input-udev-db.timer` so the oneshot re-runs on
        [](#opt-aster_nixos.input.udev.reseedInterval). Catches evdev
        nodes that appear after activation (USB probe late, later
        HID). Writes `/run/udev/data` only — does **not** notify a
        live libinput, because this kernel does not multicast
        uevents. Off by default. Implies the oneshot unit exists;
        does not imply [](#opt-aster_nixos.input.udev.service).
      '';
    };

    reseedInterval = lib.mkOption {
      type = lib.types.str;
      default = "30s";
      description = ''
        `OnUnitActiveSec` for `seed-input-udev-db.timer`. Ignored
        unless [](#opt-aster_nixos.input.udev.reseed) is on.
      '';
    };
  };

  config = lib.mkIf cfgDb {
    environment.systemPackages = [
      seed
      # Enumeration and explicit-device diagnostics both require udev metadata.
      pkgs.libinput
    ];

    system.activationScripts.inputUdevDb = lib.mkIf cfg.activation ''
      echo "activation: seeding /run/udev/data for evdev"
      ${
        lib.getExe seed
      } || echo "activation: seed-input-udev-db failed (non-fatal; run seed-input-udev-db by hand)"
    '';

    systemd.services =
      lib.mkIf (cfg.service || cfg.reseed) { seed-input-udev-db = seedUnit; };

    systemd.timers = lib.mkIf cfg.reseed {
      seed-input-udev-db = {
        description =
          "Re-seed /run/udev/data when evdev nodes appear after activation";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "10s";
          OnUnitActiveSec = cfg.reseedInterval;
          AccuracySec = "5s";
          Unit = "seed-input-udev-db.service";
        };
      };
    };
  };
}
