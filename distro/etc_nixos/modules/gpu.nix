# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: firmware, PowerVR Vulkan and Zink GL integration.
# The display controller and render node are separate devices. Keep Vulkan,
# GBM and EGL on the same Mesa release and preserve kmsro device pairing.
# The firmware has its own redistribution license; see its package metadata.

{ config, lib, pkgs, ... }:

let
  fw = pkgs.powervr-firmware;
  pvr = pkgs.mesa-powervr;
  cfg = config.aster_nixos.gpu;
  optionalEnv = lib.filterAttrs (_: v: v != null && v != "");
  # Mesa debug tokens use any non-alphanumeric, non-underscore separator.
  zinkDebugFlags = if cfg.zinkDebug == null then
    [ ]
  else
    lib.filter builtins.isString
    (builtins.split "[^a-zA-Z0-9_]+" cfg.zinkDebug);

in {
  options.aster_nixos.gpu.tools = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install libdrm and its modetest tools. Vulkan and GL userspace also
      include these tools for device enumeration and KMS diagnostics.
    '';
  };

  options.aster_nixos.gpu.vulkan = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install the PowerVR Vulkan ICD and loader. The GL userspace option
      also enables this layer; no target-side LLVM or software GL is included.
    '';
  };

  options.aster_nixos.gpu.userspace = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Install Zink GL/GLES/EGL/GBM over the PowerVR Vulkan driver, plus
      Wayland and GL diagnostic clients. Required by cosmic-comp.
    '';
  };

  options.aster_nixos.gpu.zinkDebug = lib.mkOption {
    type = lib.types.nullOr lib.types.str;
    default = null;
    example = "rp";
    description = ''
      Optional ZINK_DEBUG tokens. For Mesa 26.1.6, rp enables renderpass
      tracking; norp disables it. These tokens are mutually exclusive;
      the exact value all also enables both and is rejected.
      The default keeps Mesa behaviour unchanged.
    '';
  };

  options.aster_nixos.gpu.zinkDescriptors = lib.mkOption {
    type = lib.types.nullOr (lib.types.enum [ "auto" "lazy" "db" ]);
    default = null;
    example = "lazy";
    description = ''
      Optional ZINK_DESCRIPTORS mode. auto selects lazy on this ICD;
      db requires VK_EXT_descriptor_buffer, which this GPU stack lacks.
    '';
  };

  options.aster_nixos.gpu.nirDebug = lib.mkOption {
    type = lib.types.nullOr lib.types.str;
    default = null;
    example = "novalidate";
    description = ''
      Optional NIR_DEBUG tokens for development builds. Release Mesa uses
      NDEBUG, so its NIR validation diagnostics are compiled out.
    '';
  };

  options.aster_nixos.gpu.shaderCacheDir = lib.mkOption {
    type = lib.types.nullOr (lib.types.strMatching "/.*");
    default = "/root/.cache";
    description = ''
      Absolute, persistent Mesa shader cache parent directory. Both Mesa
      components share it; null leaves location selection to Mesa. Avoid /run,
      whose contents are lost at reboot.
    '';
  };

  options.aster_nixos.gpu.shaderCacheMaxSize = lib.mkOption {
    type = lib.types.nullOr lib.types.str;
    default = "256M";
    description = ''
      MESA_SHADER_CACHE_MAX_SIZE, the on-disk shader cache eviction target.
      The default is 256 MiB; this is not a hard quota for the parent directory.
      null restores the Mesa default.
    '';
  };

  options.aster_nixos.gpu.shaderCacheDisable = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Disable Mesa shader caching for manual diagnostics. Leave false
      for normal desktop use.
    '';
  };

  options.aster_nixos.gpu.wgpuBackend = lib.mkOption {
    type = lib.types.nullOr (lib.types.enum [ "vulkan" "gl" ]);
    default = null;
    example = "vulkan";
    description = ''
      Optional WGPU_BACKEND preference for clients built with wgpu.
      The compositor itself uses GLES through Zink.
    '';
  };

  options.aster_nixos.gpu.icedBackend = lib.mkOption {
    type =
      lib.types.nullOr (lib.types.enum [ "wgpu" "tiny-skia" "wgpu,tiny-skia" ]);
    default = null;
    example = "wgpu,tiny-skia";
    description = ''
      Optional session-wide ICED_BACKEND selection for clients with
      selectable renderers. Only compiled backends are available; fixed
      renderers ignore this setting. wgpu,tiny-skia retains software fallback.
      Four application wrappers otherwise default to tiny-skia with exact
      SHM damage.
    '';
  };

  options.aster_nixos.gpu.clientWgpu = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Enable the wgpu build feature for COSMIC Launcher, applets and
      notifications. Runtime renderer selection is separate; this does
      not change the four bundled applications' software-renderer defaults.
    '';
  };

  config = lib.mkMerge [
    {
      # Apply the independent build option after the GPU package definitions.
      nixpkgs.overlays = lib.mkAfter [
        (_: prev: {
          cosmic-launcher =
            prev.cosmic-launcher.override { enableWgpu = cfg.clientWgpu; };
          cosmic-applets =
            prev.cosmic-applets.override { enableWgpu = cfg.clientWgpu; };
          cosmic-notifications =
            prev.cosmic-notifications.override { enableWgpu = cfg.clientWgpu; };
        })
      ];

      # The firmware license permits redistribution with Imagination hardware.
      nixpkgs.config.allowUnfreePredicate = pkg:
        lib.getName pkg == "powervr-firmware";
      hardware.firmware = [ fw ];
      # Provide the final rootfs path used by Asterinas' firmware loader.
      # core.nix separately includes firmware in the initramfs for early probe.
      system.activationScripts.powervrFirmware = ''
        mkdir -p /lib/firmware
        ln -sfnT ${fw}/lib/firmware/powervr /lib/firmware/powervr
      '';
    }
    (lib.mkIf (cfg.vulkan || cfg.userspace) {
      environment.systemPackages = with pkgs; [
        mesa-powervr
        vulkan-loader
        vulkan-tools
      ];

      environment.sessionVariables = {
        # Architecture-specific ICD names live under the exported directory.
        VK_DRIVER_FILES = "${pvr}/${pvr.icdDir}";
        # Mesa requires explicit opt-in for this non-conformant BVNC.
        PVR_I_WANT_A_BROKEN_VULKAN_DRIVER = "1";
      } // optionalEnv {
        NIR_DEBUG = cfg.nirDebug;
        MESA_SHADER_CACHE_DIR = cfg.shaderCacheDir;
        MESA_SHADER_CACHE_MAX_SIZE = cfg.shaderCacheMaxSize;
        MESA_SHADER_CACHE_DISABLE =
          if cfg.shaderCacheDisable then "true" else null;
      };
      system.activationScripts.mesaShaderCache =
        lib.mkIf (cfg.shaderCacheDir != null) ''
          mkdir -p -- ${lib.escapeShellArg cfg.shaderCacheDir}
        '';
    })
    (lib.mkIf cfg.userspace {
      environment.systemPackages = with pkgs; [
        mesa
        wayland-utils
        mesa-demos
        glmark2-es2-wayland
      ];

      environment.sessionVariables = {
        # Do not force MESA_LOADER_DRIVER_OVERRIDE=zink: display-only card0
        # must go through kmsro to pair with the PowerVR render node.
        __EGL_VENDOR_LIBRARY_DIRS = "${pkgs.mesa}/share/glvnd/egl_vendor.d";
      } // optionalEnv {
        ZINK_DEBUG = cfg.zinkDebug;
        ZINK_DESCRIPTORS = cfg.zinkDescriptors;
        WGPU_BACKEND = cfg.wgpuBackend;
        ICED_BACKEND = cfg.icedBackend;
      };

      assertions = [
        {
          assertion = cfg.zinkDebug != "all" && !(lib.elem "rp" zinkDebugFlags
            && lib.elem "norp" zinkDebugFlags);
          message =
            "aster_nixos.gpu.zinkDebug cannot enable both rp and norp (including the all value).";
        }
        {
          assertion = cfg.zinkDescriptors != "db";
          message = ''
            aster_nixos.gpu.zinkDescriptors = "db" requires
            VK_EXT_descriptor_buffer, which the K1 PowerVR driver does not support.
            Use "auto" or "lazy".
          '';
        }
      ];

      warnings = lib.optional (cfg.icedBackend == "wgpu") ''
        aster_nixos.gpu.icedBackend = "wgpu" disables software fallback in
        clients with selectable renderers. Use "wgpu,tiny-skia" to retain
        fallback. Clients with fixed renderers ignore this setting.
      '';
    })

    (lib.mkIf (cfg.tools || cfg.vulkan || cfg.userspace) {
      environment.systemPackages = [ pkgs.libdrm ];
    })
    {
      # This catches accidental platform changes; the separate Rust overlay
      # and final ELF scan also enforce the no-vector userspace requirement.
      assertions = [{
        assertion = (config.nixpkgs.hostPlatform.gcc.arch or null) == "rv64gc";
        message = ''
          The riscv64 host platform must pin gcc.arch = "rv64gc" (IMAFDC, no V).
          Without it, the compiler's built-in default determines the target ISA.
          rustc does not read gcc.arch — that pin is overlays/no-rvv-rust.
        '';
      }];
    }
  ];
}
