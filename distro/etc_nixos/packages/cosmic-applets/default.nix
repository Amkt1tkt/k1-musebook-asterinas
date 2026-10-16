# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 applets with complete desktop files and schemas.
# The installed names dispatch through one multiplexer; preserve argv0 symlinks.
# Pulse and input libraries remain link dependencies even without their daemons.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, fetchFromGitHub, rustPlatform, libcosmicAppHook, cosmic-icons
, just, pkg-config, python3, util-linuxMinimal, dbus, glib, libinput, pulseaudio
, udev, xkeyboard_config, enableWgpu ? false, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-applets";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-applets";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-DmU9Dlb8w3a8U+oSGwWARPh1SRbv/8TW7TO9SSvDe1U=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-wWs3B5hh2DP93i+4gGDTi+7NT4bj8ULJ+fT95sXxUdg=";

  # Patch the writable vendor copy; the source and dependency pins stay unchanged.
  # Virtual workspace: `--features libcosmic/wgpu` has no root
  # package to attach to. Every member inherits
  # `workspace.dependencies.libcosmic`, so add the feature there.
  # The two-line context is unique in this tag's Cargo.toml.
  postPatch = ''
    python3 ${./apply_vendor.py} \
      --vendor "$cargoDepsCopy" --patch ${./upower-initialization.patch}
  '' + lib.optionalString enableWgpu ''
        substituteInPlace Cargo.toml \
          --replace-fail \
            '    "winit",
        "dbus-config",
    ] };' \
            '    "winit",
        "dbus-config",
        "wgpu",
    ] };'
  '';

  nativeBuildInputs = [
    just
    pkg-config
    python3
    util-linuxMinimal
    # The cross targetPackages splice lacks runtime icon packages.
    (libcosmicAppHook.override {
      includeSettings = false;
      targetPackages = { inherit cosmic-icons; };
    })
  ];

  buildInputs = [
    dbus
    glib
    libinput
    # Channel lists this. cosmic-applet-audio links `libpulse-binding`.
    # No Pulse on this board is a runtime fact; dropping the input
    # would fail the link. See header.
    pulseaudio
    udev
  ];

  dontUseJustBuild = true;
  dontUseJustCheck = true;

  justFlags = [
    "--set"
    "prefix"
    (placeholder "out")
    "--set"
    "target"
    "${stdenv.hostPlatform.rust.cargoShortTarget}/release"
  ];

  # The build host cannot execute the RISC-V test harness.
  doCheck = false;

  # Thin LTO bounds build memory; use the same target flag key as the hook.
  env = {
    CARGO_PROFILE_RELEASE_LTO = "thin";
  } // lib.optionalAttrs stdenv.hostPlatform.isRiscV64 {
    "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
      noRvvFlags;
  };

  preFixup = ''
    libcosmicAppWrapperArgs+=(
      --set-default X11_BASE_RULES_XML ${xkeyboard_config}/share/X11/xkb/rules/base.xml
      --set-default X11_EXTRA_RULES_XML ${xkeyboard_config}/share/X11/xkb/rules/base.extras.xml
    )
  '';

  meta = {
    homepage = "https://github.com/pop-os/cosmic-applets";
    description = "COSMIC applets, pinned to 25.05's alpha.7";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
