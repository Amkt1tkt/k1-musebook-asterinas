#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: compile the actual vector pin and check RISC-V instruction bytes.
"""Run with native Cargo and a rustc wrapper providing the RISC-V standard library.

Pass --nixpkgs-source to select the same source tree as the image. Use --cargo
and --rustc for its cross-build tools; --work-dir retains logs and artifacts.
The positive controls require actual vector instructions, so an ineffective
compiler fixture cannot make the pin tests pass.
"""

import argparse
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile


def checked(command, *, cwd=None, env=None, log=None):
    result = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True)
    if log is not None:
        log.write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        raise RuntimeError(f"command failed: {command!r}\n{result.stdout}{result.stderr}")
    return result.stdout


def run(args, work):
    here = Path(__file__).resolve().parent
    cases = json.loads(checked([
        args.nix_instantiate, "--eval", "--strict", "--json",
        str(here / "no-rvv-codegen.nix"),
        "--argstr", "nixpkgsSource", str(Path(args.nixpkgs_source).resolve()),
    ]))
    scan_file = runpy.run_path(str(here / "check-no-rvv.py"))["scan_path_file"]
    work.mkdir(parents=True, exist_ok=True)
    (work / "Cargo.toml").write_text(
        '[package]\nname = "vector-pin-test"\nversion = "0.1.0"\nedition = "2021"\n'
        '[lib]\npath = "vector.rs"\n[workspace]\n'
    )
    (work / "Cargo.lock").write_text(
        'version = 3\n[[package]]\nname = "vector-pin-test"\nversion = "0.1.0"\n'
    )
    (work / "vector.rs").write_text(
        '#![no_std]\n#[no_mangle]\n'
        'pub fn vector_candidate(a: &mut [u32], b: &[u32]) {\n'
        '    for (a, b) in a.iter_mut().zip(b.iter()) { *a = a.wrapping_add(*b); }\n'
        '}\n'
    )
    versions = {tool: checked([path, "--version"]).strip()
                for tool, path in [("cargo", args.cargo), ("rustc", args.rustc)]}
    results = []
    for case in cases:
        if case["rejected"] != case["expectReject"]:
            raise RuntimeError(f"{case['name']}: unexpected Nix flag validation result")
        target_dir = work / "target" / case["name"]
        env = os.environ.copy()
        for key in list(env):
            if key.endswith("_RUSTFLAGS") or key in ["RUSTFLAGS", "NIX_RUSTFLAGS"]:
                env.pop(key)
        env.update(case["env"])
        env.update(RUSTC=args.rustc, CARGO_HOME=str(work / "cargo-home"),
                   CARGO_TARGET_DIR=str(target_dir))
        checked([args.cargo, "build", "--lib", "--release", "--locked", "--offline",
                 "--target", "riscv64gc-unknown-linux-gnu"],
                cwd=work, env=env, log=work / (case["name"] + ".log"))
        artifact = target_dir / "riscv64gc-unknown-linux-gnu/release/libvector_pin_test.rlib"
        scanned = scan_file(str(artifact))
        if scanned.error or not scanned.scanned:
            raise RuntimeError(f"{case['name']}: invalid RISC-V artifact: {scanned.error}")
        has_vector = bool(scanned.insn_hits)
        if has_vector != case["expectVector"] or (not has_vector and scanned.attr_hits):
            raise RuntimeError(f"{case['name']}: unexpected vector instructions or ISA")
        results.append({"name": case["name"], "rejected": case["rejected"],
                        "vectorInstructions": len(scanned.insn_hits)})
    (work / "results.json").write_text(json.dumps({"versions": versions, "cases": results}, indent=2) + "\n")
    print(f"PASS: {len(results)} real Cargo vector pin cases")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nixpkgs-source", required=True)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--rustc", default="rustc")
    parser.add_argument("--nix-instantiate", default="nix-instantiate")
    parser.add_argument("--work-dir", type=Path)
    args = parser.parse_args()
    if args.work_dir is not None:
        run(args, args.work_dir.resolve())
    else:
        with tempfile.TemporaryDirectory(prefix="k1-vector-pin-") as directory:
            run(args, Path(directory))


if __name__ == "__main__":
    main()
