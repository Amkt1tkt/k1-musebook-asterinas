#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: test the actual patched subscription on a private native D-Bus.
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--vendor', type=Path, required=True)
    parser.add_argument('--work-dir', type=Path, required=True)
    parser.add_argument('--dbus-daemon', type=Path, required=True)
    parser.add_argument('--upstream', action='store_true', help='run against unpatched source as a regression control')
    parser.add_argument('--filter', default='')
    args = parser.parse_args()
    package = Path(__file__).resolve().parent.parent
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    vendor = work / 'vendor'
    if not vendor.exists():
        vendor.mkdir()
        for original in args.vendor.resolve(strict=True).iterdir():
            dest = vendor / original.name
            if original.name != 'cosmic-settings-subscriptions-0.1.0':
                dest.symlink_to(original)
    crate_copy = vendor / 'cosmic-settings-subscriptions-0.1.0'
    if crate_copy.exists():
        shutil.rmtree(crate_copy)
    shutil.copytree(args.vendor / crate_copy.name, crate_copy)
    subprocess.run([sys.executable, str(package / 'apply_vendor.py'),
                    '--vendor', str(vendor), '--patch', str(package / 'upower-initialization.patch')], check=True)
    fixture = work / ('upstream-fixture' if args.upstream else 'fixture')
    (fixture / 'src').mkdir(parents=True, exist_ok=True)
    (fixture / '.cargo').mkdir(exist_ok=True)
    shutil.copyfile(package / 'tests/Cargo.toml', fixture / 'Cargo.toml')
    shutil.copyfile(package / 'tests/subscription.rs', fixture / 'src/lib.rs')
    selected = vendor
    if args.upstream:
        selected = work / 'upstream-vendor'
        if not selected.exists():
            selected.mkdir()
            for original in args.vendor.resolve(strict=True).iterdir():
                (selected / original.name).symlink_to(original)
    (fixture / '.cargo/config.toml').write_text(
        (args.vendor / '.cargo/config.toml').read_text().replace('@vendor@', str(selected)))
    lockfile = fixture / 'Cargo.lock'
    shutil.copyfile(args.vendor / 'Cargo.lock', lockfile)
    # Prune the application workspace while keeping its exact vendored versions.
    subprocess.run(['cargo', 'metadata', '--offline', '--format-version=1'],
                   cwd=fixture, stdout=subprocess.DEVNULL, check=True)
    bus_config = work / 'bus.conf'
    bus_config.write_text('''<busconfig>
      <type>session</type><listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth>
      <policy context="default"><allow user="*"/><allow own="*"/>
        <allow send_destination="*"/><allow receive_sender="*"/></policy>
    </busconfig>''')
    bus = subprocess.Popen([str(args.dbus_daemon), '--config-file=' + str(bus_config), '--nofork', '--print-address=1'],
                           stdout=subprocess.PIPE, text=True)
    try:
        address = bus.stdout.readline().strip()
        assert address, 'Private D-Bus failed to start'
        env = os.environ.copy()
        env['DBUS_SYSTEM_BUS_ADDRESS'] = address
        subprocess.run(['cargo', 'test', '--offline', '--locked', '--target-dir', str(work / 'target'),
                        args.filter, '--', '--test-threads=1', '--nocapture'],
                       cwd=fixture, env=env, check=True)
    finally:
        bus.terminate()
        bus.wait(timeout=5)


if __name__ == '__main__':
    main()
