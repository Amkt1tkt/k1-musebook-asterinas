# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: root autologin, serial rescue and COSMIC session startup.
# A system unit replaces the unavailable systemd user session manager.
# tty1 belongs to the graphical session; tty2 and the UART remain rescue paths.
# /etc/aster-no-desktop or aster.no_desktop on the command line skips graphics.

{ config, lib, pkgs, ... }:

let
  cfgAuto = config.aster_nixos.desktop.autoStart;
  cfgSession = config.aster_nixos.desktop.session;
  sessionEnv = config.environment.sessionVariables;

in {
  options.aster_nixos.desktop.autoStart = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Start the COSMIC session at boot. Requires desktop.session, which
      installs k1-cosmic-session. When disabled, the unit remains available
      for manual startup and the system boots to text logins.

      Creating /etc/aster-no-desktop or setting aster.no_desktop on the
      kernel command line suppresses automatic and manual service starts.
      The serial console remains available independently.
    '';
  };

  options.aster_nixos.serialConsole.rawShell = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Start a PAM-free root shell on the serial console instead of autologin.
      By default, console-getty provides autologin, with at most five
      starts in 60 seconds before falling back to this rescue shell.
    '';
  };

  config = lib.mkMerge [
    {
      # Autologin skips password authentication but still runs account checks.
      # Keep password expiry from forcing a password change during login -f.
      security.pam.services.login.rules.account.unix.settings.no_pass_expiry =
        true;
      # Avoid making login depend on lastlog storage via required pam_lastlog.
      # util-linux login may still update utmp/wtmp itself.
      security.pam.services.login.updateWtmp = lib.mkForce false;

      # Existing mutable accounts retain their password-aging fields.
      # Day 1 avoids day 0's forced password-change meaning.
      system.activationScripts.k1RootNoAging = {
        deps = [ "users" ];
        text = ''
          echo "activation: disabling root password aging"
          ${pkgs.shadow}/bin/chage -d 1 -M -1 -m 0 root || true
        '';
      };

      # NixOS normally enables console-getty only for containers. Keep it on
      # the UART and enter the rescue shell when automatic retries end.
      systemd.services.console-getty = {
        enable = lib.mkForce (!config.aster_nixos.serialConsole.rawShell);
        wantedBy = [ "multi-user.target" ];
        unitConfig = {
          StartLimitBurst = 5;
          StartLimitIntervalSec = 60;
          OnFailure = "console-shell.service";
        };
        # Skip OnFailure during retries (systemd.service(5), RestartMode).
        serviceConfig.RestartMode = "direct";
      };

      # rawShell bypasses login/PAM; otherwise this is the getty's fallback.
      systemd.services.console-shell = {
        description = "Rescue root shell on the serial console (no login/PAM)";
        # Keep the two login paths from reading the same UART concurrently.
        after = [ "console-getty.service" ];
        conflicts = [ "console-getty.service" ];
        wantedBy = lib.optional config.aster_nixos.serialConsole.rawShell
          "multi-user.target";
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          Type = "idle";
          ExecStart = "${pkgs.bashInteractive}/bin/bash --login";
          Environment = [ "HOME=/root" "TERM=linux" ];
          WorkingDirectory = "/root";
          StandardInput = "tty";
          StandardOutput = "tty";
          StandardError = "tty";
          TTYPath = "/dev/console";
          Restart = "always";
          RestartSec = 2;
        };
      };

      systemd.targets.getty.wants =
        lib.mkForce [ "autovt@tty1.service" "autovt@tty2.service" ];

      systemd.services."autovt@hvc0".enable = lib.mkForce false;
      systemd.services."autovt@tty3".enable = lib.mkForce false;
      systemd.services."autovt@tty4".enable = lib.mkForce false;
      systemd.services."autovt@tty5".enable = lib.mkForce false;
      systemd.services."autovt@tty6".enable = lib.mkForce false;

      assertions = [
        {
          assertion = !cfgAuto || cfgSession;
          message = ''
            aster_nixos.desktop.autoStart requires aster_nixos.desktop.session = true
            to install k1-cosmic-session. Disable autoStart to boot to text logins.
          '';
        }
        {
          assertion = !cfgAuto || config.aster_nixos.disable-systemd != "true";
          message = ''
            aster_nixos.desktop.autoStart requires systemd to start the session service.
            Set aster_nixos.disable-systemd = "false" or disable autoStart.
          '';
        }
      ];
    }

    # The unit is defined whenever the session layer is installed, so a
    # build with `autoStart = false` can still `systemctl start` it.
    # `wantedBy` / default target are the only pieces gated on autoStart.
    (lib.mkIf cfgSession {
      systemd.services.cosmic-session = {
        description = "COSMIC desktop session";
        # Order after text services so Conflicts stops tty1's getty before
        # the compositor takes the VT, while serial recovery is available.
        after =
          [ "multi-user.target" "getty@tty1.service" "autovt@tty1.service" ];
        conflicts = [ "getty@tty1.service" "autovt@tty1.service" ];

        # Conditions below also apply to manual starts.
        wantedBy = lib.optionals cfgAuto [ "graphical.target" ];

        unitConfig = {
          ConditionPathExists = "!/etc/aster-no-desktop";
          # A bare word also matches `aster.no_desktop=…` (LHS of an
          # assignment; systemd.unit(5)). Either spelling skips the unit.
          ConditionKernelCommandLine = "!aster.no_desktop";
          # Bound restart loops while allowing isolated session failures to retry.
          StartLimitBurst = 3;
          StartLimitIntervalSec = 90;
        };

        serviceConfig = {
          Type = "simple";
          # Use the launcher and dependencies from the active system generation.
          ExecStart = "/run/current-system/sw/bin/k1-cosmic-session";
          WorkingDirectory = "/root";
          # libseat needs tty1 as its controlling terminal for VT operations.
          # Write output directly to kmsg: tty1 is in graphics mode, and
          # journald stream forwarding is unavailable on this kernel.
          StandardInput = "tty";
          StandardOutput = "append:/dev/kmsg";
          StandardError = "append:/dev/kmsg";
          TTYPath = "/dev/tty1";
          TTYReset = "yes";
          Restart = "on-failure";
          # Retry without starting the conflicting recovery getty.
          RestartMode = "direct";
          RestartSec = 3;
          # Stop all session children together so none retains the DRM master.
          KillMode = "control-group";
          TimeoutStopSec = 30;
        };

        # Optional battery/brightness helpers are supplied by the system profile,
        # outside the launcher's fixed runtime dependencies.
        path = [ config.system.path ];

        # System services do not source /etc/profile. Include the full session
        # environment so GPU driver selection reaches the compositor and clients.
        # Values must be literal strings or store paths, without shell expansion.
        environment = let
          plain = lib.mapAttrs (_: v:
            if lib.isList v then
              lib.concatMapStringsSep ":" toString v
            else
              toString v) (lib.filterAttrs (_: v: v != null) sessionEnv);
        in plain // {
          HOME = "/root";
          # The main Wayland socket needs an explicit render node for dmabuf.
          COSMIC_RENDER_DEVICE =
            plain.COSMIC_RENDER_DEVICE or "/dev/dri/renderD128";
        };

        # Restore a text login after automatic retries end. Manual stops
        # leave tty1 alone; tty2 and the serial console remain available.
        onFailure = [ "getty@tty1.service" ];
      };
    })

    (lib.mkIf (cfgAuto && cfgSession) {
      # graphical.target also orders the text and serial services first.
      systemd.defaultUnit = "graphical.target";
    })
  ];
}
