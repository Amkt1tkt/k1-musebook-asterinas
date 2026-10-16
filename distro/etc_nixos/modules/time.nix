# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: boot RTC synchronization and orderly shutdown writeback.
# The kernel already samples the SPM8821 PMIC RTC. These userspace checks
# reject old dates before further synchronization; they do not guard that sample.

{ config, lib, pkgs, ... }:

let
  cfg = config.aster_nixos.time;
  useSystemd = config.aster_nixos.disable-systemd != "true";
  # 2020-01-01 00:00:00 UTC: reject obviously uninitialized calendar values.
  minimumEpoch = 1577836800;

  hctosysGuarded = pkgs.writeShellApplication {
    name = "aster-hwclock-hctosys";
    runtimeInputs = [ pkgs.util-linux pkgs.coreutils ];
    text = ''
      if ! shown=$(hwclock --show --utc --noadjfile); then
        echo "aster-hwclock: RTC read failed; leaving system time unchanged"
        exit 0
      fi
      if ! secs=$(date -u -d "$shown" +%s 2>/dev/null); then
        echo "aster-hwclock: cannot parse RTC time '$shown'; skipping synchronization"
        exit 0
      fi
      if [ "$secs" -lt ${toString minimumEpoch} ]; then
        echo "aster-hwclock: RTC reads $shown (before 2020); skipping synchronization"
        exit 0
      fi

      if hwclock --hctosys --utc --noadjfile; then
        echo "aster-hwclock: system clock synchronized from RTC"
      else
        echo "aster-hwclock: RTC-to-system synchronization failed (non-fatal)"
      fi
    '';
  };

  systohcGuarded = pkgs.writeShellApplication {
    name = "aster-hwclock-systohc";
    runtimeInputs = [ pkgs.util-linux pkgs.coreutils ];
    text = ''
      if ! secs=$(date -u +%s); then
        echo "aster-hwclock: cannot read system time; skipping RTC writeback"
        exit 0
      fi
      if [ "$secs" -lt ${toString minimumEpoch} ]; then
        echo "aster-hwclock: system clock predates 2020; skipping RTC writeback"
        exit 0
      fi

      if ! hwclock --systohc --utc --noadjfile; then
        echo "aster-hwclock: RTC writeback failed (non-fatal)"
      fi
    '';
  };
in {
  options.aster_nixos.time.hwclock = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Synchronize system time from the default RTC device once at boot, and
      save system time to it when the writeback service stops, including
      orderly shutdown. Both directions use UTC without /etc/adjtime and
      reject dates before 2020; device failures are non-fatal.

      This option does not control the kernel's initial RTC sample. Without
      systemd, only boot synchronization is available.
    '';
  };

  config = lib.mkIf cfg.hwclock {
    environment.systemPackages = [ pkgs.util-linux ];

    # Run before core.nix's interactive stage-2 hook when PID 1 is disabled.
    boot.postBootCommands = lib.mkIf (!useSystemd) (lib.mkBefore ''
      if ! ${pkgs.coreutils}/bin/timeout --kill-after=1s 8s ${
        lib.getExe hctosysGuarded
      }; then
        echo "aster-hwclock: boot synchronization failed or timed out; continuing"
      fi
    '');

    systemd.services = lib.mkIf useSystemd {
      aster-hwclock-hctosys = {
        # Applying a configuration must not reset the running wall clock.
        restartIfChanged = false;
        description = "Synchronize system time from RTC";
        wantedBy = [ "sysinit.target" ];
        after = [ "local-fs.target" ];
        before = [ "sysinit.target" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = {
          Type = "oneshot";
          ExecStart = lib.getExe hctosysGuarded;
          RemainAfterExit = true;
          TimeoutStartSec = "8";
          TimeoutStopSec = "1";
        };
      };
      aster-hwclock-systohc = {
        description = "Save system time to RTC on service stop";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.coreutils}/bin/true";
          ExecStop = lib.getExe systohcGuarded;
          TimeoutStopSec = "8";
        };
      };
    };
  };
}
