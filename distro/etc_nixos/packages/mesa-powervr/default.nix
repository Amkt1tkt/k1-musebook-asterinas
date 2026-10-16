# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: Mesa PowerVR Vulkan ICD from release source.
# BXE-2-32 device support requires Mesa 25.3.0 or later and the `imagination`
# driver. Pin it independently of nixpkgs and share the release with mesa-zink.
# EGL and GBM must share Mesa's internal ABI; the ICD itself uses the Vulkan API.
#
# The native instance supplies mesa_clc, vtn_bindgen2 and pco_clc, with LLVM,
# clang and SPIR-V build dependencies. The cross instance takes those tools via
# `=system`, keeping LLVM out of the target ICD closure. GL lives in mesa-zink
# so an ICD-only build does not also build the GL stack.

{ lib, stdenv, buildPackages, fetchurl, meson, ninja, pkg-config
, python3Packages, bison, flex, libdrm, wayland, wayland-protocols
, wayland-scanner, expat, zlib, zstd,

# Only ever used by a *native* instance, which is the one that builds the
# build-time compilers. See the LLVM note above.
llvmPackages, spirv-tools, spirv-llvm-translator,

# Shared with `mesa-zink`. Override either argument to rebuild just this
# package against another tarball; changing the pin for both halves is
# `../mesa-release.nix`.
version ? (import ../mesa-release.nix).version
, hash ? (import ../mesa-release.nix).hash,

# Define NDEBUG explicitly: nixpkgs uses meson's plain build type, which does
# not activate b_ndebug=if-release. Preserve the hook's existing optimization
# flags while disabling Mesa's runtime validators and assertions.
ndebug ? true, }:

let
  # Build-time shader compilers must run on the build platform. `=system`
  # selects tools from the native instance instead of building cross copies.
  needsNativeTools = !stdenv.buildPlatform.canExecute stdenv.hostPlatform;
  nativeTools = buildPackages.mesa-powervr;

  # Mesa locates libclang-cpp through llvm-config --libdir. Nix splits it from
  # libLLVM, so expose a merged directory. Rewrite the full llvm-config output
  # to preserve combined queries such as --libdir --libs.
  llvmLibs = buildPackages.symlinkJoin {
    name = "llvm-and-clang-libs";
    paths = [
      (lib.getLib llvmPackages.libllvm)
      (lib.getLib llvmPackages.clang-unwrapped)
    ];
  };

  llvmConfigWrapper = buildPackages.writeShellScriptBin "llvm-config" ''
    real=${lib.getDev llvmPackages.libllvm}/bin/llvm-config
    libdir=$("$real" --libdir)
    out=$("$real" "$@") || exit $?
    printf '%s\n' "''${out//"$libdir"/${llvmLibs}/lib}"
  '';

in stdenv.mkDerivation (finalAttrs: {
  pname = "mesa-powervr";
  inherit version;

  src = fetchurl {
    url = "https://archive.mesa3d.org/mesa-${finalAttrs.version}.tar.xz";
    inherit hash;
  };

  postPatch = ''
    patchShebangs .

    # Require device information for the K1's BXE-2-32 before building the ICD.
    if [ ! -f src/imagination/common/device_info/bxe-2-32.h ]; then
      echo "mesa-${finalAttrs.version} has no device info for BXE-2-32." >&2
      echo "The K1's GPU is BVNC 36.29.52.182, first known to Mesa 25.3.0." >&2
      exit 1
    fi
  '';

  # Keep optional dependencies optional instead of promoting every auto feature.
  mesonAutoFeatures = "auto";

  mesonFlags = [
    "--sysconfdir=/etc"

    # The point of the package.
    (lib.mesonOption "vulkan-drivers" "imagination")

    # ...and nothing else. Empty rather than absent: `auto` would build
    # everything Mesa thinks applies to riscv64.
    (lib.mesonOption "gallium-drivers" "")
    (lib.mesonOption "vulkan-layers" "")
    (lib.mesonOption "video-codecs" "")
    (lib.mesonOption "tools" "")

    # Wayland WSI avoids the X11 client-library closure.
    (lib.mesonOption "platforms" "wayland")

    # The GL half is `mesa-zink`, same tarball, separate derivation.
    (lib.mesonBool "opengl" false)
    (lib.mesonEnable "gles1" false)
    (lib.mesonEnable "gles2" false)
    (lib.mesonEnable "egl" false)
    (lib.mesonEnable "gbm" false)
    (lib.mesonEnable "glvnd" false)
    (lib.mesonOption "glx" "disabled")

    # The kernel implements the open PowerVR UAPI, not the Services DDK bridge.
    (lib.mesonBool "imagination-srv" false)

    # Combo, not bool: `true` / `false` / `if-release`. See `ndebug` above.
    (lib.mesonOption "b_ndebug" (if ndebug then "true" else "false"))

    # Boolean options (auto_features does not touch these). Pin off so a
    # future default flip cannot add per-submit trace hooks. Mesa 26.1.6
    # already defaults all three to false (`meson.options`).
    (lib.mesonBool "perfetto" false)
    (lib.mesonBool "gpuvis" false)
    (lib.mesonBool "sysprof" false)

    # PVR pipeline cache is `#ifdef ENABLE_SHADER_CACHE`. Feature-auto
    # would also allow this on Linux; said explicitly so a later hook
    # that restores `auto_features=enabled` cannot disable it, and so
    # the ICD half matches `mesa-zink`.
    (lib.mesonEnable "shader-cache" true)
  ] ++ lib.optionals needsNativeTools [
    # Both compilers come from the native instance, and with `mesa-clc=system`
    # `with_clc` is false, which is what lets LLVM stay out of this closure.
    (lib.mesonOption "mesa-clc" "system")
    (lib.mesonOption "precomp-compiler" "system")
    (lib.mesonEnable "llvm" false)
    (lib.mesonEnable "shared-llvm" false)
  ] ++ lib.optionals (!needsNativeTools) [
    # This instance exists to produce the three build-time compilers a cross
    # build takes from it. `install-mesa-clc` installs `vtn_bindgen2` as well
    # (`src/compiler/spirv/meson.build:93`).
    (lib.mesonBool "install-mesa-clc" true)
    (lib.mesonBool "install-precomp-compiler" true)
    # With shared LLVM, finding `libclang-cpp` is enough and Mesa skips the list
    # of a dozen static `clang*` libraries. `auto` already resolves this way on
    # Linux; saying it means the fallback cannot come back silently.
    (lib.mesonEnable "shared-llvm" true)
    # Mesa no longer accepts clang-libdir; llvmConfigWrapper supplies the path.
  ];

  strictDeps = true;

  depsBuildBuild = [ pkg-config buildPackages.stdenv.cc ];

  nativeBuildInputs = [
    meson
    ninja
    pkg-config
    bison
    flex
    wayland-scanner
    python3Packages.python
    python3Packages.mako
    python3Packages.packaging
    python3Packages.ply
    python3Packages.pycparser
    python3Packages.pyyaml
  ] ++ lib.optional needsNativeTools nativeTools;

  buildInputs = [ libdrm wayland wayland-protocols expat zlib zstd ]
    ++ lib.optionals (!needsNativeTools) [
      # The CLC half of the build: `mesa_clc` is an LLVM program, and Mesa asks for
      # SPIRV-Tools and the SPIR-V/LLVM translator alongside it
      # (`meson.build:1975-2000`).
      llvmPackages.libllvm
      # Both, as nixpkgs' own Mesa does, and they are not interchangeable: `clang`
      # is the wrapper script, while `clang-unwrapped` is the package whose `dev`
      # output carries `include/clang/...`. Without the second one, `clc_helpers.cpp`
      # fails on `#include <clang/Config/config.h>` -- the libraries are found by
      # then, because `cpp.find_library()` contributes no include path at all.
      llvmPackages.clang
      llvmPackages.clang-unwrapped
      llvmPackages.libclc
      spirv-tools
      spirv-llvm-translator
    ];

  # The wrapper has to come first, since it exists to be found instead of the
  # real one. Everything else about LLVM discovery is nixpkgs' usual arrangement.
  preConfigure = lib.optionalString (!needsNativeTools) ''
    PATH=${llvmConfigWrapper}/bin:${lib.getDev llvmPackages.libllvm}/bin:$PATH
  '';

  # The native tools must not become runtime dependencies of a riscv64 rootfs.
  disallowedRequisites = lib.optional needsNativeTools nativeTools;

  doCheck = false;

  # Check the ICD and manifest without executing target binaries.
  # postInstall also runs in cross builds, where installCheckPhase is skipped.
  postInstall = ''
    icd=$(echo $out/share/vulkan/icd.d/powervr_mesa_icd.*.json)
    test -f "$icd" || { echo "no ICD manifest was installed" >&2; exit 1; }
    test -f $out/lib/libvulkan_powervr_mesa.so || {
      echo "no PowerVR Vulkan driver was installed" >&2; exit 1; }

    # `vulkan_icd_lib_path` is `prefix / libdir`, and nixpkgs' meson hook passes
    # an absolute `--libdir`, so the manifest should already name a store path.
    # If it ever names a bare filename the loader will search the system library
    # path, find nothing, and report no devices.
    grep -q "\"library_path\"[[:space:]]*:[[:space:]]*\"$out/lib/" "$icd" || {
      echo "the ICD manifest does not point at an absolute path in $out:" >&2
      cat "$icd" >&2
      exit 1
    }
  '';

  passthru = {
    # Where `VK_DRIVER_FILES` should point. The directory, not the file: the
    # manifest name carries an architecture suffix
    # (`vulkan-manifest-per-architecture` defaults to true and this package does
    # not override it), and a path spelled out with the suffix would silently
    # stop existing on a rebuild for another architecture.
    icdDir = "share/vulkan/icd.d";
  };

  meta = {
    description =
      "Mesa's PowerVR Rogue Vulkan driver, built on its own for the SpacemiT K1";
    homepage = "https://docs.mesa3d.org/drivers/powervr.html";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
})
