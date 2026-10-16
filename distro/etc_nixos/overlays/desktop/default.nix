# SPDX-License-Identifier: MPL-2.0

# K1 MUSE Book --- (apply desktop compatibility in each nixpkgs package namespace)
# Original upstream implementation:
# final: prev: {
#   xorg-server = prev.xorg-server.overrideAttrs (oldAttrs: {
#     patches = (oldAttrs.patches or [ ]) ++ [
#       ./patches/xorgServer/0001-Skip-checking-graphics-under-sys.patch
#     ];
#     buildInputs = (oldAttrs.buildInputs or [ ]) ++ [ final.libudev-zero ];
#     mesonFlags = (oldAttrs.mesonFlags or [ ]) ++ [
#       "-Dglamor=true"
#       "-Doptimization=0"
#       "-Dudev=false"
#       "-Dudev_kms=false"
#     ];
#     postInstall = (oldAttrs.postInstall or "") + ''
#       mkdir -p $out/share/X11/xorg.conf.d
#       cp ${./patches/xorgServer/10-modesetting.conf} $out/share/X11/xorg.conf.d/10-modesetting.conf
#     '';
#   });
#
#   xf86-video-fbdev = prev.xf86-video-fbdev.overrideAttrs (oldAttrs: {
#     # The driver loads its helper modules after dlopen.
#     # See https://github.com/NixOS/nixpkgs/pull/545344.
#     hardeningDisable = (oldAttrs.hardeningDisable or [ ]) ++ [ "bindnow" ];
#   });
#
#   xfdesktop = prev.xfdesktop.overrideAttrs (oldAttrs: {
#     patches = (oldAttrs.patches or [ ]) ++ [
#       ./patches/xfdesktop4/0001-Fix-not-using-consistent-monitor-identifiers.patch
#     ];
#   });
# }

final: prev:
let
  patchXorgServer = package:
    package.overrideAttrs (oldAttrs: {
      patches = (oldAttrs.patches or [ ])
        ++ [ ./patches/xorgServer/0001-Skip-checking-graphics-under-sys.patch ];
      buildInputs = (oldAttrs.buildInputs or [ ]) ++ [ final.libudev-zero ];
      mesonFlags = (oldAttrs.mesonFlags or [ ])
        ++ [ "-Dglamor=true" "-Dudev=false" "-Dudev_kms=false" ];
      postInstall = (oldAttrs.postInstall or "") + ''
        mkdir -p $out/share/X11/xorg.conf.d
        cp ${
          ./patches/xorgServer/10-modesetting.conf
        } $out/share/X11/xorg.conf.d/10-modesetting.conf
      '';
    });

  patchFbdev = package:
    package.overrideAttrs (oldAttrs: {
      # The driver loads its helper modules after dlopen.
      # See https://github.com/NixOS/nixpkgs/pull/545344.
      hardeningDisable =
        final.lib.unique ((oldAttrs.hardeningDisable or [ ]) ++ [ "bindnow" ]);
    });

  patchXfdesktop = package:
    package.overrideAttrs (oldAttrs: {
      patches = (oldAttrs.patches or [ ]) ++ [
        ./patches/xfdesktop4/0001-Fall-back-for-missing-monitor-model.patch
      ];
    });
  # NixOS 25.05 consumes the scoped packages; modern nixpkgs uses top-level names.
in (if prev ? xorg-server then {
  xorg-server = patchXorgServer prev.xorg-server;
  xf86-video-fbdev = patchFbdev prev.xf86-video-fbdev;
} else {
  xorg = prev.xorg.overrideScope (_: scopedPrev: {
    xorgserver = patchXorgServer scopedPrev.xorgserver;
    xf86videofbdev = patchFbdev scopedPrev.xf86videofbdev;
  });
}) // (if prev ? xfdesktop then {
  xfdesktop = patchXfdesktop prev.xfdesktop;
} else {
  xfce = prev.xfce.overrideScope
    (_: scopedPrev: { xfdesktop = patchXfdesktop scopedPrev.xfdesktop; });
})
# K1 MUSE Book ---
