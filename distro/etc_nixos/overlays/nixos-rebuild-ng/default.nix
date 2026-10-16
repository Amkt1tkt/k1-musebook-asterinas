self: super:

# K1 MUSE Book --- (select exact activation sources and reject unsafe online changes)
# {
#   nixos-rebuild-ng = super.nixos-rebuild-ng.overrideAttrs (oldAttrs: {
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
in {
  nixos-rebuild-ng = super.nixos-rebuild-ng.overrideAttrs (oldAttrs: {
    patches = (oldAttrs.patches or [ ]) ++ [ activationPatch ];
    patchFlags = (oldAttrs.patchFlags or [ "-p1" ]) ++ [ "--fuzz=0" ];
    postPatch = (oldAttrs.postPatch or "") + ''
      cp ${./test-activation.py} tests/test_asterinas_activation.py
    '';
    # These upstream cases require online activation. The replacement tests
    # exercise its rejection before upgrades, builds and profile changes.
    disabledTests = (oldAttrs.disabledTests or [ ]) ++ [
      "test_execute_nix_switch_flake"
      "test_execute_nix_switch_build_target_host"
      "test_execute_nix_switch_flake_target_host"
      "test_execute_nix_switch_flake_build_host"
      "test_execute_switch_rollback"
      "test_execute_test_flake"
      "test_execute_test_rollback"
    ] ++ super.lib.optionals (super.lib.trivial.release == "26.05") [
      "test_execute_switch_store_path"
      "test_execute_switch_store_path_target_host"
    ];
  });
}
# K1 MUSE Book ---
