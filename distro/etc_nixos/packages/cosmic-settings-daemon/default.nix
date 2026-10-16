# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 settings daemon without geolocation or sound themes.
# Missing GEOCLUE_AGENT is nonfatal; the board has no location service.
# Keep adw-gtk3 data, but omit the cross-broken Pop sound-theme dependency.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, fetchFromGitHub, stdenv, rustPlatform, adw-gtk3, pkg-config
, libpulseaudio, libinput, udev, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-settings-daemon";
  version = "1.0.0-alpha.7";

  # nixpkgs-update: no auto update
  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-settings-daemon";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-vdhkE5CmgiGYg5TXxN7lLqxjv7apKEKvIscXFIzZfRc=";
  };

  postPatch = ''
    substituteInPlace src/battery.rs \
      --replace-fail '/usr/share/sounds/Pop/' '/no-pop-gtk-theme/share/sounds/Pop/'
    substituteInPlace src/theme.rs \
      --replace-fail '/usr/share/themes/adw-gtk3' '${adw-gtk3}/share/themes/adw-gtk3'
  '';

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-Dzv1SDeZFIa+LFQQ91lO7RBHldsjDnGf+R12Ln2WZwU=";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ libinput libpulseaudio udev ];

  # The build host cannot execute the RISC-V test harness.
  doCheck = false;

  # Thin LTO bounds build memory; use the same target flag key as the hook.
  env = {
    CARGO_PROFILE_RELEASE_LTO = "thin";
    GEOCLUE_AGENT = "/no-geoclue-agent";
  } // lib.optionalAttrs stdenv.hostPlatform.isRiscV64 {
    "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
      noRvvFlags;
  };

  makeFlags = [
    "prefix=$(out)"
    "CARGO_TARGET_DIR=target/${stdenv.hostPlatform.rust.cargoShortTarget}"
  ];

  dontCargoInstall = true;

  meta = {
    homepage = "https://github.com/pop-os/cosmic-settings-daemon";
    description =
      "COSMIC settings daemon, pinned to 25.05's alpha.7, no geoclue";
    mainProgram = "cosmic-settings-daemon";
    license = lib.licenses.gpl3Only;
    platforms = lib.platforms.linux;
  };
})
