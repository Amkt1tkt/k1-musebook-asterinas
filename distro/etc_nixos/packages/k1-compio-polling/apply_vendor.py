#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: select polling in an exact writable compio vendor copy.
import argparse
import hashlib
import json
from pathlib import Path

MANIFEST_SHA = '1e9ece8bd984a56561769a26d92bc12c66935d5eb01631b015aa499777e70d5a'
# Removing the unused io-uring dependency changes the lock's graph, but no
# version, source, or checksum of the remaining packages may change.
LOCK_HASHES = {
    '0210a1492ddfcc2cb9580b1845fe63b021759530fe43bca2dafb088c38681e63':
        '49796b73d449c5312eba35a85a928d3bd2bb132aac243009f5e83b62ca6db49a',
    'ae4255fb586b2b62e3db06fbf39b8e41ce99a39dcf4ea18c302296cb90acea1f':
        'ad47b91ad616bf32a09d916feef9245fd92a7e21a527f4b3a73e7d4a7f3e9133',
    '14fb9abd8e9c96cc7037c2bb34c0a833401e723712f328f111e97d8929ea11c2':
        '3028b2c2a56b94e4b01a1ef6d8380e2204c0640a04a71c355a059544e175a362',
}
OLD_DEFAULT = b'default = [\n    "runtime",\n    "io-uring",\n]\n'
NEW_DEFAULT = b'''# K1 MUSE Book --- (Asterinas does not provide io_uring; use the polling backend)
# default = [
#     "runtime",
#     "io-uring",
# ]
default = [
    "runtime",
    "polling",
]
# K1 MUSE Book ---
'''
IO_URING_PACKAGE = b'''[[package]]
name = "io-uring"
version = "0.7.4"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ab01638bb6a279897b7691f87f3f3c232451711fd419a69ced980ce61074fa46"
dependencies = [
 "bitflags 2.9.0",
 "cfg-if",
 "libc",
]

'''


def digest(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--vendor', type=Path, required=True)
    parser.add_argument('--source', type=Path, required=True)
    args = parser.parse_args()
    crate = args.vendor.resolve(strict=True) / 'compio-0.14.0'
    manifest = crate / 'Cargo.toml'
    checksum_path = crate / '.cargo-checksum.json'
    source_lock = args.source.resolve(strict=True) / 'Cargo.lock'
    vendor_lock = args.vendor.resolve(strict=True) / 'Cargo.lock'
    for path in (manifest, checksum_path, source_lock, vendor_lock):
        require(not path.resolve(strict=True).is_relative_to('/nix/store'),
                f'Patch writable build copies, never a store source: {path}')

    original_manifest = manifest.read_bytes()
    original_lock = source_lock.read_bytes()
    require(digest(original_manifest) == MANIFEST_SHA, 'Unexpected compio Cargo.toml')
    require(original_manifest.count(OLD_DEFAULT) == 1, 'Unexpected compio default features')
    require(vendor_lock.read_bytes() == original_lock, 'Source and vendor Cargo.lock differ')
    expected_lock = LOCK_HASHES.get(digest(original_lock))
    require(expected_lock is not None, 'Unexpected COSMIC Cargo.lock')
    require(original_lock.count(b' "io-uring",\n') == 1, 'Unexpected io-uring dependency edges')
    require(original_lock.count(IO_URING_PACKAGE) == 1, 'Unexpected io-uring lock entry')
    new_lock = original_lock.replace(b' "io-uring",\n', b'').replace(IO_URING_PACKAGE, b'')
    require(digest(new_lock) == expected_lock, 'Unexpected polling lock graph')
    new_manifest = original_manifest.replace(OLD_DEFAULT, NEW_DEFAULT)

    checksums = json.loads(checksum_path.read_text())
    files = {str(path.relative_to(crate)): digest(path.read_bytes())
             for path in crate.rglob('*') if path.is_file() and path != checksum_path}
    # fetchCargoVendor may emit an empty map. Check every listed input when
    # present, then retain the package checksum and install a complete map.
    for name, expected in checksums['files'].items():
        require(files.get(name) == expected, f'Unexpected vendor checksum: {name}')
    checksums['files'] = files | {'Cargo.toml': digest(new_manifest)}
    new_checksums = (json.dumps(checksums, sort_keys=True) + '\n').encode()

    # Prepare and validate all inputs before modifying any build file.
    for path, contents in ((manifest, new_manifest), (checksum_path, new_checksums),
                           (source_lock, new_lock), (vendor_lock, new_lock)):
        path.write_bytes(contents)
    print('K1 compio polling vendor verified:', crate)


if __name__ == '__main__':
    main()
