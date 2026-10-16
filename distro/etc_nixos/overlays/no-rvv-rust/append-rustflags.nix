# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: append flags to Cargo's highest-priority configured source.
# Encoded flags take precedence over RUSTFLAGS, which takes precedence over
# target flags. Preserve top-level versus env placement: nixpkgs rejects a
# name defined in both. These call sites supply space-separated rustc tokens.

{ lib, hostPlatform, flags, marker, requireTrailingMarker ? false
, validateFlags ? ({ isEncoded, value }: value) }:

let
  targetKey = "CARGO_TARGET_${hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS";
  encodedKey = "CARGO_ENCODED_RUSTFLAGS";
  separator = builtins.fromJSON ''"\u001f"'';
  asString = v: if builtins.isList v then lib.concatStringsSep " " v else v;

in old:

if !hostPlatform.isRiscV64 then
  { }
else
  let
    env = old.env or { };
    key = if (old ? ${encodedKey}) || (env ? ${encodedKey}) then
      encodedKey
    else if (old ? RUSTFLAGS) || (env ? RUSTFLAGS) then
      "RUSTFLAGS"
    else
      targetKey;
    encode = value:
      if key == encodedKey then
        lib.replaceStrings [ " " ] [ separator ] value
      else
        value;
    delimiter = if key == encodedKey then separator else " ";
    inTop = old ? ${key};
    prevFlags = validateFlags {
      isEncoded = key == encodedKey;
      value = asString (if inTop then old.${key} else (env.${key} or ""));
    };
    encodedMarker = encode marker;
    # Code-generation constraints must follow any package-owned overrides.
    alreadyPresent = if requireTrailingMarker then
      prevFlags == encodedMarker
      || lib.hasSuffix (delimiter + encodedMarker) prevFlags
    else
      lib.hasInfix encodedMarker prevFlags;
    merged = if alreadyPresent then
      prevFlags
    else
      lib.concatStringsSep delimiter
      (lib.filter (s: s != "") [ prevFlags (encode flags) ]);
  in if inTop then {
    ${key} = merged;
  } else {
    env = env // { ${key} = merged; };
  }
