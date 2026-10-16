{ config, lib, pkgs,
# K1 MUSE Book --- (select the available Nixpkgs manager interface)
options,
# K1 MUSE Book ---
... }:

# K1 MUSE Book --- (share manager settings and bound boot-time syslog selection)
let
  managerSettings = {
    LogLevel = if config.aster_nixos.systemd.verbose then "notice" else "crit";
    ShowStatus = if config.aster_nixos.systemd.verbose then "yes" else "no";
    # Preserve the upstream file-descriptor allowance for Nix and login sessions.
    DefaultLimitNOFILE = "1048576:1048576";
    DefaultStandardOutput = "tty";
  };

  journalUnits = [
    "systemd-journald.service"
    "systemd-journald.socket"
    "systemd-journald-dev-log.socket"
    "systemd-journald-audit.socket"
  ];

  selectSyslog = pkgs.writeShellApplication {
    name = "k1-syslog-kmsg-start";
    runtimeInputs = [ config.systemd.package pkgs.coreutils ];
    text = builtins.readFile ./k1-syslog-kmsg-start.sh;
  };
  # K1 MUSE Book ---
in {
  # K1 MUSE Book --- (board UART verbosity; default off so a clean boot stays quiet)
  options.aster_nixos.systemd.verbose = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Enable PID 1 notice-level logging and console unit status. The default
      uses critical messages only to limit UART traffic. Kernel command-line
      systemd.log_level overrides this setting; COSMIC's RUST_LOG is separate.
    '';
  };
  # K1 MUSE Book ---

  # K1 MUSE Book --- (combine common services with the available manager interface)
  config = lib.mkMerge [
    # K1 MUSE Book ---
    {
      systemd.package = pkgs.aster_systemd;

      # TODO: The following services currently do not work and
      # may affect systemd startup or cause performance issues.
      # Enable them after they can run successfully.
      systemd.coredump.enable = false;
      systemd.oomd.enable = false;
      systemd.services.logrotate.enable = false;
      systemd.services.network-setup.enable = false;
      systemd.services.resolvconf.enable = false;
      systemd.services.systemd-random-seed.enable = false;
      # K1 MUSE Book --- (the board session owns login and runtime setup)
      systemd.services.systemd-logind.enable = false;
      systemd.services."dbus-org.freedesktop.login1".enable = false;
      systemd.services.systemd-user-sessions.enable = false;
      systemd.services."user@".enable = false;
      systemd.services."user-runtime-dir@".enable = false;
      systemd.services.systemd-firstboot.enable = false;
      systemd.services.systemd-vconsole-setup.enable = false;
      # K1 MUSE Book ---
      # K1 MUSE Book --- (board modules prepare their required runtime paths)
      # Keep the generic boot/cleanup rules disabled until validated on Asterinas.
      systemd.services.systemd-tmpfiles-clean.enable = false;
      systemd.timers.systemd-tmpfiles-clean.enable = false;
      systemd.services.systemd-tmpfiles-setup.enable = false;
      # K1 MUSE Book ---

      # K1 MUSE Book --- (document the upstream compatibility policy)
      # timesyncd needs the unimplemented clock_adjtime interface; time.nix
      # reads and writes the RTC. Device registration/trigger uevents are
      # incomplete; input-udev.nix initializes libinput's device metadata.
      # K1 MUSE Book ---
      services.timesyncd.enable = false;
      services.udev.enable = false;

      # K1 MUSE Book --- (login compatibility is managed by the session module)
      # session.nix configures login PAM and the root password-aging policy.
      # K1 MUSE Book ---
      services.getty.autologinUser = "root";
      services.getty.loginProgram = "${pkgs.util-linux.bin}/bin/login";
      users.users.root = {
        shell = "${pkgs.bash}/bin/bash";
        hashedPassword = null;
      };

      systemd.targets.getty.wants =
        # tty1: provide text login ONLY when X server is disabled.
        # Other VTs: always provide text logins
        # K1 MUSE Book --- (the session module owns board console selection)
        # session.nix keeps tty1/tty2 and selects getty or a raw shell on UART.
        # K1 MUSE Book ---
        (lib.optional (!config.services.xserver.enable) "autovt@tty1.service")
        ++ [
          "autovt@hvc0.service"
          "autovt@tty2.service"
          "autovt@tty3.service"
          "autovt@tty4.service"
          "autovt@tty5.service"
          "autovt@tty6.service"
        ];

      # K1 MUSE Book --- (gate the syslog fallback on the settled journal state)
      systemd.services.aster-syslog-kmsg = {
        description =
          "Select boot-time syslog forwarding when journald is unavailable";
        wantedBy = [ "multi-user.target" ];
        after = journalUnits;
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          Type = "exec";
          ExecStart = lib.getExe selectSyslog;
          StandardOutput = "tty";
          StandardError = "inherit";
          Restart = "on-failure";
          RestartSec = 5;
        };
      };

      # Stop all default journal activation paths before rebinding /dev/log.
      # Starting journald again stops this writer through the same conflicts.
      systemd.services.aster-syslog-kmsg-forwarder = {
        description = "Forward traditional syslog to /dev/kmsg";
        after = journalUnits;
        conflicts = journalUnits;
        unitConfig.StartLimitIntervalSec = 0;
        serviceConfig = {
          Type = "simple";
          ExecStart =
            "${pkgs.busybox}/bin/busybox syslogd -n -S -l 8 -s 0 -O /dev/kmsg";
          StandardOutput = "tty";
          StandardError = "inherit";
          Restart = "on-failure";
          RestartSec = 5;
        };
      };
      # K1 MUSE Book ---
    }

    # K1 MUSE Book --- (configurable systemd logging with direct console output)
    # systemd.settings.Manager = {
    #   LogLevel = "crit";
    #   ShowStatus = "no";
    #   # Login sessions inherit these limits. Nix needs far more file descriptors
    #   # than the default limit of 1024 when it downloads a large closure.
    #   DefaultLimitNOFILE = "1048576:1048576";
    # };
    # Both interfaces generate the [Manager] section of system.conf.
    (if options.systemd ? settings then {
      systemd.settings.Manager = managerSettings;
    } else {
      systemd.extraConfig = lib.generators.toKeyValue { } managerSettings;
    })
    # K1 MUSE Book ---

    # K1 MUSE Book --- (combine common services with the available manager interface)
  ];
  # K1 MUSE Book ---
}
