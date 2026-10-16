# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: real Cargo inputs for vector pin precedence and ordering tests.

{ nixpkgsSource }:

let
  lib = import (builtins.toPath nixpkgsSource + "/lib");
  hostPlatform = {
    isRiscV64 = true;
    rust.cargoEnvVarTarget = "RISCV64GC_UNKNOWN_LINUX_GNU";
  };
  pin = import ../overlays/no-rvv-rust/pin.nix { inherit lib hostPlatform; };
  flags = import ../overlays/no-rvv-rust/flags.nix;
  separator = builtins.fromJSON ''"\u001f"'';
  encoded = lib.replaceStrings [ " " ] [ separator ] flags;
  key = "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_RUSTFLAGS";
  asEnv = attrs:
    lib.mapAttrs (_: value:
      if builtins.isList value then lib.concatStringsSep " " value else value)
    ((attrs.env or { }) // builtins.removeAttrs attrs [ "env" ]);
  pinned = name: expectReject: input:
    let
      result =
        builtins.tryEval (builtins.deepSeq (pin input) (input // pin input));
    in {
      inherit name expectReject;
      rejected = !result.success;
      expectVector = expectReject;
      env = asEnv (if result.success then result.value else input);
    };
  control = name: enable: {
    inherit name;
    expectReject = false;
    rejected = false;
    expectVector = true;
    env.RUSTFLAGS = "${flags} -C target-feature=+${enable}";
  };
in [
  (pinned "later-v" true { RUSTFLAGS = "${flags} -C target-feature=+v"; })
  (pinned "later-zve" true {
    env.${key} = "${flags} -Ctarget-feature=+zve32x";
  })
  (pinned "list-flags" true {
    RUSTFLAGS = [ flags "-C" "target-feature=+m,+zve32x" ];
  })
  (pinned "encoded-wins" true {
    env.CARGO_ENCODED_RUSTFLAGS =
      "${encoded}${separator}-C${separator}target-feature=+v";
    RUSTFLAGS = "-C target-feature=+v";
    ${key} = "-C target-feature=+zve32x";
  })
  (pinned "empty-encoded-wins" false {
    CARGO_ENCODED_RUSTFLAGS = "";
    RUSTFLAGS = "-C target-feature=+v";
  })
  (pinned "rustflags-wins" false {
    RUSTFLAGS = flags;
    ${key} = "-C target-feature=+zve32x";
  })
  (pinned "encoded-link-spaces" false {
    CARGO_ENCODED_RUSTFLAGS = "-Clink-args=foo target-feature=+v";
  })
  (pinned "encoded-separate-link-spaces" false {
    CARGO_ENCODED_RUSTFLAGS = "-C${separator}link-args=foo target-feature=+v";
  })
  (pinned "separate-long-codegen" true {
    RUSTFLAGS = "${flags} --codegen target-feature=+m,+zve32x";
  })
  (control "control-v" "v")
  (control "control-zve" "zve32x")
]
