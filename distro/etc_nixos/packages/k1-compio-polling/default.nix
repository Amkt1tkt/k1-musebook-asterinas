# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: use compio's polling backend in the pinned RISC-V GUI apps.
{ pkgs }:
pkg:
if !pkgs.stdenv.hostPlatform.isRiscV64 then
  pkg
else
  pkg.overrideAttrs (old:
    let
      python = pkgs.buildPackages.python3;
      nativeInputs = old.nativeBuildInputs or [ ];
      # Inspect metadata without forcing unspliced setup hooks' derivations.
      hasPython = builtins.any (input:
        builtins.isAttrs input && (input.pname or null) == python.pname
        && (input.version or null) == python.version) nativeInputs;
    in {
      nativeBuildInputs = nativeInputs ++ pkgs.lib.optional (!hasPython) python;
      postPatch = (old.postPatch or "") + ''
        ${python}/bin/python3 ${./apply_vendor.py} \
          --vendor "$cargoDepsCopy" --source .
      '';
    })
