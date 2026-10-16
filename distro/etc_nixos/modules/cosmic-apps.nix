# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: COSMIC applications with RISC-V rendering and file-I/O fixes.

{ config, lib, pkgs, ... }:

let
  cfg = config.aster_nixos.desktop.apps;

  # The cross-build libcosmic hook can set the build-platform flag key.
  # Append to the application's effective Cargo flags so dlopen dependencies
  # retain their RUNPATH after Nix's fixup.
  forceLinkFlags = lib.concatStringsSep " "
    ([ "-C link-arg=-Wl,--push-state,--no-as-needed" ]
      ++ map (l: "-C link-arg=-l${l}") [
        "EGL"
        "xkbcommon"
        "X11"
        "X11-xcb"
        "Xcursor"
        "Xi"
        "xcb"
        "wayland-client"
        "wayland-egl"
        "vulkan"
      ] ++ [ "-C link-arg=-Wl,--pop-state" ]);
  appendForceLink = import ../overlays/no-rvv-rust/append-rustflags.nix {
    inherit lib;
    hostPlatform = pkgs.stdenv.hostPlatform;
    flags = forceLinkFlags;
    # cosmic-edit already provides its own EGL and Wayland link flags.
    marker = "-lwayland-client";
  };

  # Only GUI recipes provide these link libraries and the libcosmic wrapper.
  withK1GuiFixes = pkg:
    if !pkgs.stdenv.hostPlatform.isRiscV64 then
      pkg
    else
      (import ../packages/k1-softbuffer-damage { inherit pkgs; })
      (pkg.overrideAttrs appendForceLink);

  # These three recipes share cosmic-files' compio dependency. Use polling
  # on RISC-V because Asterinas does not implement io_uring.
  pollingApps = map (import ../packages/k1-compio-polling { inherit pkgs; })
    (with pkgs; [ cosmic-files cosmic-edit cosmic-term ]);
  guiApps = [ pkgs.cosmic-settings ] ++ pollingApps;

in {
  options.aster_nixos.desktop.apps = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install COSMIC Settings, Files, Edit and Terminal, display and screenshot
      command-line clients, icon themes and Fira fonts. GUI applications use
      software rendering by default on RISC-V.

      This module requires [](#opt-aster_nixos.desktop.session).
      The screenshot client also requires a Screenshot portal backend, which
      the current K1 session does not provide.
    '';
  };

  options.aster_nixos.desktop.appsExtra = lib.mkOption {
    type = lib.types.listOf lib.types.package;
    default = [ ];
    example = lib.literalExpression "[ pkgs.cosmic-store pkgs.cosmic-player ]";
    description = ''
      Additional packages to install when [](#opt-aster_nixos.desktop.apps)
      is enabled. Packages are added unchanged; this option does not apply
      the bundled applications' compatibility fixes or configure services.
    '';
  };

  config = lib.mkIf cfg {
    assertions = [{
      assertion = config.aster_nixos.desktop.session;
      message = "aster_nixos.desktop.apps needs aster_nixos.desktop.session";
    }];

    environment.systemPackages = (map withK1GuiFixes guiApps) ++ (with pkgs; [
      cosmic-randr
      cosmic-screenshot
      cosmic-icons
      hicolor-icon-theme
    ]) ++ config.aster_nixos.desktop.appsExtra;

    # Provide an additional UI font without overriding COSMIC's font settings.
    fonts.packages = [ pkgs.fira ];
  };
}
