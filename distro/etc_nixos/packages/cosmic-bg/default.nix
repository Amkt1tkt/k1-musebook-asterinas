# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 wallpaper daemon with an absolute packaged image path.
# The fallback is an image, so omitting wallpapers would leave the surface empty.
# Keep the Wayland wrapper hook even though this daemon is not an Iced UI.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, rustPlatform, fetchFromGitHub, cosmic-wallpapers
, libcosmicAppHook, cosmic-icons, just, nasm, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-bg";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-bg";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-KMP7TmamtbycF/nKctjYozMJwVr9zdp4A8AWriswo2g=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-GLXooTjcGq4MsBNnlpHBBUJGNs5UjKMQJGJuj9UO2wk=";

  # Channel `postPatch`. Kept, including the `cosmic-wallpapers`
  # interpolation; see header. Do not drop the input in this file.
  postPatch = ''
    substituteInPlace config/src/lib.rs data/v1/all \
      --replace-fail '/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg' \
      "${cosmic-wallpapers}/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg"
  '';

  nativeBuildInputs = [
    just
    # Channel has this hook even though this is not an iced app; see header.
    (libcosmicAppHook.override {
      includeSettings = false;
      targetPackages = { inherit cosmic-icons; };
    })
    # Build-platform assembler. Cross: x86_64 nasm, not riscv64. See header.
    nasm
  ];

  dontUseJustBuild = true;
  dontUseJustCheck = true;

  justFlags = [
    "--set"
    "prefix"
    (placeholder "out")
    "--set"
    "bin-src"
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-bg"
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

  meta = {
    homepage = "https://github.com/pop-os/cosmic-bg";
    description = "COSMIC background daemon, pinned to 25.05's alpha.7";
    mainProgram = "cosmic-bg";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
  };
})
