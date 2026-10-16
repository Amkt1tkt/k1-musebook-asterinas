# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: eval-time test that the no-rvv-rust overlay wraps buildRustPackage.
#
#     nix-instantiate --eval --strict --json tests/no-rvv-overlay-test.nix
#
# All `true` / expected values = pass. `buildRustPackage` is a
# makeOverridable functor, not a lambda; wrapping it as a lambda makes
# splice.nix throw `expected a set but found a function`. Stub `lib`
# so this runs without nixpkgs — shape and idempotence only.

let
  lib = {
    replaceStrings = builtins.replaceStrings;
    hasInfix = infix: s: builtins.match ".*${infix}.*" s != null;
    hasSuffix = suffix: s:
      let
        suffixLen = builtins.stringLength suffix;
        stringLen = builtins.stringLength s;
      in stringLen >= suffixLen
      && builtins.substring (stringLen - suffixLen) suffixLen s == suffix;
    concatStringsSep = builtins.concatStringsSep;
    filter = builtins.filter;
    isAttrs = builtins.isAttrs;
    optionalAttrs = c: a: if c then a else { };
  };

  # Shape makeOverridable produces for a package that returns a function.
  mkFunctor = tag: {
    __functionArgs = { pname = false; };
    override = _newArgs: mkFunctor "${tag}+overridden";
    __functor = _self: args: {
      inherit tag;
      pname = args.pname;
      # Record the patch overrideAttrs received, for the assertions.
      overrideAttrs = f: {
        inherit tag;
        patch = f { };
      };
    };
  };

  riscv = {
    isRiscV64 = true;
    rust.cargoEnvVarTarget = "RISCV64GC_UNKNOWN_LINUX_GNU";
  };
  x86 = {
    isRiscV64 = false;
    rust.cargoEnvVarTarget = "X86_64_UNKNOWN_LINUX_GNU";
  };

  mkPrev = hp: {
    inherit lib;
    stdenv.hostPlatform = hp;
    rustPlatform = {
      buildRustPackage = mkFunctor "base";
      # Sibling attrs on rustPlatform must be left alone.
      rust = { cargo = "cargo-drv"; };
    };
  };

  overlay = import ../overlays/no-rvv-rust/default.nix;
  pin = import ../overlays/no-rvv-rust/pin.nix {
    inherit lib;
    hostPlatform = riscv;
  };
  flags = import ../overlays/no-rvv-rust/flags.nix;
  key = "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_RUSTFLAGS";

  applied = overlay null (mkPrev riscv);
  b = applied.rustPlatform.buildRustPackage;
in {
  # Overlay shape: empty on non-riscv64; still an attrset (splice).
  notRiscvIsEmpty = overlay null (mkPrev x86) == { };
  stillAttrs = builtins.isAttrs b;
  hasFunctor = b ? __functor;
  functionArgsPreserved = b.__functionArgs == { pname = false; };
  siblingKept = applied.rustPlatform.rust.cargo == "cargo-drv";

  # The belt actually attaches; override must re-wrap.
  pinApplied = (b { pname = "demo"; }).patch == { env = { ${key} = flags; }; };
  pinSurvivesOverride = ((b.override { }) { pname = "demo"; }).patch == {
    env = { ${key} = flags; };
  };
  overrideReachesOrig = ((b.override { }) { pname = "demo"; }).tag
    == "base+overridden";

  # pin writes exactly one place and is idempotent.
  pinFreshWritesEnvOnly = pin { } == { env = { ${key} = flags; }; };
  pinTopLevelStaysTopLevel = pin { ${key} = "-C link-arg=-lEGL"; } == {
    ${key} = "-C link-arg=-lEGL ${flags}";
  };
  pinIsIdempotent = pin { env = { ${key} = flags; }; } == {
    env = { ${key} = flags; };
  };
  pinKeepsOtherEnv = pin { env = { CARGO_PROFILE_RELEASE_LTO = "thin"; }; } == {
    env = {
      CARGO_PROFILE_RELEASE_LTO = "thin";
      ${key} = flags;
    };
  };

  # Package-owned RUSTFLAGS (nixos-25.05 cosmic-edit). Cargo source 2
  # (`RUSTFLAGS`) present means source 3 (`CARGO_TARGET_*`) is ignored,
  # so the belt must append to RUSTFLAGS and must not also write 3.
  # cosmic-edit uses a list; the result is a string.
  pinFollowsTopLevelRustflagsList =
    pin { RUSTFLAGS = [ "-C link-arg=-lEGL" "-C link-arg=-lwayland-client" ]; }
    == {
      RUSTFLAGS = "-C link-arg=-lEGL -C link-arg=-lwayland-client ${flags}";
    };
  pinFollowsTopLevelRustflagsString = pin { RUSTFLAGS = "-C link-arg=-lEGL"; }
    == {
      RUSTFLAGS = "-C link-arg=-lEGL ${flags}";
    };
  pinFollowsEnvRustflags = pin { env = { RUSTFLAGS = "-C link-arg=-lEGL"; }; }
    == {
      env = { RUSTFLAGS = "-C link-arg=-lEGL ${flags}"; };
    };
  pinRustflagsIdempotent = pin { RUSTFLAGS = "-C link-arg=-lEGL ${flags}"; }
    == {
      RUSTFLAGS = "-C link-arg=-lEGL ${flags}";
    };
  # RUSTFLAGS wins over CARGO_TARGET_*: both present → only RUSTFLAGS moves.
  pinPrefersRustflagsOverTargetKey = pin {
    RUSTFLAGS = "-C link-arg=-lEGL";
    ${key} = "--cfg x";
  } == {
    RUSTFLAGS = "-C link-arg=-lEGL ${flags}";
  };

  # Force-link dependencies required by modules/cosmic-apps.nix.
  # Hook-style package (no RUSTFLAGS): link args land on the target key
  # after the vector pin. cosmic-edit style (top-level RUSTFLAGS already
  # has `-lwayland-client`): unchanged.
  forceLinkGoesToTargetKeyAfterPin = let
    link =
      "-C link-arg=-Wl,--push-state,--no-as-needed -C link-arg=-lwayland-client -C link-arg=-Wl,--pop-state";
    append = import ../overlays/no-rvv-rust/append-rustflags.nix {
      inherit lib;
      hostPlatform = riscv;
      flags = link;
      marker = "-lwayland-client";
    };
    before = { env = { ${key} = "${flags}"; }; };
  in append before == { env = { ${key} = "${flags} ${link}"; }; };
  forceLinkSkipsPackageThatAlreadyLinksWayland = let
    append = import ../overlays/no-rvv-rust/append-rustflags.nix {
      inherit lib;
      hostPlatform = riscv;
      flags = "-C link-arg=-lwayland-client";
      marker = "-lwayland-client";
    };
    edit = {
      RUSTFLAGS = "-C link-arg=-lEGL -C link-arg=-lwayland-client ${flags}";
    };
  in append edit == edit;

  pinCompletesVectorSubsets = pin { RUSTFLAGS = "-C target-feature=-v"; } == {
    RUSTFLAGS = "-C target-feature=-v ${flags}";
  };
  pinRejectsLaterVectorEnable = !(builtins.tryEval
    (pin { RUSTFLAGS = "${flags} -C target-feature=+v"; }).RUSTFLAGS).success;
  pinRejectsLaterSubsetEnable = !(builtins.tryEval (pin {
    env.${key} = "${flags} -Ctarget-feature=+zve32x";
  }).env.${key}).success;
  pinRejectsListVectorEnable = !(builtins.tryEval (pin {
    RUSTFLAGS = [ flags "-C" "target-feature=+m,+v" ];
  }).RUSTFLAGS).success;
  pinRejectsVectorLengthEnable = !(builtins.tryEval
    (pin { RUSTFLAGS = "-C=target-feature=+zvl128b"; }).RUSTFLAGS).success;
  pinRejectsLongCodegenVectorEnable = !(builtins.tryEval (pin {
    RUSTFLAGS = "--codegen=target-feature=+m,+zve64d";
  }).RUSTFLAGS).success;
  pinKeepsNonVectorPlusValues = pin { RUSTFLAGS = "-C link-arg=/tmp/+v"; } == {
    RUSTFLAGS = "-C link-arg=/tmp/+v ${flags}";
  };
  pinChecksOnlyEffectiveEncodedFlags = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
    input = {
      CARGO_ENCODED_RUSTFLAGS = encoded;
      RUSTFLAGS = "-C target-feature=+v";
    };
  in pin input == { CARGO_ENCODED_RUSTFLAGS = encoded; };
  pinReappliedAfterOtherFlagsIsIdempotent = let
    input = { RUSTFLAGS = "${flags} --cfg=demo"; };
    once = pin input;
  in pin once == once;
  pinRequiresTokenBoundary = pin { RUSTFLAGS = "--cfg=demo${flags}"; } == {
    RUSTFLAGS = "--cfg=demo${flags} ${flags}";
  };
  encodedPinRejectsLaterVectorEnable = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
    input = "${encoded}${sep}-C${sep}target-feature=+v";
  in !(builtins.tryEval (pin {
    env.CARGO_ENCODED_RUSTFLAGS = input;
  }).env.CARGO_ENCODED_RUSTFLAGS).success;
  pinRejectsSeparatedLongCodegenVectorEnable = !(builtins.tryEval
    (pin { RUSTFLAGS = "--codegen target-feature=+m,+v"; }).RUSTFLAGS).success;
  pinIgnoresBareFeatureValue = pin { RUSTFLAGS = "target-feature=+v"; } == {
    RUSTFLAGS = "target-feature=+v ${flags}";
  };
  pinPreservesEncodedLinkArgumentSpaces = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
    input = "-Clink-args=foo target-feature=+v";
  in pin { CARGO_ENCODED_RUSTFLAGS = input; } == {
    CARGO_ENCODED_RUSTFLAGS = "${input}${sep}${encoded}";
  };
  pinPreservesSeparatedEncodedLinkArgumentSpaces = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
    input = "-C${sep}link-args=foo target-feature=+v";
  in pin { CARGO_ENCODED_RUSTFLAGS = input; } == {
    CARGO_ENCODED_RUSTFLAGS = "${input}${sep}${encoded}";
  };
  pinRejectsVectorBitmanipExtension = !(builtins.tryEval
    (pin { RUSTFLAGS = "-Ctarget-feature=+m,+zvbb"; }).RUSTFLAGS).success;
  pinRejectsVectorFloatExtension = let sep = builtins.fromJSON ''"\u001f"'';
  in !(builtins.tryEval (pin {
    CARGO_ENCODED_RUSTFLAGS = "-C${sep}target-feature=+zvfh";
  }).CARGO_ENCODED_RUSTFLAGS).success;
  pinAllowsScalarBitmanipExtension =
    pin { RUSTFLAGS = "-Ctarget-feature=+zbb"; } == {
      RUSTFLAGS = "-Ctarget-feature=+zbb ${flags}";
    };
  pinChecksOnlyEffectiveEncodedVectorFamilies = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
  in pin {
    CARGO_ENCODED_RUSTFLAGS = encoded;
    RUSTFLAGS = "-Ctarget-feature=+zvbb";
    env.${key} = "-Ctarget-feature=+zvfh";
  } == {
    CARGO_ENCODED_RUSTFLAGS = encoded;
  };
  pinChecksOnlyEffectiveRustflagsVectorFamilies = pin {
    RUSTFLAGS = flags;
    env.${key} = "-Ctarget-feature=+zvbb,+zvfh";
  } == {
    RUSTFLAGS = flags;
  };
  encodedTakesPrecedence = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
  in pin {
    CARGO_ENCODED_RUSTFLAGS = "--cfg${sep}demo";
    RUSTFLAGS = "--cfg ignored";
  } == {
    CARGO_ENCODED_RUSTFLAGS = "--cfg${sep}demo${sep}${encoded}";
  };
  encodedEnvIsIdempotent = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
    original = {
      env = {
        CARGO_ENCODED_RUSTFLAGS = encoded;
        OTHER = "keep";
      };
    };
  in pin original == original;
  emptyEncodedStillTakesPrecedence = let
    sep = builtins.fromJSON ''"\u001f"'';
    encoded = builtins.replaceStrings [ " " ] [ sep ] flags;
  in pin {
    env = { CARGO_ENCODED_RUSTFLAGS = ""; };
    RUSTFLAGS = "ignored";
  } == {
    env = { CARGO_ENCODED_RUSTFLAGS = encoded; };
  };
}
