# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 notifications daemon required by cosmic-session.
# The session starts it together with the panel and passes a shared socket.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, rustPlatform, fetchFromGitHub, libcosmicAppHook, cosmic-icons
, just, which, enableWgpu ? false, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-notifications";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-notifications";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-xcGmcOoxHb7gj6fW7xPZ5R0MTkMVXNJgvr9d5LPy9kw=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-7EDe1OjGS6Xl0N/BrZG8lzWO0pFbwiEp+kn6WyydJQM=";

  # Root package (not a virtual workspace). `--features libcosmic/wgpu`
  # is the Cargo form for enabling a dependency feature.
  buildFeatures = lib.optionals enableWgpu [ "libcosmic/wgpu" ];

  nativeBuildInputs = [
    just
    # Channel `nativeBuildInputs`. Kept: the justfile calls it, and a build
    # tool is a build-platform program, which is what `nativeBuildInputs`
    # already means under cross.
    which
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
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-notifications"
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
    homepage = "https://github.com/pop-os/cosmic-notifications";
    description = "COSMIC notifications daemon, pinned to 25.05's alpha.7";
    mainProgram = "cosmic-notifications";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
