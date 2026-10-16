# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: alpha.7 session with the unsupported portal omitted.
# XDP_COSMIC names an intentionally absent executable; failure is nonfatal.
# k1-cosmic-session supplies the session without a systemd user manager.
# Shared build policy: thin LTO, no target RVV, and no cross-executed tests.

{ lib, fetchFromGitHub, bash, rustPlatform, just, dbus, stdenv, }:

let

  # Keep the package usable without the global no-RVV overlay.
  noRvvFlags = import ../../overlays/no-rvv-rust/flags.nix;

in rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cosmic-session";
  version = "1.0.0-alpha.7";

  # nixpkgs-update: no auto update
  src = fetchFromGitHub {
    owner = "pop-os";
    repo = "cosmic-session";
    tag = "epoch-${finalAttrs.version}";
    hash = "sha256-vozm4vcXV3RB9Pk6om1UNCfGh80vIVJvSwbzwGDQw3Y=";
  };

  # Pinned together with the alpha.7 source.
  cargoHash = "sha256-68budhhbt8wPY7sfDqwIs4MWB/NBXsswK6HbC2AnHqE=";

  postPatch = ''
    substituteInPlace data/start-cosmic \
      --replace-fail '/usr/bin/cosmic-session' "${
        placeholder "out"
      }/bin/cosmic-session" \
      --replace-fail '/usr/bin/dbus-run-session' "${
        lib.getBin dbus
      }/bin/dbus-run-session"
    substituteInPlace data/cosmic.desktop \
      --replace-fail '/usr/bin/start-cosmic' "${
        placeholder "out"
      }/bin/start-cosmic"
  '';

  buildInputs = [ bash ];
  nativeBuildInputs = [ just ];

  dontUseJustBuild = true;

  justFlags = [
    "--set"
    "prefix"
    (placeholder "out")
    "--set"
    "cosmic_dconf_profile"
    "${placeholder "out"}/etc/dconf/profile/cosmic"
    "--set"
    "cargo-target-dir"
    "target/${stdenv.hostPlatform.rust.cargoShortTarget}"
  ];

  # The build host cannot execute the RISC-V test harness.
  doCheck = false;

  # Thin LTO bounds build memory; use the same target flag key as the hook.
  env = {
    CARGO_PROFILE_RELEASE_LTO = "thin";
    XDP_COSMIC = "/no-xdg-desktop-portal-cosmic";
  } // lib.optionalAttrs stdenv.hostPlatform.isRiscV64 {
    "CARGO_TARGET_${stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
      noRvvFlags;
  };

  passthru = { providedSessions = [ "cosmic" ]; };

  meta = {
    homepage = "https://github.com/pop-os/cosmic-session";
    description =
      "COSMIC session manager, pinned to 25.05's alpha.7, no portal";
    license = lib.licenses.gpl3Only;
    mainProgram = "cosmic-session";
    platforms = lib.platforms.linux;
  };
})
