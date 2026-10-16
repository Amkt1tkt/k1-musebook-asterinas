# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: COSMIC composition, session dependencies and appearance defaults.
# The root session uses libseat's builtin backend and a private session bus;
# optional system-bus bridges live in the dbus/logind-shim/upower-shim modules.

{ config, lib, pkgs, ... }:

let
  cfg = config.aster_nixos.desktop.cosmic-comp;
  cfgTerm = config.aster_nixos.desktop.terminal;
  cfgSession = config.aster_nixos.desktop.session;
  cfgLowEffects = config.aster_nixos.desktop.lowEffects;
  cursorTheme = pkgs.vanilla-dmz;
  cursorThemeName = "DMZ-White";
  cursorEnv = {
    XCURSOR_THEME = cursorThemeName;
    XCURSOR_SIZE = "24";
    XCURSOR_PATH = "${cursorTheme}/share/icons";
  };
  # The panel button resolves this desktop ID for its name and icon before
  # dispatching its Exec command. The standalone workspaces app is absent,
  # so this is a no-op entry with the panel button's packaged icon.
  workspacesButtonEntry = pkgs.writeTextFile {
    name = "cosmic-workspaces-desktop-entry";
    destination = "/share/applications/com.system76.CosmicWorkspaces.desktop";
    text = ''
      [Desktop Entry]
      Type=Application
      Name=Workspaces
      Icon=com.system76.CosmicPanelWorkspacesButton
      Exec=true
      Terminal=false
      NoDisplay=true
      Categories=Utility;
    '';
  };
  # Keep complete schema overlays out of systemPackages: their files would
  # collide with package defaults. XDG_DATA_DIRS below establishes precedence.
  lowEffectsDefaults = pkgs.runCommand "aster-cosmic-low-effects" { } ''
    mkdir -p "$out/share/cosmic/com.system76.CosmicBackground/v1"
    cp ${./cosmic-config/com.system76.CosmicBackground/v1/all} \
      "$out/share/cosmic/com.system76.CosmicBackground/v1/all"
    cp ${./cosmic-config/com.system76.CosmicBackground/v1/backgrounds} \
      "$out/share/cosmic/com.system76.CosmicBackground/v1/backgrounds"
  '';

  # Omit applets without hardware audio or NetworkManager support. The null
  # PulseAudio sink still serves session clients. Config selects one schema
  # directory per ID, so retain all other keys and the container's entries.
  panelDefaults = pkgs.runCommand "aster-cosmic-panel-defaults" { } ''
    mkdir -p "$out/share/cosmic"
    cp -r --no-preserve=mode,ownership \
      ${pkgs.cosmic-panel.src}/data/default_schema/. "$out/share/cosmic/"
    cp ${./cosmic-config/com.system76.CosmicPanel.Panel/v1/plugins_wings} \
      "$out/share/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings"
    # Preserve the built-in Dock defaults used when no package schema is installed.
    cp ${./cosmic-config/com.system76.CosmicPanel.Dock/v1/autohide} \
      "$out/share/cosmic/com.system76.CosmicPanel.Dock/v1/autohide"
    cp ${./cosmic-config/com.system76.CosmicPanel.Dock/v1/exclusive_zone} \
      "$out/share/cosmic/com.system76.CosmicPanel.Dock/v1/exclusive_zone"
  '';
  # Capture stdout and stderr, including panics and fallback tracing output.
  # A session pipe can delay errors behind UART traffic; /run avoids rootfs writes.
  panelLogWrapper = pkgs.writeShellScriptBin "cosmic-panel" ''
    exec >>/run/cosmic-panel.stderr 2>&1
    exec ${pkgs.cosmic-panel}/bin/cosmic-panel "$@"
  '';
  # Disable the desktop-icons client to avoid full-screen SHM uploads on
  # pointer movement. cosmic-session does not restart a successful exit.
  filesAppletShim = pkgs.writeShellScriptBin "cosmic-files-applet" ''
    echo "cosmic-files-applet: desktop icons disabled"
    exit 0
  '';

  # Manual compositor entry point for serial recovery and device diagnostics.
  launcher = pkgs.writeShellApplication {
    name = "k1-cosmic-compositor";
    runtimeInputs = [ pkgs.cosmic-comp pkgs.coreutils config.systemd.package ];
    runtimeEnv = cursorEnv;
    text = builtins.readFile ./k1-cosmic-compositor.sh;
  };
  # cosmic-session resolves components by PATH; wrappers must precede packages.
  sessionLauncher = pkgs.writeShellApplication {
    name = "k1-cosmic-session";
    runtimeInputs = [
      panelLogWrapper
      filesAppletShim
      pkgs.cosmic-session
      pkgs.bash
      pkgs.dbus
      pkgs.cosmic-comp
      pkgs.cosmic-settings-daemon
      pkgs.cosmic-notifications
      pkgs.cosmic-panel
      pkgs.cosmic-applets
      pkgs.pulseaudio
      pkgs.cosmic-launcher
      pkgs.pop-launcher
      pkgs.cosmic-app-library
      pkgs.cosmic-bg
      pkgs.coreutils
      config.systemd.package
    ];
    runtimeEnv = cursorEnv;
    text = ''
      # Desktop IDs and complete schemas must be visible across package wrappers.
      cosmic_component_data_dirs=${
        lib.escapeShellArg
        ((lib.optionalString cfgLowEffects "${lowEffectsDefaults}/share:")
          + "${panelDefaults}/share:${pkgs.cosmic-applets}/share:${pkgs.cosmic-panel}/share:${pkgs.cosmic-app-library}/share:${workspacesButtonEntry}/share")
      }
    '' + builtins.readFile ./k1-cosmic-session.sh;
  };

in {
  options.aster_nixos.desktop.cosmic-comp = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install the standalone compositor launcher, cursor theme and runtime
      directories. Requires the board GPU userspace stack. The session option
      supplies the full desktop; this option alone starts nothing at boot.
    '';
  };

  options.aster_nixos.desktop.terminal = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install foot as an additional Wayland terminal and DejaVu Sans Mono.
      COSMIC keeps its packaged terminal shortcut and Dock defaults;
      existing user settings take precedence.
    '';
  };

  options.aster_nixos.desktop.session = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install the COSMIC session and its required components with a private
      session bus. The separate session module controls boot-time startup.
      A dedicated package set avoids enabling services that Asterinas does
      not implement through the full upstream NixOS COSMIC module. Default
      panel applets omit audio and network controls without service support;
      existing user layouts take precedence.
    '';
  };

  options.aster_nixos.desktop.lowEffects = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Use a solid background with no wallpaper rotation. A complete
      CosmicBackground/v1 schema is placed before the package defaults;
      existing user settings take precedence. This option affects the
      session background only, not panel animations or application rendering.
    '';
  };

  config = lib.mkMerge [
    (lib.mkIf cfg {
      assertions = [{
        assertion = config.aster_nixos.gpu.userspace;
        message =
          "aster_nixos.desktop.cosmic-comp requires aster_nixos.gpu.userspace = true (GBM/EGL via Zink).";
      }];

      environment.systemPackages = [ launcher pkgs.cosmic-comp cursorTheme ];
      # Leave DISPLAY and WAYLAND_DISPLAY unset: they select a nested backend
      # in cosmic-comp. The board render node enables dmabuf on its main socket.
      environment.sessionVariables = {
        XDG_RUNTIME_DIR = "/run/user/0";
        XDG_SESSION_TYPE = "wayland";
        COSMIC_RENDER_DEVICE = "/dev/dri/renderD128";
        inherit (cursorEnv) XCURSOR_THEME XCURSOR_SIZE;
      };
      xdg.icons.fallbackCursorThemes = [ cursorThemeName ];
      # There is no full logind user-session setup. Activation also runs when
      # the serial rescue configuration replaces systemd as PID 1.
      system.activationScripts.xdgRuntimeDirRoot = ''
        mkdir -p /run/user/0
        chmod 0700 /run/user/0
      '';
      # cosmic-comp attempts XWayland startup; Smithay prepares its sockets
      # before spawning the executable, even if XWayland is unavailable.
      system.activationScripts.x11UnixDir = ''
        echo "activation: creating /tmp/.X11-unix (mode 1777) for cosmic-comp XWayland sockets"
        mkdir -p /tmp/.X11-unix
        chmod 1777 /tmp/.X11-unix
      '';
    })

    (lib.mkIf cfgTerm {
      environment.systemPackages = [ pkgs.foot ];
      # dejavu_fonts.minimal contains only the proportional DejaVuSans face.
      fonts.packages = [ pkgs.dejavu_fonts ];
      fonts.fontconfig.defaultFonts.monospace = [ "DejaVu Sans Mono" ];
    })

    (lib.mkIf cfgSession {
      assertions = [{
        assertion = cfg;
        message =
          "aster_nixos.desktop.session requires aster_nixos.desktop.cosmic-comp = true.";
      }];

      environment.systemPackages = [
        sessionLauncher
        pkgs.cosmic-session
        pkgs.cosmic-settings-daemon
        pkgs.cosmic-notifications
        pkgs.cosmic-panel
        pkgs.cosmic-applets
        pkgs.cosmic-launcher
        pkgs.pop-launcher
        pkgs.cosmic-app-library
        pkgs.cosmic-bg
        pkgs.dbus
      ];
      # COSMIC schemas are not part of the default system profile link set.
      environment.pathsToLink =
        [ "/share/applications" "/share/cosmic" "/share/icons" ];
    })
  ];
}
