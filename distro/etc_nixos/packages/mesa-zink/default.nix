# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: Mesa Zink GL/EGL/GBM from the same release as mesa-powervr.
# EGL and GBM share Mesa-internal loader and backend interfaces; keep their
# versions aligned. Explicit feature flags limit the stack to Zink and Wayland.
# Zink does not need the ICD's CLC tools, so this derivation has no native or
# target LLVM dependency. Rusticl and software GL are disabled.
# Do not name target LLVM in disallowedRequisites: that would realize LLVM
# merely to check that the output does not reference it.

{ lib, stdenv, buildPackages, fetchurl, meson, ninja, pkg-config
, python3Packages, bison, flex, patchelf, libdrm, wayland, wayland-protocols
, wayland-scanner, libglvnd, vulkan-loader, expat, zlib, zstd, udev
, version ? (import ../mesa-release.nix).version
, hash ? (import ../mesa-release.nix).hash,

# Match mesa-powervr's explicit NDEBUG setting while retaining meson's plain
# build type and the existing optimization flags.
ndebug ? true, }:

stdenv.mkDerivation (finalAttrs: {
  pname = "mesa";
  inherit version;

  src = fetchurl {
    url = "https://archive.mesa3d.org/mesa-${finalAttrs.version}.tar.xz";
    inherit hash;
  };

  postPatch = ''
    patchShebangs .
  '';

  # Keep optional dependencies optional; required features are enabled below.
  mesonAutoFeatures = "auto";

  mesonFlags = [
    "--sysconfdir=/etc"

    (lib.mesonOption "gallium-drivers" "zink")
    (lib.mesonOption "vulkan-drivers" "")
    (lib.mesonOption "vulkan-layers" "")
    (lib.mesonOption "video-codecs" "")
    (lib.mesonOption "tools" "")

    # Wayland + the EGL platforms meson adds itself once EGL and GBM are on
    # (`surfaceless`, `drm` — `meson.build` around the `with_egl` block).
    # X11 is not a target: cosmic-comp is a Wayland compositor.
    (lib.mesonOption "platforms" "wayland")

    (lib.mesonBool "opengl" true)
    (lib.mesonEnable "gles1" false)
    (lib.mesonEnable "gles2" true)
    (lib.mesonEnable "egl" true)
    (lib.mesonEnable "gbm" true)
    # Use in-tree GBM to preserve EGL's internal backend ABI. Consumers of this
    # stack take GBM from pkgs.mesa; the channel's pkgs.libgbm is unchanged.
    (lib.mesonBool "libgbm-external" false)
    (lib.mesonEnable "glvnd" true)
    (lib.mesonOption "glx" "disabled")

    (lib.mesonEnable "llvm" false)
    (lib.mesonEnable "shared-llvm" false)
    (lib.mesonBool "gallium-rusticl" false)
    (lib.mesonBool "teflon" false)
    (lib.mesonBool "spirv-to-dxil" false)
    (lib.mesonBool "install-mesa-clc" false)
    (lib.mesonBool "install-precomp-compiler" false)

    # Keep unused features disabled even if the hook changes auto_features.
    (lib.mesonEnable "xlib-lease" false)
    (lib.mesonEnable "gallium-va" false)
    (lib.mesonEnable "gallium-mediafoundation" false)
    (lib.mesonEnable "microsoft-clc" false)
    (lib.mesonEnable "android-libbacktrace" false)
    (lib.mesonEnable "libunwind" false)
    (lib.mesonEnable "valgrind" false)
    (lib.mesonEnable "lmsensors" false)
    (lib.mesonEnable "display-info" false)
    (lib.mesonEnable "spirv-tools" false)
    (lib.mesonEnable "intel-rt" false)

    (lib.mesonEnable "zlib" true)
    (lib.mesonEnable "zstd" true)
    (lib.mesonEnable "expat" true)
    (lib.mesonEnable "xmlconfig" true)
    (lib.mesonEnable "shader-cache" true)

    # Combo, not bool. Same switch as `mesa-powervr`. See `ndebug` above.
    (lib.mesonOption "b_ndebug" (if ndebug then "true" else "false"))

    # Boolean options (auto_features does not touch these). Pin off so a
    # future default flip cannot add per-submit trace hooks. Mesa 26.1.6
    # already defaults all three to false (`meson.options`).
    (lib.mesonBool "perfetto" false)
    (lib.mesonBool "gpuvis" false)
    (lib.mesonBool "sysprof" false)
  ];

  strictDeps = true;

  depsBuildBuild = [ pkg-config buildPackages.stdenv.cc ];

  nativeBuildInputs = [
    meson
    ninja
    pkg-config
    bison
    flex
    patchelf
    wayland-scanner
    python3Packages.python
    python3Packages.mako
    python3Packages.packaging
    python3Packages.ply
    python3Packages.pycparser
    python3Packages.pyyaml
  ];

  buildInputs = [
    libdrm
    wayland
    wayland-protocols
    libglvnd
    vulkan-loader
    expat
    zlib
    zstd
    udev
  ];

  # gbm.pc requires libdrm. Propagate it so consumers' pkg-config probes can
  # enable GBM capabilities such as fd_for_plane and modifiers2.
  propagatedBuildInputs = [ libdrm ];

  # Empty on purpose. Naming a riscv64 LLVM here would realise it.
  disallowedRequisites = [ ];

  doCheck = false;

  # Check installed files without executing target binaries. nullglob makes
  # absent files detectable without depending on versioned library suffixes.
  postInstall = ''
    shopt -s nullglob

    fatal() {
      echo "mesa-zink: $1" >&2
      shift
      for dir in "$@"; do
        echo "mesa-zink: contents of $dir:" >&2
        ls -la "$dir" >&2 || true
      done
      exit 1
    }

    egl=( "$out"/lib/libEGL_mesa.so* )
    if [ ''${#egl[@]} -eq 0 ]; then
      fatal "no glvnd EGL vendor library was installed" "$out/lib"
    fi

    # With glvnd enabled, Mesa supplies the EGL vendor and libglvnd supplies
    # the public GLES library. Verify both sides of that build configuration.
    mesa_gles=( "$out"/lib/libGLESv2.so* "$out"/lib/libGLESv1_CM.so* )
    if [ ''${#mesa_gles[@]} -ne 0 ]; then
      fatal "Mesa installed its own GLES library, which only happens when glvnd is off" "$out/lib"
    fi
    glvnd_gles=( "${lib.getLib libglvnd}"/lib/libGLESv2.so* )
    if [ ''${#glvnd_gles[@]} -eq 0 ]; then
      fatal "libglvnd provides no libGLESv2; with -Dglvnd=enabled nothing else will" \
        "${lib.getLib libglvnd}/lib"
    fi

    gallium=( "$out"/lib/libgallium*.so )
    if [ ''${#gallium[@]} -eq 0 ]; then
      fatal "no libgallium (Zink megadriver) was installed" "$out/lib"
    fi

    json=( "$out"/share/glvnd/egl_vendor.d/*.json )
    if [ ''${#json[@]} -eq 0 ]; then
      fatal "no EGL vendor JSON was installed" "$out/share/glvnd/egl_vendor.d"
    fi

    gbm=( "$out"/lib/libgbm.so* )
    if [ ''${#gbm[@]} -eq 0 ]; then
      fatal "no in-tree libgbm was installed" "$out/lib"
    fi

    # smithay's build.rs is `pkg_config::probe_library("gbm")`. Linking
    # `libgbm.so` is not the same check: `gbm-sys` uses `#[link(name =
    # "gbm")]` and the cc-wrapper, and will succeed without this file.
    # The `.pc` is what turns that probe from a cargo warning into the
    # `fd_for_plane` / `modifiers2` cfgs Mesa 26.1 actually has.
    if [ ! -f "$out/lib/pkgconfig/gbm.pc" ]; then
      fatal "no gbm.pc was installed" "$out/lib/pkgconfig" "$out/lib"
    fi
  '';

  postFixup = ''
    for js in "$out"/share/glvnd/egl_vendor.d/*.json; do
      substituteInPlace "$js" --replace-fail '"libEGL_' '"'"$out/lib/libEGL_"
    done

    # Zink dlopen()s libvulkan.so; the loader is not a NEEDED of libgallium.
    for so in "$out"/lib/libgallium*.so; do
      patchelf --add-rpath ${lib.getLib vulkan-loader}/lib "$so"
    done
  '';

  passthru = {
    inherit (libglvnd) driverLink;
    eglPlatforms = [ "wayland" ];
    galliumDrivers = [ "zink" ];
    vulkanDrivers = [ ];
    vulkanLayers = [ ];
  };

  meta = {
    description =
      "Mesa GL/EGL/GBM with Zink only, no LLVM, for the SpacemiT K1";
    homepage = "https://docs.mesa3d.org/drivers/zink.html";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
})
