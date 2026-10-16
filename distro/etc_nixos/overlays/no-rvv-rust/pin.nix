# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: reject vector enables and keep the full disable block last.
# Explicit vector enables can leave implicit zvl features active even after
# -v/-zve; LLVM rejects that combination. Check only Cargo's effective source.
{ lib, hostPlatform }:
let
  flags = import ./flags.nix;
  separator = builtins.fromJSON ''"\u001f"'';
  validateFlags = { isEncoded, value }:
    let
      # Encoded argv can contain spaces within a single link argument.
      tokens = builtins.filter builtins.isString
        (builtins.split (if isEncoded then separator else "[[:space:]]+")
          value);
      step = state: token:
        let
          attached =
            builtins.match "(-C=?|--codegen=)target-feature=(.*)" token;
          separate = builtins.match "target-feature=(.*)" token;
          featureValue = if state.expectCodegen && separate != null then
            builtins.elemAt separate 0
          else if attached != null then
            builtins.elemAt attached 1
          else
            null;
        in {
          expectCodegen = builtins.elem token [ "-C" "--codegen" ];
          features = state.features ++ (if featureValue == null then
            [ ]
          else
            builtins.filter builtins.isString
            (builtins.split "," featureValue));
        };
      parsed = builtins.foldl' step {
        expectCodegen = false;
        features = [ ];
      } tokens;
      forbidden =
        builtins.filter (feature: builtins.match "\\+(v|zv.*)" feature != null)
        parsed.features;
    in if forbidden == [ ] then
      value
    else
      throw "no-rvv-rust: ${
        lib.concatStringsSep ", " forbidden
      } is incompatible with Asterinas: the kernel does not save RVV state";
in import ./append-rustflags.nix {
  inherit lib hostPlatform flags validateFlags;
  marker = flags;
  requireTrailingMarker = true;
}
