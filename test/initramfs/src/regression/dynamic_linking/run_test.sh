#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

# K1 MUSE Book: run the generic dynamic-linking tests (no drm_probe).

set -e

./dyn_link
./fpu_threads
