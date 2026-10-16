# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: login1 brightness and orderly power requests.
# Build from Cargo.lock and a local locked vendor directory. Keep vendor inside
# src for cargoVendorDir, exclude Cargo output, and leave generated vendor untracked.

{ lib, stdenv, rustPlatform, }:

let noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage {
  pname = "logind-shim";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ./.;
    filter = path: type:
      let base = baseNameOf path;
      in base != "target" && lib.cleanSourceFilter path type;
  };
  cargoVendorDir = if builtins.pathExists ./vendor then
    "vendor"
  else
    throw ''
      logind-shim: missing vendor directory. Generate the locked dependencies:
        cd distro/etc_nixos/packages/logind-shim
        cargo vendor --locked vendor
    '';

  nativeBuildInputs = [ ];
  buildInputs = [ ];

  # The builder cannot execute the target test binary. Run cargo test natively.
  doCheck = false;

  env = {
    CARGO_PROFILE_RELEASE_LTO = "thin";
  } // lib.optionalAttrs stdenv.hostPlatform.isRiscV64 {
    "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
      noRvvFlags;
  };

  meta = {
    description =
      "Minimal login1 brightness and orderly power bridge for COSMIC";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "logind-shim";
  };
}
