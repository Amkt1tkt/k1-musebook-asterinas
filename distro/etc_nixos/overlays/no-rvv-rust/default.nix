# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: pin vector-free code generation for every RISC-V Rust package.
# The target includes activation tools as well as desktop applications.
# Keep nixpkgs' overridable functor shape so cross-platform splicing and
# buildRustPackage.override both preserve the pin. Scan final ELFs as well:
# explicit #[target_feature] functions are not disabled by these flags.

final: prev:

let
  lib = prev.lib;
  host = prev.stdenv.hostPlatform;

  # Single merge. A name written both at the top level and in `env`
  # trips nixpkgs' assertion at eval time — see `pin.nix`.
  pin = import ./pin.nix {
    inherit lib;
    hostPlatform = host;
  };

  # override recreates the functor, so wrap its result recursively.
  wrapBuildRustPackage = orig:
    let called = args: (orig args).overrideAttrs pin;
    in if builtins.isFunction orig then
      called
    else if lib.isAttrs orig && orig ? __functor then
      orig // {
        __functor = _self: called;
      } // lib.optionalAttrs (orig ? override) {
        override = newArgs: wrapBuildRustPackage (orig.override newArgs);
      }
    else
      throw ''
        no-rvv-rust: rustPlatform.buildRustPackage is neither a function
        nor a functor (did nixpkgs change makeOverridable?). Not wrapping
        would omit the RISC-V code-generation constraint.
      '';
in lib.optionalAttrs host.isRiscV64 {
  rustPlatform = prev.rustPlatform // {
    buildRustPackage = wrapBuildRustPackage prev.rustPlatform.buildRustPackage;
  };
}
