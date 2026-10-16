// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: exercise wake_all batch accounting with real OSTD waiters.

use super::*;
use crate::prelude::*;

fn assert_pending(queue: &WaitQueue, expected: usize) {
    let wakers = queue.wakers.lock();
    assert_eq!(wakers.len(), expected);
    assert_eq!(queue.num_wakers.load(Ordering::Relaxed) as usize, expected);
}

fn empty_and_closed_entries() {
    let queue = WaitQueue::new();
    assert_pending(&queue, 0);
    assert_eq!(queue.wake_all(), 0);
    assert!(!queue.wake_one());

    let (waiter, stale) = Waiter::new_pair();
    drop(waiter);
    queue.enqueue(stale.clone());
    queue.enqueue(stale);
    assert_pending(&queue, 2);
    assert_eq!(queue.wake_all(), 0);
    assert_pending(&queue, 0);
    assert!(!queue.wake_one());
}

fn unique_wakes_and_later_notifications() {
    let queue = WaitQueue::new();
    let (first, first_waker) = Waiter::new_pair();
    let (already_woken, already_waker) = Waiter::new_pair();
    let (closed, closed_waker) = Waiter::new_pair();
    let (second, second_waker) = Waiter::new_pair();
    assert!(already_waker.wake_up());
    drop(closed);

    for waker in [
        first_waker.clone(),
        first_waker.clone(),
        already_waker,
        closed_waker,
        second_waker.clone(),
    ] {
        queue.enqueue(waker);
    }
    assert_pending(&queue, 5);
    assert_eq!(queue.wake_all(), 2);
    assert_pending(&queue, 0);
    first.wait();
    already_woken.wait();
    second.wait();

    // Reusing the same waiters after consuming their wake must not lose a
    // later notification or leave the lock-free count incorrectly empty.
    for _ in 0..64 {
        queue.enqueue(first_waker.clone());
        queue.enqueue(second_waker.clone());
        assert_pending(&queue, 2);
        assert_eq!(queue.wake_all(), 2);
        assert_pending(&queue, 0);
        first.wait();
        second.wait();
    }
}

fn wake_one_skips_stale_and_duplicate_entries() {
    let queue = WaitQueue::new();
    let (closed, stale) = Waiter::new_pair();
    let (first, first_waker) = Waiter::new_pair();
    let (second, second_waker) = Waiter::new_pair();
    drop(closed);
    for waker in [
        stale,
        first_waker.clone(),
        first_waker.clone(),
        second_waker,
    ] {
        queue.enqueue(waker);
    }
    assert_pending(&queue, 4);
    assert!(queue.wake_one());
    assert_pending(&queue, 2);
    assert!(queue.wake_one());
    assert_pending(&queue, 0);
    first.wait();
    second.wait();

    queue.enqueue(first_waker);
    assert_eq!(queue.wake_all(), 1);
    assert_pending(&queue, 0);
    first.wait();
    assert!(!queue.wake_one());
}

#[ktest]
fn wake_all_batch_regression_suite() {
    empty_and_closed_entries();
    unique_wakes_and_later_notifications();
    wake_one_skips_stale_and_duplicate_entries();
}
