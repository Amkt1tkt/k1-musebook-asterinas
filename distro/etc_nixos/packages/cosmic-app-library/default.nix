# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 application library and its Dock desktop identity.
# The package is cosmic-applibrary; the process is cosmic-app-library.
# The Dock resolves com.system76.CosmicAppLibrary through XDG_DATA_DIRS.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, rustPlatform, fetchFromGitHub, libcosmicAppHook, cosmic-icons
, just, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  # Channel pname. The binary and the session process are `cosmic-app-library`;
  # see header.
  pname = "cosmic-applibrary";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-applibrary";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-2rGw4Jmr2rL9X1URA5wVPIZocuroS8yaQs42sJqiTbw=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-+nkXcbAPcIkg+p4ct/E0bm5O0n1MGN43AaWPjw13t9s=";

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
    # Process name, not `pname`. Channel justFlags use this path.
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cosmic-app-library"
  ];

  # Channel `postPatch`. The vendor-extract recipe is not used
  # (`dontUseJustBuild`), but the string is still in the justfile.
  postPatch = ''
    substituteInPlace justfile --replace '#!/usr/bin/env' "#!$(command -v env)"
  '';

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
    homepage = "https://github.com/pop-os/cosmic-applibrary";
    description = "COSMIC application library, pinned to 25.05's alpha.7";
    mainProgram = "cosmic-app-library";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
