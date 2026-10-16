# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: optional Vulkan benchmark, excluded from the desktop by default.
# The package provides explicit Wayland/headless wrappers and does not imply
# driver activation, allowing enumeration-failure controls when desired.

{ config, options, lib, pkgs, ... }:

let
  cfg = config.aster_nixos.gpu.vkmark;
  vkmark = pkgs.callPackage ../packages/vkmark { };

  # Each sibling is optional when this module is imported independently.
  enabled = name: builtins.hasAttr name options.aster_nixos.gpu
    && config.aster_nixos.gpu.${name};
  vulkanOn = enabled "vulkan" || enabled "userspace";

in {
  options.aster_nixos.gpu.vkmark = lib.mkOption {
    type = lib.types.bool;
    default = false;
    description = ''
      Whether to install `vkmark` (Vulkan, Wayland + headless plugins only).

      Disabled by default. Does not enable [](#opt-aster_nixos.gpu.vulkan)
      or [](#opt-aster_nixos.gpu.userspace); those options supply the K1 ICD
      and required `PVR_I_WANT_A_BROKEN_VULKAN_DRIVER` opt-in.

      Wrappers:

        * `vkmark-wayland`  — `--winsys wayland` (compare to glmark2-es2-wayland)
        * `vkmark-headless` — `--winsys headless` (no compositor, GPU-only)
    '';
  };

  config = lib.mkIf cfg {
    environment.systemPackages = [ vkmark ];

    warnings = lib.optional (!vulkanOn) ''
      aster_nixos.gpu.vkmark is on but aster_nixos.gpu.vulkan (and
      userspace) are off. Using the K1 GPU requires a Vulkan ICD and
      PVR_I_WANT_A_BROKEN_VULKAN_DRIVER supplied by another runtime setup.
      Enable gpu.vulkan when benchmarking the board GPU.
    '';
  };
}
