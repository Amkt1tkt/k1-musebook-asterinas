# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 launcher with the target tokio_unstable configuration.
# Search runs in the external pop-launcher process; session.nix supplies its PATH
# and cosmic.nix supplies the desktop-entry search paths.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, rustPlatform, fetchFromGitHub, libcosmicAppHook, cosmic-icons
, just, enableWgpu ? false, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-launcher";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-launcher";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-niMsDLVMUEr8VyubDm6RGgS7p22v5nH/F8cASJOszhs=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-Wh3vI42RBfXpI7mvPVUmGK3fITGi+EGyo+lG7VnZg3w=";

  nativeBuildInputs = [
    just
    # The cross targetPackages splice lacks runtime icon packages.
    (libcosmicAppHook.override {
      includeSettings = false;
      targetPackages = { inherit cosmic-icons; };
    })
  ];

  # Optional package feature; software rendering remains the default.
  buildFeatures = lib.optionals enableWgpu [ "wgpu" ];

  dontUseJustBuild = true;
  dontUseJustCheck = true;

  justFlags = [
    "--set"
    "prefix"
    (placeholder "out")
    "--set"
    "bin-src"
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-launcher"
  ];

  # The build host cannot execute the RISC-V test harness.
  doCheck = false;

  # Thin LTO bounds build memory; use the same target flag key as the hook.
  env = {
    CARGO_PROFILE_RELEASE_LTO = "thin";
    "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
      "--cfg tokio_unstable"
      + lib.optionalString stdenv.hostPlatform.isRiscV64 " ${noRvvFlags}";
  };

  meta = {
    homepage = "https://github.com/pop-os/cosmic-launcher";
    description = "COSMIC launcher, pinned to 25.05's alpha.7";
    mainProgram = "cosmic-launcher";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
