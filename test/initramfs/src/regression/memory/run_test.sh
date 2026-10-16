#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

./mmap/mmap_and_fork
./mmap/mmap_and_mprotect
./mmap/mmap_and_mremap
./mmap/mmap_beyond_the_file
./mmap/mmap_err
./mmap/mmap_holes
./mmap/mmap_readahead
./mmap/mmap_shared_filebacked
./mmap/mmap_vmrss
./mmap/rev_map_ext2
./mmap/rev_map_tmpfs

# K1 MUSE Book --- (demand-paged GEM mmap; drm_dumb_hold keeps the BO alive)
# The only demand-paged device mapping in the tree is a DRM buffer object.
../device/drm/drm_dumb_hold ./mmap_device/mmap_on_demand_device
# K1 MUSE Book ---
