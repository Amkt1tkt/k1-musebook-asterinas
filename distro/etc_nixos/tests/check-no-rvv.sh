#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# K1 MUSE Book: scan a store path for RVV opcodes (kernel has no VectorContext).
# Recurse on a directory, scan a file as-is.
#
# The kernel does not preserve vector state and leaves sstatus.VS Off.
# Executing userspace RVV instructions therefore traps as illegal.
#
# check-no-rvv.py checks OP-V and vector-width LOAD-FP/STORE-FP encodings.
# ISA attributes and disassembler mnemonics alone cannot establish absence.
# The scanner uses executable sections and uncovered executable-segment gaps;
# without section metadata it scans executable segments, allowing data hits.
#
# Missing tools must fail (exit 2). "Could not scan" is never a pass.
#
#   ./check-no-rvv.sh /nix/store/...-nixos-system-...
#   ./check-no-rvv.sh --self-test

set -euo pipefail

die() {
    echo "$0: $*" >&2
    exit 2
}

# Do not use `dirname`: with PATH emptied it is missing, and under
# `set -e` that dies before the "no python" message.
_self=$0
case $_self in
    /*) _dir=${_self%/*} ;;
    */*) _dir=$(pwd)/${_self%/*} ;;
    *) _dir=$(pwd) ;;
esac
here=$(CDPATH= cd -- "$_dir" && pwd) || die "cannot resolve script dir: $0"
scanner=$here/check-no-rvv.py

# An encoding scanner is required; attribute-only checks are insufficient.
if [ ! -f "$scanner" ]; then
    die "missing $scanner; encoding verification is unavailable"
fi

# Find python3 ourselves. macOS /usr/bin/python3 may be an Xcode stub.
find_python() {
    if [ -n "${PYTHON:-}" ]; then
        command -v "$PYTHON" > /dev/null 2>&1 || return 1
        echo "$PYTHON"
        return
    fi
    local c
    for c in python3 python3.12 python3.11 python3.10 python; do
        if command -v "$c" > /dev/null 2>&1; then
            echo "$c"
            return
        fi
    done
    return 1
}

# Cannot `die` inside `$(...)`: exit only kills the subshell.
if [ -n "${PYTHON:-}" ]; then
    PYTHON_BIN=$(find_python) || die "PYTHON=$PYTHON not found"
else
    PYTHON_BIN=$(find_python) || die "no python3; encoding scan needs it (missing tool = fail). install python3 or set PYTHON="
fi

if ! "$PYTHON_BIN" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 8) else 2)'; then
    die "$PYTHON_BIN is not Python >= 3.8 (or is an Xcode stub)"
fi

# objdump is optional. The .py mnemonic pass uses it when present.
find_objdump() {
    if [ -n "${OBJDUMP:-}" ]; then
        command -v "$OBJDUMP" > /dev/null 2>&1 || return 1
        echo "$OBJDUMP"
        return
    fi
    local c
    for c in \
        llvm-objdump \
        riscv64-unknown-linux-gnu-objdump \
        riscv64-linux-gnu-objdump \
        riscv64-unknown-linux-musl-objdump \
        riscv64-unknown-elf-objdump \
        riscv64-elf-objdump \
        objdump
    do
        if command -v "$c" > /dev/null 2>&1; then
            echo "$c"
            return
        fi
    done
    # Common in a nix profile; do not glob all of /nix/store.
    local d
    for d in \
        "${HOME:-}/.nix-profile/bin" \
        /nix/var/nix/profiles/default/bin
    do
        [ -d "$d" ] || continue
        for c in "$d"/llvm-objdump "$d"/riscv64-*-objdump; do
            if [ -x "$c" ]; then
                echo "$c"
                return
            fi
        done
    done
    return 1
}

OBJDUMP_BIN=""
if [ -n "${OBJDUMP:-}" ]; then
    OBJDUMP_BIN=$(find_objdump) || die "OBJDUMP=$OBJDUMP not found"
else
    if OBJDUMP_BIN=$(find_objdump); then
        :
    else
        echo "$0: no objdump (llvm-objdump / riscv64-*-objdump)." >&2
        echo "$0: encoding scan does not need it. set OBJDUMP= for the optional mnemonic pass." >&2
        OBJDUMP_BIN=""
    fi
fi

# READELF= is ignored; the .py reads .riscv.attributes itself.
if [ -n "${READELF:-}" ]; then
    echo "$0: READELF=$READELF is ignored (encoding scan)." >&2
fi

extra=()
if [ -n "$OBJDUMP_BIN" ]; then
    extra+=(--objdump "$OBJDUMP_BIN")
fi

exec "$PYTHON_BIN" "$scanner" "${extra[@]}" "$@"
