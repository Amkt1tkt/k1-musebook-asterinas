self: super:

# K1 MUSE Book --- (boot-only activation and the board Rust target constraint)
# {
#   switch-to-configuration-ng = super.switch-to-configuration-ng.overrideAttrs (oldAttrs: {
#     patches = (oldAttrs.patches or [ ]) ++ [ ./0001-Bypass-system-dbus.patch ];
#   });
# }
let
  activationPatch = if super.lib.trivial.release == "25.05" then
    ./0001-Limit-activation-nixos-25.05.patch
  else if super.lib.trivial.release == "26.05" then
    ./0001-Limit-activation-nixos-26.05.patch
  else
    throw "Asterinas activation overlay supports only Nixpkgs 25.05 and 26.05";
  pinRust = import ../no-rvv-rust/pin.nix {
    inherit (super) lib;
    hostPlatform = super.stdenv.hostPlatform;
  };
in {
  switch-to-configuration-ng = super.switch-to-configuration-ng.overrideAttrs
    (oldAttrs:
      {
        patches = (oldAttrs.patches or [ ]) ++ [ activationPatch ];
        patchFlags = (oldAttrs.patchFlags or [ "-p1" ]) ++ [ "--fuzz=0" ];
      } // pinRust oldAttrs);
}
# K1 MUSE Book ---
