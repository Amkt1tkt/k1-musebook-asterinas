<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: precise software-frame damage and Wayland lifecycle contracts. -->

# Precise softbuffer damage

The pinned COSMIC Settings, Files, Edit and Terminal applications use this
softbuffer 0.4.1 adapter with tiny-skia software rendering. Wrappers set
`ICED_BACKEND=tiny-skia` as a default, preserving explicit environment overrides.
Other session clients keep their own renderer policy. The adapter compares
successfully presented pixel history to avoid redundant damage and buffer swaps.

## Integration

`apply_vendor.py` verifies the complete SHA256 of both original source files and
any existing checksum entries before patching a writable vendor copy. It changes
`src/lib.rs` and `src/wayland/mod.rs`, adds `src/k1_damage.rs`, and rebuilds Cargo's
complete per-file checksum manifest while retaining package metadata. All four
applications must use the same pinned source; a changed dependency is rejected.
`Buffer::present()` compares the complete frame. Explicit
`present_with_damage()` preserves the original presentation path and invalidates
this adapter's snapshot.

## Pixels and protocol

Comparison uses the safe back-buffer slice and an immutable snapshot of the last
successful frame. It never reads the compositor-owned front buffer, adds no
unsafe code, and compares all 32 pixel bits without hashing or widget heuristics.

| Condition | Presentation and state |
| --- | --- |
| First frame, unknown history, size mismatch, capacity overflow or allocation failure | Full damage, attach and commit; establish history only if a snapshot can be saved |
| Changed pixels | Damage the minimum bounding rectangle of all changes; save history after the swap |
| Identical pixels | Commit and flush without attach/swap/damage; both logical ages become one |
| Resize or explicit damage | Invalidate history and both ages; the caller must repair the entire reusable back buffer |
| Older Wayland surface protocol | Use whole-window damage for changed pixels |
| Fatal dispatch/flush error | Return an error and invalidate history and both ages |

Age describes successful logical pixel history. For A→B→A, damage is relative
to the current front B even if the reusable back already contains A. Identical
frames still commit pending frame callbacks and surface state. `WouldBlock`
leaves requests in the shared connection for the existing event loop to flush;
the adapter adds no busy wait.

A snapshot is limited to 4,194,304 pixels (16 MiB). During a changed frame, old
and new snapshots can total about 32 MiB per surface, in addition to upstream's
two SHM buffers. Identical frames still scan but do not copy. Large copies use
`try_reserve_exact`; small `Rc` allocations retain Rust's usual allocation policy.

## Verification

The independent board-maintenance checkout contains native regressions.
Run from the repository root with `K1_AUX` pointing to its `k1-musebook` directory
and `COSMIC_TERM_VENDOR` to this application's pinned Cargo vendor directory:

```sh
DAMAGE_DIR="$PWD/distro/etc_nixos/packages/k1-softbuffer-damage"
python3 "$K1_AUX/tests/test_softbuffer_damage.py" \
  --source "$DAMAGE_DIR/k1_damage.rs" --output-dir /tmp/k1-softbuffer-unit
python3 "$K1_AUX/tests/softbuffer-wayland/run.py" \
  --patch-dir "$DAMAGE_DIR" --vendor "$COSMIC_TERM_VENDOR" \
  --work-dir /tmp/k1-softbuffer-wayland
```

Use fresh output directories. The first fixture compiles the actual helper and
checks bounds, size/capacity/allocation fallback, A/B/A, repeated-frame pixels and
ages, failure and explicit invalidation. The protocol fixture compares original,
patched and deliberately missing-commit variants for continuous frame-callback
progress, recording input hashes and protocol requests.

These checks do not prove Asterinas/PowerVR hardware timing or final screen pixels.
They do not force socket backpressure or disconnection. At unchanged physical
size, scale/transform/opaque-region changes also depend on the pinned Smithay
metadata behavior. Board testing must include scaling, maximize, hide/restore,
pointer input and scrolling.
