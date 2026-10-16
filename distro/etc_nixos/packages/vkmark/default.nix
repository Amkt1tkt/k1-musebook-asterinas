# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: Vulkan Wayland/headless benchmark, opt-in through gpu.vkmark.
# Pin source, native protocol/code generators and the installed shader data.
# Explicit wrappers avoid selecting unsupported display/KMS backends.

{ lib, stdenv, fetchFromGitHub, meson, ninja, pkg-config, makeWrapper
, buildPackages, vulkan-headers, vulkan-loader, wayland, wayland-protocols, glm
, assimp, }:

stdenv.mkDerivation (finalAttrs: {
  pname = "vkmark";
  version = "2025.01";

  src = fetchFromGitHub {
    owner = "vkmark";
    repo = "vkmark";
    rev = finalAttrs.version;
    # Source hash from nixpkgs 25.11 for tag 2025.01.
    hash = "sha256-Rjpjqe7htwlhDdwELm74MvSzHzXLhRD/P8IES7nz/VY=";
  };

  postPatch = ''
    # Host (riscv64) vulkan.pc prefix does not contain vk.xml; the
    # registry lives in vulkan-headers. Same substitute as nixpkgs.
    substituteInPlace src/meson.build \
      --replace-fail "vulkan_dep.get_pkgconfig_variable('prefix')" \
                     "'${vulkan-headers}'"

    # 2025.01 looks up wayland-scanner as a host dep. Cross cannot run
    # that binary. Matches upstream 101be30.
    substituteInPlace meson.build \
      --replace-fail \
        "wayland_scanner_dep = dependency('wayland-scanner', required : get_option('wayland') == 'true')" \
        "wayland_scanner_dep = dependency('wayland-scanner', required : get_option('wayland') == 'true', native: true)"

    # tests/ has no meson option and would compile a host-arch binary.
    substituteInPlace meson.build \
      --replace-fail "subdir('tests')" "# tests skipped (cross; cannot execute)"
  '';

  mesonAutoFeatures = "auto";

  mesonFlags = [
    (lib.mesonOption "wayland" "true")
    (lib.mesonOption "xcb" "false")
    (lib.mesonOption "kms" "false")
  ];

  strictDeps = true;

  # Native dependencies need a build-platform pkg-config. Under strictDeps,
  # nativeBuildInputs supplies the build-to-host wrapper instead.
  depsBuildBuild = [ pkg-config ];

  nativeBuildInputs = [
    meson
    ninja
    pkg-config
    # Generate format_map_gen.h from vk.xml with the build-platform interpreter.
    buildPackages.python3
    makeWrapper
    buildPackages.wayland-scanner
  ];

  buildInputs =
    [ vulkan-headers vulkan-loader wayland wayland-protocols glm assimp ];

  env = {
    PKG_CONFIG_PATH =
      "${lib.getDev buildPackages.wayland-scanner}/lib/pkgconfig";
  };

  doCheck = false;

  # Check the binary, plugin and shaders without executing target binaries.
  # postInstall also runs in cross builds, where installCheckPhase is skipped.
  postInstall = ''
    plugin=$(echo $out/lib/vkmark/wayland.so $out/lib64/vkmark/wayland.so)
    found=
    for p in $plugin; do
      if [ -f "$p" ]; then found=$p; break; fi
    done
    test -n "$found" || { echo "no wayland winsys plugin was installed" >&2; exit 1; }
    test -f $out/bin/vkmark || { echo "no vkmark binary" >&2; exit 1; }
    test -d $out/share/vkmark/shaders || { echo "no shader data" >&2; exit 1; }
    test -f $out/share/vkmark/shaders/vkcube.vert.spv || {
      echo "prebuilt SPIR-V missing; data/ install failed" >&2; exit 1; }

    # Default probe is "choose best" and the help text only lists
    # xcb/wayland/kms. headless and display still get built. display
    # needs VK_KHR_display, which this ICD does not advertise
    # (mesa-powervr `-Dplatforms=wayland`). Force the two we can use.
    makeWrapper $out/bin/vkmark $out/bin/vkmark-wayland \
      --add-flags "--winsys wayland"
    makeWrapper $out/bin/vkmark $out/bin/vkmark-headless \
      --add-flags "--winsys headless"
  '';

  meta = {
    description = "Vulkan benchmark (Wayland + headless), no X11/KMS plugins";
    homepage = "https://github.com/vkmark/vkmark";
    license = lib.licenses.lgpl21Plus;
    platforms = lib.platforms.linux;
    mainProgram = "vkmark";
  };
})
