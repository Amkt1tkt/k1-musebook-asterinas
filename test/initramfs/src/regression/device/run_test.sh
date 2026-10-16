#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

# K1 MUSE Book --- (KMS capabilities, GEM mapping, and authentication differ between DRM registries)
# ./drm/device_node
# ./drm/gem_create_dumb_buffer
# ./drm/get_info
# ./drm/kms_properties
# ./drm/kms_resources
# ./drm/master
./drm/device_node
./drm/get_info
./drm/kms_properties
./drm/kms_resources
case "$(uname -m)" in
    riscv64)
        # K1 supports atomic KMS and private GEM mappings, but not AUTH_MAGIC.
        ./drm/drm_smoke
        ./drm/kms_atomic
        ./dri_sysfs.sh
        ;;
    *)
        ./drm/gem_create_dumb_buffer
        ./drm/master
        ;;
esac
# K1 MUSE Book ---

./pty/close_pty
./pty/open_ptmx
./pty/open_pty
./pty/open_pty_peer
./pty/pty_blocking
./pty/pty_packet_mode
./pty/signal_char
./pty/termios2

./vt/vt_ioctl

./devtmpfs_mode
./evdev
./framebuffer
./full
./hwrng
./nvme
./random
