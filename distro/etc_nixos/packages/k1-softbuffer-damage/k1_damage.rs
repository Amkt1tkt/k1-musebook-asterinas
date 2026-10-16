// SPDX-License-Identifier: MIT OR Apache-2.0
// K1 MUSE Book: exact software-frame damage with bounded snapshots and logical ages.
// Compare the successful front frame so cursor-only redraws need no new attachment.

use std::rc::Rc;

pub(crate) const MAX_PIXELS: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Damage {
    Full,
    Partial(Bounds),
    Unchanged,
}

#[derive(Debug)]
pub(crate) struct Snapshot {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

pub(crate) struct Prepared {
    pub damage: Damage,
    pub next: Option<Rc<Snapshot>>,
}

// The callback permits deterministic allocation-failure testing without replacing
// the production comparison, bounds checks, or state-transition decisions.
fn prepare_with(
    pixels: &[u32],
    width: u32,
    height: u32,
    previous: Option<&Snapshot>,
    copy: impl FnOnce(&[u32]) -> Option<Vec<u32>>,
) -> Prepared {
    let valid_size = (width as usize).checked_mul(height as usize);
    if width == 0 || height == 0 || valid_size != Some(pixels.len()) || pixels.len() > MAX_PIXELS {
        return Prepared {
            damage: Damage::Full,
            next: None,
        };
    }
    let previous = previous.filter(|old| {
        old.width == width && old.height == height && old.pixels.len() == pixels.len()
    });
    let mut damage = Damage::Full;
    if let Some(old) = previous {
        let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
        for (y, (row, before)) in pixels
            .chunks_exact(width as usize)
            .zip(old.pixels.chunks_exact(width as usize))
            .enumerate()
        {
            for (x, (&pixel, &before)) in row.iter().zip(before).enumerate() {
                if pixel != before {
                    let (x, y) = (x as u32, y as u32);
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x + 1);
                    bottom = bottom.max(y + 1);
                }
            }
        }
        if right == 0 {
            return Prepared {
                damage: Damage::Unchanged,
                next: None,
            };
        }
        damage = if left == 0 && top == 0 && right == width && bottom == height {
            Damage::Full
        } else {
            Damage::Partial(Bounds {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            })
        };
    }
    let next = copy(pixels).map(|pixels| {
        Rc::new(Snapshot {
            width,
            height,
            pixels,
        })
    });
    if next.is_none() {
        damage = Damage::Full;
    }
    Prepared { damage, next }
}

pub(crate) fn prepare(
    pixels: &[u32],
    width: u32,
    height: u32,
    previous: Option<&Snapshot>,
) -> Prepared {
    prepare_with(pixels, width, height, previous, |pixels| {
        let mut copy = Vec::new();
        copy.try_reserve_exact(pixels.len()).ok()?;
        copy.extend_from_slice(pixels);
        Some(copy)
    })
}

// Logical age describes pixel contents, not how many wl_surface.attach requests
// occurred. Both buffers equal the newest successful frame after an unchanged
// commit. On an actual swap, the other buffer contains the previous logical frame.
pub(crate) fn unchanged_ages(front: &mut u8, back: &mut u8) {
    *front = 1;
    *back = 1;
}

pub(crate) fn swapped_ages(front: &mut u8, back: &mut u8) {
    *front = 1;
    if *back != 0 {
        *back = back.checked_add(1).unwrap_or(0);
    }
}

#[derive(Default)]
pub(crate) struct State {
    snapshot: Option<Rc<Snapshot>>,
}

impl State {
    pub fn snapshot(&self) -> Option<Rc<Snapshot>> {
        self.snapshot.clone()
    }

    pub fn invalidate(&mut self) {
        self.snapshot = None;
    }

    pub fn finish(&mut self, prepared: Prepared, succeeded: bool) {
        if !succeeded {
            self.invalidate();
        } else if prepared.damage != Damage::Unchanged {
            self.snapshot = prepared.next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bounds_and_alpha_are_not_hashed() {
        let initial = prepare(&[0; 12], 4, 3, None).next.unwrap();
        let mut pixels = vec![0; 12];
        pixels[5] = 1;
        pixels[11] = 0xff000000;
        let changed = prepare(&pixels, 4, 3, Some(&initial));
        assert_eq!(
            changed.damage,
            Damage::Partial(Bounds {
                x: 1,
                y: 1,
                width: 3,
                height: 2
            })
        );
        let next = changed.next.unwrap();
        assert_eq!(
            prepare(&pixels, 4, 3, Some(&next)).damage,
            Damage::Unchanged
        );
    }

    #[test]
    fn bounded_allocation_resize_and_failure_fall_back() {
        let old = prepare(&[1, 2, 3, 4], 2, 2, None).next.unwrap();
        assert_eq!(
            prepare(&[1, 2, 3, 4], 4, 1, Some(&old)).damage,
            Damage::Full
        );
        assert!(prepare(&[0; 3], 2, 2, Some(&old)).next.is_none());
        assert!(prepare(&[], u32::MAX, u32::MAX, None).next.is_none());
        assert!(
            prepare(&vec![0; MAX_PIXELS + 1], (MAX_PIXELS + 1) as u32, 1, None)
                .next
                .is_none()
        );
        let failed = prepare_with(&[1, 9, 3, 4], 2, 2, Some(&old), |_| None);
        assert_eq!(failed.damage, Damage::Full);
        assert!(failed.next.is_none());
        let unchanged = prepare_with(&[1, 2, 3, 4], 2, 2, Some(&old), |_| {
            panic!("unchanged must not allocate")
        });
        assert_eq!(unchanged.damage, Damage::Unchanged);
    }

    #[test]
    fn logical_ages_track_real_pixels_across_repeats_and_a_b_a() {
        let (mut front, mut back) = (vec![0; 4], vec![0; 4]);
        let (mut front_age, mut back_age) = (0, 0);
        let mut snapshot: Option<Rc<Snapshot>> = None;
        let mut history: Vec<Vec<u32>> = Vec::new();
        let mut attached = Vec::new();
        for &value in &[1, 1, 1, 2, 2, 2, 1, 1, 3, 3, 1] {
            if back_age != 0 {
                assert_eq!(
                    &back,
                    &history[back_age as usize - 1],
                    "age must identify the actual reusable pixels"
                );
            }
            let wanted = vec![value; 4];
            // Model the caller's age-based repair; do not assume a full redraw.
            if back_age == 0 {
                back.copy_from_slice(&wanted);
            } else {
                for (i, pixel) in back.iter_mut().enumerate() {
                    if history[back_age as usize - 1][i] != wanted[i] {
                        *pixel = wanted[i];
                    }
                }
            }
            let prepared = prepare(&back, 2, 2, snapshot.as_deref());
            if prepared.damage == Damage::Unchanged {
                assert_eq!(front, wanted);
                unchanged_ages(&mut front_age, &mut back_age);
            } else {
                std::mem::swap(&mut front, &mut back);
                std::mem::swap(&mut front_age, &mut back_age);
                swapped_ages(&mut front_age, &mut back_age);
                snapshot = prepared.next;
                attached.push(value);
            }
            assert_eq!(front, wanted);
            history.insert(0, wanted);
        }
        assert_eq!(attached, [1, 2, 1, 3, 1]);
    }

    #[test]
    fn damage_compares_with_front_not_the_reused_back_buffer() {
        let a = prepare(&[1, 1], 2, 1, None).next.unwrap();
        let b = prepare(&[1, 2], 2, 1, Some(&a)).next.unwrap();
        assert_eq!(
            prepare(&[1, 1], 2, 1, Some(&b)).damage,
            Damage::Partial(Bounds {
                x: 1,
                y: 0,
                width: 1,
                height: 1
            })
        );
    }
    #[test]
    fn failed_present_and_explicit_invalidation_drop_snapshot() {
        let mut state = State::default();
        state.finish(prepare(&[1, 2], 2, 1, None), true);
        assert!(state.snapshot.is_some());
        let prepared = prepare(&[1, 2], 2, 1, state.snapshot.as_deref());
        state.finish(prepared, false);
        assert!(state.snapshot.is_none());
        state.finish(prepare(&[1, 2], 2, 1, None), true);
        state.invalidate();
        assert!(state.snapshot.is_none());
    }
}
