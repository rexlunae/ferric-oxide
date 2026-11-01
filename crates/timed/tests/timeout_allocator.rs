//! Instrument the actual global allocator to catch calls made during expiry.
//!
//! A timer IRQ can interrupt an allocator lock holder. A zero-call expiry path
//! cannot re-enter that allocator, including when the final entry is removed.
//! Payloads here, like the kernel's timeout payload, have no allocating Drop.

#![cfg(feature = "alloc")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use ferric_timed::timeout::{Clock, ClockSnapshot, TimeoutQueue, Timestamp};

std::thread_local! {
    // Const TLS has no allocating initializer or destructor. Other test-harness
    // threads cannot contribute allocator calls to the measured thread.
    static CALLS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct ObservedAllocator;

fn record() {
    CALLS
        .try_with(|calls| {
            if let Some(count) = calls.get() {
                calls.set(Some(count + 1));
            }
        })
        .ok();
}

// SAFETY: every operation forwards the unchanged pointer and layout to System;
// recording uses only const-initialized thread-local integer storage.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: GlobalAlloc's caller supplied a valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: GlobalAlloc's caller supplied a valid layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record();
        // SAFETY: the pointer and matching layout come from this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        // SAFETY: GlobalAlloc's caller upholds its pointer, layout and size rules.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

fn observe<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    CALLS.with(|calls| calls.set(Some(0)));
    let result = operation();
    let count = CALLS.with(|calls| calls.replace(None).unwrap());
    (result, count)
}

#[test]
fn due_checks_expiry_and_cancellation_do_not_enter_the_allocator() {
    let allocation = std::hint::black_box(Box::new(42));
    let (_, calls) = observe(|| drop(allocation));
    assert_ne!(calls, 0, "allocator instrumentation must observe frees");
    let before = ClockSnapshot::new(Timestamp::new(0), Timestamp::new(0));
    let after = ClockSnapshot::new(Timestamp::new(10_000), Timestamp::new(10_000));
    for population in [1, 64, 4096] {
        let mut queue = TimeoutQueue::new();
        let mut tokens = Vec::new();
        for clock in [Clock::Monotonic, Clock::Realtime] {
            for n in 0..population {
                tokens.push(queue.insert(clock, Timestamp::new(n + 1), n).unwrap());
            }
        }
        let (not_due, calls) = observe(|| (queue.next_deadline(before), queue.pop_due(before)));
        assert_eq!(not_due, (Some(Timestamp::new(1)), None));
        assert_eq!(calls, 0, "an empty due check used the allocator");

        let (cancelled, calls) = observe(|| queue.cancel(tokens[0]));
        assert_eq!(cancelled, Some(0));
        assert_eq!(calls, 0, "cancellation used the allocator");

        let (expired, calls) = observe(|| {
            let mut expired = 0;
            while queue.pop_due(after).is_some() {
                expired += 1;
            }
            expired
        });
        assert_eq!(expired, 2 * population - 1);
        assert!(queue.is_empty());
        assert_eq!(
            calls, 0,
            "expiry, including the last node, used the allocator"
        );

        for clock in [Clock::Monotonic, Clock::Realtime] {
            queue.insert(clock, Timestamp::new(1), 42).unwrap();
        }
        let (cancelled, calls) = observe(|| queue.cancel_where(|&value| value == 42));
        assert_eq!(cancelled, 2);
        assert!(queue.is_empty());
        assert_eq!(calls, 0, "final-owner cancellation used the allocator");
    }
}
