# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: COSMIC/systemd board configuration, cross-built for rv64gc.
# Cross-build against the board's pinned nixpkgs tree, independently of the
# upstream QEMU test distribution.
# Firmware boots the separate kernel/initramfs/rootfs GPT partitions; NixOS
# supplies userspace and system units, with no installed Linux kernel/GRUB.

{ config, lib, pkgs, ... }:

{
  imports = [
    ./modules/core.nix
    ./modules/systemd.nix
    # Rogue firmware, PowerVR Vulkan, and optionally the GL half (Zink over
    # Vulkan). Both Mesa halves are built from the same release tarball.
    ./modules/gpu.nix
    # COSMIC compositor/session launchers and desktop defaults. The compositor
    # requires gpu.userspace; session.nix controls automatic startup.
    ./modules/cosmic.nix
    # `/run/udev/data/c13:N` for libinput. Independent of cosmic-comp.
    ./modules/input-udev.nix
    # Minimal `org.freedesktop.UPower` for the panel's battery applet.
    ./modules/upower-shim.nix
    # agetty + autologin, and the display-manager-shaped unit that execs
    # `k1-cosmic-session` at boot.
    ./modules/session.nix
    # System/session D-Bus configuration. Disabling the system bus forces the
    # private-bus alias; an unavailable system bus also falls back to that alias.
    ./modules/dbus.nix
    # login1 brightness and root-only orderly power requests.
    ./modules/logind-shim.nix
    # Boot RTC synchronization and guarded writeback on orderly shutdown.
    ./modules/time.nix
    # `vkmark`, off by default.
    ./modules/gpu-bench.nix
    # COSMIC GUI applications, command-line clients, icons and fonts.
    ./modules/cosmic-apps.nix
  ];

  # Asterinas-specific options consumed by `core.nix`. Mirrors the option set in
  # `aster_nixos_installer/templates/aster_configuration.nix`, with K1 values.
  options.aster_nixos = {
    kernel = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      # The K1 boots its kernel from the `kernel` GPT partition, so the
      # system closure has no use for one. `null` says so.
      default = null;
      description =
        "The kernel image to link into the system closure, or null.";
    };
    disable-systemd = lib.mkOption {
      type = lib.types.enum [ "true" "false" ];
      # "false" makes the stage 2 init script exec systemd as PID 1.
      default = "false";
      description = "Whether to disable systemd in stage 2 init.";
    };
    stage-2-hook = lib.mkOption {
      type = lib.types.str;
      default = "/bin/sh -l";
      description = "Stage 2 init command (fallback when systemd disabled).";
    };
    log-level = lib.mkOption {
      type = lib.types.enum [ "error" "warn" "info" "debug" "trace" ];
      default = "error";
      description = "The log level of Asterinas NixOS.";
    };
    console = lib.mkOption {
      type = lib.types.enum [ "tty0" "hvc0" ];
      # The real K1 console (ttyS0) is set by our DTB bootargs; this value only
      # feeds the (unused-on-K1) NixOS kernel-params line in core.nix.
      default = "hvc0";
      description = "The console device.";
    };
    break-into-stage-1-shell = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description =
        "Drop into an initramfs shell instead of switching root (debug).";
    };
    substituters = lib.mkOption {
      type = lib.types.str;
      default = "https://cache.ztier.in";
      description =
        "The substituters for Asterinas NixOS (community riscv64 cache).";
    };
    trusted-public-keys = lib.mkOption {
      type = lib.types.str;
      default =
        "cache.ztier.link-1:3P5j2ZB9dNgFFFVkCQWT3mh0E+S3rIWtZvoql64UaXM=";
      description = "The trusted public keys for the riscv64 cache.";
    };
  };

  config = {
    # 25.05 needs an explicit NG opt-in; 26.05 provides it unconditionally.
    # The patched frontend rejects unsupported actions before changing profiles.
    system.rebuild = lib.optionalAttrs (lib.trivial.release == "25.05") {
      enableNg = true;
    };
    aster_nixos.gpu.tools = true;
    # Vulkan driver + loader: enough for `vulkaninfo` to name the GPU.
    aster_nixos.gpu.vulkan = true;
    # GLES2/EGL/GBM over Zink.
    aster_nixos.gpu.userspace = true;
    # Install the standalone compositor launcher and its runtime environment.
    aster_nixos.desktop.cosmic-comp = true;
    # Additional Wayland terminal and monospace font.
    aster_nixos.desktop.terminal = true;
    # Session layer: cosmic-session + settings-daemon + panel + applets.
    aster_nixos.desktop.session = true;
    # Settings / Files / Edit / Term / Screenshot + icons + Fira.
    aster_nixos.desktop.apps = true;
    # Boot into that session (graphical.target + cosmic-session.service).
    # Runtime escapes that do not need a rebuild: `touch /etc/aster-no-desktop`,
    # or `aster.no_desktop` on the kernel cmdline.
    aster_nixos.desktop.autoStart = true;
    # Two-line `/run/udev/data/c13:N` for each evdev node.
    aster_nixos.input.udev-db = true;
    # Battery state from BAT0 through the minimal UPower bridge.
    aster_nixos.desktop.upower-shim = true;
    # Brightness slider and root-only shutdown/reboot through systemd.
    aster_nixos.desktop.logind-shim = true;
    # System bus shared by the battery and login1 bridges.
    aster_nixos.dbus.system = true;
    # Read the RTC at boot and save valid system time on orderly shutdown.
    aster_nixos.time.hwclock = true;

    # Cross-compile: run compilers natively on x86_64, emit riscv64 binaries.
    nixpkgs.buildPlatform = "x86_64-linux";

    # The kernel preserves floating-point state but does not manage RVV state.
    # Pin GCC to rv64gc/lp64d; the no-rvv-rust overlay separately constrains
    # Rust targets because rustc does not consume gcc.arch.
    nixpkgs.hostPlatform = {
      system = "riscv64-linux";
      gcc = {
        arch = "rv64gc";
        abi = "lp64d";
      };
    };

    nixpkgs.overlays = [
      (import ./overlays/no-rvv-rust/default.nix)
      (import ./overlays/nixos-rebuild-ng/default.nix)
      (import ./overlays/switch-to-configuration-ng/default.nix)
      (import ./overlays/systemd/default.nix)
      (import ./overlays/gpu/default.nix)
    ];

    # K1 MUSE Book --- (keep ext2 reachable until the kernel shutdown clean pass)
    # Upstream NixOS defaults systemd.shutdownRamfs.enable to true. Its /shutdown
    # pivot detaches the old root before reboot(2), while Asterinas currently
    # marks ext2 clean by walking mounted filesystems in that final syscall.
    # systemd-shutdown still stops writers, remounts / read-only, and syncs;
    # without the pivot it retains / for the kernel's final clean + flush.
    # systemd.shutdownRamfs.enable = true; # previous inherited default
    systemd.shutdownRamfs.enable = false;
    # K1 MUSE Book ---

    # The K1 does not use the NixOS bootloader; SPL loads our kernel + initrd.
    boot.loader.grub.enable = lib.mkForce false;

    # Kernel command-line systemd.log_level takes precedence over this default.
    aster_nixos.systemd.verbose = false;

    networking.hostName = "asterinas-k1";
    system.nixos.distroName = "Asterinas NixOS";
    system.stateVersion = "25.05";

    # DHCP has not been validated with this kernel and systemdMinimal.
    networking.useDHCP = false;
    networking.firewall.enable = false;
    networking.dhcpcd.enable = false;

    # nsncd's AF_UNIX path is unsupported. Local root needs no NSS daemon;
    # clear modules that would otherwise require nscd during evaluation.
    services.nscd.enable = false;
    system.nssModules = lib.mkForce [ ];

    # Additive empty. Packages come from the imported modules.
    environment.systemPackages = with pkgs; [ ];
  };
}
