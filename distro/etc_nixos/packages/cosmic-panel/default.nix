# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 panel using its required tiny-skia renderer.
# Install alongside cosmic-applets and cosmic-notifications: schema entries
# resolve through applet desktop files, and notifications share a session socket.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, rustPlatform, fetchFromGitHub, libcosmicAppHook, cosmic-icons
, just, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-panel";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-panel";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-QcrkfU6HNZ2tWfKsMdcv58HC/PE7b4T14AIep85TWOY=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-qufOJeWPRjj4GgWNJmQfYaGKeYOQbkTeFzrUSi9QNnQ=";

  nativeBuildInputs = [
    just
    # The cross targetPackages splice lacks runtime icon packages.
    (libcosmicAppHook.override {
      includeSettings = false;
      targetPackages = { inherit cosmic-icons; };
    })
  ];

  dontUseJustBuild = true;
  dontUseJustCheck = true;

  justFlags = [
    "--set"
    "prefix"
    (placeholder "out")
    "--set"
    "bin-src"
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-panel"
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
    homepage = "https://github.com/pop-os/cosmic-panel";
    description = "COSMIC panel, pinned to 25.05's alpha.7";
    mainProgram = "cosmic-panel";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
