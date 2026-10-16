# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: software rendering and exact damage for the four pinned COSMIC GUI apps.
{ pkgs }:
pkg:
pkg.overrideAttrs (old:
  let
    # cosmic-edit has its own install-time wrapper in the pinned nixpkgs;
    # the other three packages use libcosmicAppHook during fixup.
    editWrapper = ''wrapProgram "$out/bin/cosmic-edit"'';
    isEdit = old.pname == "cosmic-edit";
    python = pkgs.buildPackages.python3;
    nativeInputs = old.nativeBuildInputs or [ ];
    # Inspect metadata without forcing unspliced cross-build hook derivations.
    hasPython = builtins.any (input:
      builtins.isAttrs input && (input.pname or null) == python.pname
      && (input.version or null) == python.version) nativeInputs;
  in assert !isEdit || pkgs.lib.hasInfix editWrapper old.postInstall; {
    nativeBuildInputs = nativeInputs ++ pkgs.lib.optional (!hasPython) python;
    postPatch = (old.postPatch or "") + ''
      ${python}/bin/python3 ${./apply_vendor.py} \
        --vendor "$cargoDepsCopy" \
        --patch ${./softbuffer-0.4.1-k1-damage.patch} \
        --helper ${./k1_damage.rs}
    '';
    postInstall = if isEdit then
      pkgs.lib.replaceStrings [ editWrapper ]
      [ (editWrapper + " --set-default ICED_BACKEND tiny-skia") ]
      old.postInstall
    else
      (old.postInstall or "");
    preFixup = (old.preFixup or "") + pkgs.lib.optionalString (!isEdit) ''
      libcosmicAppWrapperArgs+=(--set-default ICED_BACKEND tiny-skia)
    '';
  })
