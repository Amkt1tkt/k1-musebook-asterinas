#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: patch an exact writable Cargo vendor copy without disabling checksums.
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

BASE = {
    'src/lib.rs': '9db7e3ad1b119b9e59b51e9f58d6e5610862b46e2eca8a174056029394bd01ce',
    'src/wayland/mod.rs': '79db632c1890c89821f3e4d68efb9e8412a36974e0450e0f75682eac018e6103',
}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--vendor', type=Path, required=True)
    parser.add_argument('--patch', type=Path, required=True)
    parser.add_argument('--helper', type=Path, required=True)
    args = parser.parse_args()
    crate = args.vendor.resolve(strict=True) / 'softbuffer-0.4.1'
    require(crate.is_dir(), f'Pinned softbuffer crate is missing: {crate}')
    require(not crate.resolve().is_relative_to('/nix/store'), 'Patch the build copy, never the store source')
    checksum_path = crate / '.cargo-checksum.json'
    # A copied directory may still contain source-file symlinks into the store.
    for relative in (*BASE, '.cargo-checksum.json'):
        resolved = (crate / relative).resolve(strict=True)
        require(resolved.is_relative_to(crate.resolve()) and not resolved.is_relative_to('/nix/store'),
                f'Expected writable crate-local file: {relative}')
    helper_target = crate / 'src/k1_damage.rs'
    require(not helper_target.exists() and not helper_target.is_symlink(),
            'The added helper path must not already exist')
    checksums = json.loads(checksum_path.read_text())
    for relative, expected in BASE.items():
        require(digest(crate / relative) == expected, f'Unexpected upstream source: {relative}')
        # fetchCargoVendor emits an empty map for this pinned git dependency.
        # If a per-file manifest exists, require it to agree with the source.
        if checksums['files']:
            require(checksums['files'].get(relative) == expected, f'Unexpected vendor checksum: {relative}')
    before = {str(p.relative_to(crate)): digest(p) for p in crate.rglob('*') if p.is_file()}
    for relative, expected in checksums['files'].items():
        require(before.get(relative) == expected, f'Unexpected vendor checksum: {relative}')
    subprocess.run(['patch', '--batch', '--forward', '--fuzz=0', '-p1', '-i', str(args.patch.resolve())], cwd=crate, check=True)
    helper_target.write_bytes(args.helper.read_bytes())
    after = {str(p.relative_to(crate)): digest(p) for p in crate.rglob('*') if p.is_file()}
    changed = {name for name in before.keys() | after.keys() if before.get(name) != after.get(name)}
    expected_changes = set(BASE) | {'src/k1_damage.rs'}
    require(changed == expected_changes, f'Unexpected patch changes: {changed ^ expected_changes}')
    # Retain any package metadata and install a complete source manifest even
    # when the original git dependency had no individual file checksums.
    checksums['files'] = {name: value for name, value in after.items() if name != '.cargo-checksum.json'}
    checksum_path.write_text(json.dumps(checksums, sort_keys=True) + '\n')
    for relative, expected in checksums['files'].items():
        require(digest(crate / relative) == expected, f'Post-patch checksum mismatch: {relative}')
    print('K1 softbuffer vendor verified:', crate)
    for relative in sorted(expected_changes):
        print(after[relative], relative)


if __name__ == '__main__':
    main()
