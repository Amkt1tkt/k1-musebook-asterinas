# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: compositor pinned to alpha.7 with GBM and EGL from the same Mesa.
# The local recipe supplies cross-build icon fallbacks and libdrm for gbm.pc.
# libcosmicAppHook retains the libraries loaded by soname at runtime.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, stdenv, rustPlatform, fetchFromGitHub, libcosmicAppHook, cosmic-icons
, pkg-config, libdisplay-info, libdrm, mesa, libinput, pixman, seatd, udev
, systemd,

useSystemd ? lib.meta.availableOn stdenv.hostPlatform systemd, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-comp";
  version = "1.0.0-alpha.7";

  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-comp";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-2AnGEUWumE1C4Mi5inN7enbxCdWCKbQdYpUvTK3jGQ4=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-fj6TIBuZ5hrds4WMHRa2krXN5fivKriO2Q/FWdnlGaA=";

  separateDebugInfo = true;

  nativeBuildInputs = [
    # The cross targetPackages splice lacks runtime icon packages.
    (libcosmicAppHook.override {
      includeSettings = false;
      targetPackages = { inherit cosmic-icons; };
    })
    pkg-config
  ];

  buildInputs = [
    libdisplay-info
    # Channel `libgbm` propagated this. mesa-zink's `gbm.pc` still
    # `Requires.private: libdrm`; without it on PKG_CONFIG_PATH the
    # smithay probe is the "Package gbm was not found" warning.
    libdrm
    # Match EGL and GBM internal ABI by taking both from mesa-zink.
    mesa
    libinput
    pixman
    seatd
    udev
  ] ++ lib.optional useSystemd systemd;

  # The only default feature of this tag is systemd.
  buildNoDefaultFeatures = !useSystemd;

  # The build host cannot execute the RISC-V test harness.
  doCheck = false;

  # Thin LTO bounds build memory; use the same target flag key as the hook.
  env = {
    CARGO_PROFILE_RELEASE_LTO = "thin";
  } // lib.optionalAttrs stdenv.hostPlatform.isRiscV64 {
    "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
      noRvvFlags;
  };

  makeFlags = [
    "prefix=${placeholder "out"}"
    "CARGO_TARGET_DIR=target/${stdenv.hostPlatform.rust.cargoShortTarget}"
  ];

  dontCargoInstall = true;

  meta = {
    homepage = "https://github.com/pop-os/cosmic-comp";
    description =
      "COSMIC compositor, pinned to 25.05's alpha.7, GBM from mesa-zink";
    mainProgram = "cosmic-comp";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
