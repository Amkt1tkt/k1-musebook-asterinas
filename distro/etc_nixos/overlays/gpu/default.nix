# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: board GPU, desktop and cross-build package overrides.
# Vulkan and Zink share the Mesa release pin. The board configuration enables
# the experimental PowerVR device explicitly; see modules/gpu.nix.

final: prev:

{
  powervr-firmware = final.callPackage ../../packages/powervr-firmware { };

  # The PowerVR Vulkan driver alone: no GL, no EGL, and no LLVM in the riscv64
  # closure -- the build host still needs one, for Mesa's CLC compilers. This is
  # the whole GPU-specific half of user space, and it is small.
  mesa-powervr = final.callPackage ../../packages/mesa-powervr { };

  # Cross pkg-config needs the build-platform wayland-scanner metadata.
  vulkan-tools = prev.vulkan-tools.overrideAttrs (old: {
    env = (old.env or { }) // {
      PKG_CONFIG_PATH = final.lib.concatStringsSep ":" (final.lib.unique
        (final.lib.filter (path: path != "")
          (final.lib.splitString ":" (old.env.PKG_CONFIG_PATH or "") ++ [
            "${
              final.lib.getDev final.buildPackages.wayland-scanner
            }/lib/pkgconfig"
          ])));
    };
  });

  # Use the same Mesa release for Vulkan and GBM/EGL; avoid target LLVM.
  mesa-zink = final.callPackage ../../packages/mesa-zink { };
  mesa = final.mesa-zink;

  # The compositor takes GBM from mesa-zink, not the channel libgbm.
  cosmic-comp = final.callPackage ../../packages/cosmic-comp { };

  # Keep the matching session, panel and required notifications daemon together.
  cosmic-session = final.callPackage ../../packages/cosmic-session { };
  cosmic-settings-daemon =
    final.callPackage ../../packages/cosmic-settings-daemon { };
  cosmic-panel = final.callPackage ../../packages/cosmic-panel { };
  cosmic-applets = final.callPackage ../../packages/cosmic-applets { };
  cosmic-notifications =
    final.callPackage ../../packages/cosmic-notifications { };

  # The UI spawns pop-launcher by PATH; its plugins search desktop files via XDG.
  cosmic-launcher = final.callPackage ../../packages/cosmic-launcher { };
  pop-launcher = final.callPackage ../../packages/pop-launcher { };

  # The Dock button requires com.system76.CosmicAppLibrary.desktop.
  cosmic-app-library = final.callPackage ../../packages/cosmic-app-library { };

  # Install the image-backed wallpaper fallback along with the daemon.
  cosmic-bg = final.callPackage ../../packages/cosmic-bg { };

  # Build the Wayland/GLES2 benchmark against the board Mesa.
  glmark2-es2-wayland =
    final.callPackage ../../packages/glmark2-es2-wayland { };

  # Optional native Vulkan benchmark, selected by modules/gpu-bench.nix.
  vkmark = final.callPackage ../../packages/vkmark { };

  # Generate cursors on the build host and supply the CSS default cursor alias.
  vanilla-dmz = prev.vanilla-dmz.overrideAttrs (old: {
    nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
      (final.buildPackages.xcursorgen or final.buildPackages.xorg.xcursorgen)
    ];
    # xcursorgen generates data during the build; keep unrelated target inputs.
    buildInputs = final.lib.remove (prev.xcursorgen or prev.xorg.xcursorgen)
      (old.buildInputs or [ ]);
    postInstall = (old.postInstall or "") + ''
      for theme in DMZ-White DMZ-Black; do
        ln -sfn left_ptr "$out/share/icons/$theme/cursors/default"
      done
    '';
  });

}
