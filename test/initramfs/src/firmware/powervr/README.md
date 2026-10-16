<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: PowerVR firmware provenance. -->

# PowerVR firmware

This directory contains the unmodified firmware binary and original `LICENSE.powervr`
used before stage 1 switches to the root filesystem.
Redistribution is permitted for Imagination hardware subject to that license.
The firmware is not Rust source, and no source-license claim is made.

The source is Imagination `linux-firmware` commit `8a58f81883f7be458daa34e418cc4079f995b279`.
`distro/etc_nixos/packages/powervr-firmware/default.nix` fixes the same revision,
firmware and license hashes for the NixOS root filesystem.
These two copies must remain byte-identical to that package's inputs.

`rogue_36.29.52.182_v1.fw` targets the K1 GPU's BVNC `36.29.52.182`.
The driver checks the embedded device and protocol information;
do not substitute an image built for another BVNC.
