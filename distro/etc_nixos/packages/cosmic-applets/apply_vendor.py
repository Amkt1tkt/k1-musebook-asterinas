#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: apply the battery subscription fix to the pinned writable vendor.
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

BASE = {
    'Cargo.toml': '486f4cdaf1852fd7400c3aea410cce509176f13befcf9b0d5c4a3a51db7cf621',
    'src/upower/device.rs': '91571522e6872a45cf68e09a5aeb8f394e6ae479b7e3cc51af92a56bc188a5e6',
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
    args = parser.parse_args()
    crate = args.vendor.resolve(strict=True) / 'cosmic-settings-subscriptions-0.1.0'
    require(crate.is_dir(), f'Pinned subscription crate is missing: {crate}')
    require(not crate.resolve().is_relative_to('/nix/store'), 'Patch the build copy, never the store source')
    checksum_path = crate / '.cargo-checksum.json'
    # A copied directory may still contain source-file symlinks into the store.
    for relative in (*BASE, '.cargo-checksum.json'):
        resolved = (crate / relative).resolve(strict=True)
        require(resolved.is_relative_to(crate.resolve()) and not resolved.is_relative_to('/nix/store'),
                f'Expected writable crate-local file: {relative}')
    checksums = json.loads(checksum_path.read_text())
    for relative, expected in BASE.items():
        require(digest(crate / relative) == expected, f'Unexpected upstream source: {relative}')
        if checksums['files']:
            require(checksums['files'].get(relative) == expected, f'Unexpected vendor checksum: {relative}')
    before = {str(p.relative_to(crate)): digest(p) for p in crate.rglob('*') if p.is_file()}
    for relative, expected in checksums['files'].items():
        require(before.get(relative) == expected, f'Unexpected vendor checksum: {relative}')
    subprocess.run(['patch', '--batch', '--forward', '--fuzz=0', '-p1', '-i', str(args.patch.resolve())], cwd=crate, check=True)
    after = {str(p.relative_to(crate)): digest(p) for p in crate.rglob('*') if p.is_file()}
    changed = {name for name in before.keys() | after.keys() if before.get(name) != after.get(name)}
    require(changed == set(BASE), f'Unexpected patch changes: {changed ^ set(BASE)}')
    # Keep the original package metadata and enable checksums for every source file.
    checksums['files'] = {name: value for name, value in after.items() if name != '.cargo-checksum.json'}
    checksum_path.write_text(json.dumps(checksums, sort_keys=True) + '\n')
    for relative, expected in checksums['files'].items():
        require(digest(crate / relative) == expected, f'Post-patch checksum mismatch: {relative}')
    print('K1 UPower subscription vendor verified:', crate)


if __name__ == '__main__':
    main()
