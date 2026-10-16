# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: finite GLES2 Wayland benchmark through the board's Mesa.
# Keep only wayland-glesv2; direct KMS flavors would contend with the desktop.
# The native scanner runs on the builder, and libGL comes from the GPU overlay.

{ lib, glmark2, libjpeg, libpng, wayland, wayland-protocols
, buildPackages, }:

glmark2.overrideAttrs (old: {
  pname = "glmark2-es2-wayland";

  nativeBuildInputs = (old.nativeBuildInputs or [ ])
    ++ [ buildPackages.wayland-scanner ];

  buildInputs = [ libjpeg libpng wayland wayland-protocols ];

  mesonFlags = [ "-Dflavors=wayland-glesv2" ];

  env = (old.env or { }) // {
    PKG_CONFIG_PATH = lib.concatStringsSep ":" (lib.filter (path: path != "") [
      (old.env.PKG_CONFIG_PATH or "")
      "${lib.getDev buildPackages.wayland-scanner}/lib/pkgconfig"
    ]);
  };

  # The inherited recipe already wraps binaries with libGL's runtime path.

  meta = (old.meta or { }) // {
    description = "glmark2 GLES2 Wayland client, no DRM/GBM/X11 flavors";
    mainProgram = "glmark2-es2-wayland";
  };
})
