# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: system D-Bus and private-session support for the root desktop.
# System services are started explicitly; power authorization remains in the shim.

{ config, lib, pkgs, ... }:

let
  cfg = config.aster_nixos.dbus;
  dbus = pkgs.dbus;
  dbusDaemon = "${dbus}/bin/dbus-daemon";
  # Without an allow-user policy, only the daemon's UID can connect. Keep
  # root's message/name permissions, but omit system service auto-activation.
  systemConf = ''
    <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
     "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
    <busconfig>
      <type>system</type>
      <listen>unix:path=/run/dbus/system_bus_socket</listen>
      <auth>EXTERNAL</auth>
      <apparmor mode="disabled"/>

      <policy context="default">
        <deny send_type="method_call"/>
        <allow send_type="signal"/>
        <allow send_requested_reply="true" send_type="method_return"/>
        <allow send_requested_reply="true" send_type="error"/>
        <allow receive_type="method_call"/>
        <allow receive_type="method_return"/>
        <allow receive_type="error"/>
        <allow receive_type="signal"/>
        <allow send_destination="org.freedesktop.DBus"/>
        <deny own="*"/>
      </policy>

      <policy user="root">
        <allow send_destination="*"/>
        <allow own="*"/>
        <allow eavesdrop="true"/>
      </policy>

    </busconfig>
  '';
  # dbus-run-session needs this configuration in both bus modes. Preserve
  # dbus's session policy and higher limits instead of using daemon defaults.
  sessionConf = ''
    <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
     "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
    <busconfig>
      <type>session</type>
      <keep_umask/>
      <listen>unix:tmpdir=/tmp</listen>
      <auth>EXTERNAL</auth>
      <apparmor mode="disabled"/>

      <servicedir>/run/current-system/sw/share/dbus-1/services</servicedir>
      <standard_session_servicedirs />

      <policy context="default">
        <allow send_destination="*" eavesdrop="true"/>
        <allow eavesdrop="true"/>
        <allow own="*"/>
      </policy>

      <limit name="max_incoming_bytes">1000000000</limit>
      <limit name="max_incoming_unix_fds">250000000</limit>
      <limit name="max_outgoing_bytes">1000000000</limit>
      <limit name="max_outgoing_unix_fds">250000000</limit>
      <limit name="max_message_size">1000000000</limit>
      <limit name="service_start_timeout">120000</limit>
      <limit name="auth_timeout">240000</limit>
      <limit name="pending_fd_timeout">150000</limit>
      <limit name="max_completed_connections">100000</limit>
      <limit name="max_incomplete_connections">10000</limit>
      <limit name="max_connections_per_user">100000</limit>
      <limit name="max_pending_service_starts">10000</limit>
      <limit name="max_names_per_connection">50000</limit>
      <limit name="max_match_rules_per_connection">50000</limit>
      <limit name="max_replies_per_connection">50000</limit>
    </busconfig>
  '';

  ensureRuntimeDir = pkgs.writeShellScript "xdg-runtime-dir-root" ''
    ${pkgs.coreutils}/bin/install -d -m0700 -o0 -g0 /run/user/0
  '';

in {
  options.aster_nixos.dbus.system = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Start a root-owned system D-Bus without system-service auto-activation.
      When disabled, COSMIC uses its private session bus and starts the shims
      there. Both modes replace the upstream NixOS D-Bus module and provide
      session-bus configuration without a systemd user manager.
    '';
  };

  options.aster_nixos.dbus.xdg-runtime-dir = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Prepare /run/user/0, owned by root with mode 0700, through activation
      and a system unit. Independent of the system bus; desktop launchers
      also prepare this directory for manual starts.
    '';
  };

  config = lib.mkMerge [
    {
      # systemd enables the upstream module by default. Suppress it in both
      # modes so disabling our system bus cannot restore socket activation.
      services.dbus.enable = lib.mkForce false;
      environment.systemPackages = [ dbus ];
      environment.etc."dbus-1/session.conf".text = sessionConf;
      environment.pathsToLink = [ "/share/dbus-1/services" ];
      # A disabled system bus must not select a socket left by another session.
      environment.sessionVariables =
        lib.optionalAttrs (!cfg.system) { COSMIC_FORCE_DBUS_ALIAS = "1"; };
    }

    (lib.mkIf cfg.xdg-runtime-dir {
      system.activationScripts.asterDbusRuntimeDir = "${ensureRuntimeDir}";

      systemd.services.xdg-runtime-dir-root = {
        description = "Create /run/user/0 (0700, root) for D-Bus and Wayland";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${ensureRuntimeDir}";
        };
      };
    })

    (lib.mkIf cfg.system {
      environment.etc."dbus-1/system.conf".text = systemConf;
      # Some clients use the traditional /var/run address. tmpfiles-setup
      # is disabled; systemd manages /run/dbus through RuntimeDirectory.
      system.activationScripts.asterDbusRunDir = ''
        mkdir -p /var
        if [ ! -e /var/run ]; then
          rm -f /var/run
          ln -s /run /var/run
        fi
      '';

      systemd.services.dbus = {
        description = "System D-Bus message bus";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          # dbus-daemon reports READY=1 after initialization, without enabling
          # systemd service activation. After= clients wait for that notification.
          Type = "notify";
          ExecStart =
            "${dbusDaemon} --nofork --nopidfile --config-file=/etc/dbus-1/system.conf";
          RuntimeDirectory = "dbus";
          RuntimeDirectoryMode = "0755";
          Restart = "no";
        };
      };
    })
  ];
}
