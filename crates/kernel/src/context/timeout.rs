//! Time-scheme timeout registration and event delivery.
//!
//! The host-tested `ferric_timed::timeout` queue keeps monotonic and realtime
//! dates separate and projects their earliest wakeup into monotonic time. This
//! is the time-scheme input to the Phase S deadline service; periodic interrupts
//! still deliver events until scheduler rearming is integrated. Event callbacks
//! run after releasing the registry lock, preserving the event/scheme lock order.
//! Final-close ownership/cancellation remains tracked by issue #362; introducing
//! queue cancellation tokens alone does not resolve in-flight registration races.

use ferric_timed::timeout::{Clock, ClockSnapshot, TimeoutQueue, Timestamp};

use crate::{
    event,
    scheme::SchemeId,
    sync::{CleanLockToken, Mutex, L1},
    syscall::{
        data::TimeSpec,
        error::{Error, Result, EINVAL, ENOMEM},
        flag::{CLOCK_MONOTONIC, CLOCK_REALTIME, EVENT_READ},
    },
    time,
};

#[derive(Debug)]
struct Timeout {
    scheme_id: SchemeId,
    event_id: usize,
}

static REGISTRY: Mutex<L1, TimeoutQueue<Timeout>> = Mutex::new(TimeoutQueue::new());

/// A snapshot of pending time-scheme events and their next monotonic deadline.
pub struct TimeoutStats {
    /// Number of registered events across both clocks.
    pub count: usize,
    /// Earliest projected monotonic nanosecond deadline, or no pending event.
    pub next_monotonic_ns: Option<u128>,
}

fn clocks(token: &mut CleanLockToken) -> ClockSnapshot {
    let offset = { *time::START.lock(token.token()) };
    let mono = time::monotonic(token);
    ClockSnapshot::new(
        Timestamp::new(mono),
        Timestamp::new(mono.saturating_add(offset)),
    )
}

/// Returns the registry count and next deadline using one clock snapshot.
pub fn stats(token: &mut CleanLockToken) -> TimeoutStats {
    let clocks = clocks(token);
    let registry = REGISTRY.lock(token.token());
    TimeoutStats {
        count: registry.len(),
        next_monotonic_ns: registry.next_deadline(clocks).map(Timestamp::get),
    }
}

/// Registers an absolute time-scheme event in its original clock domain.
///
/// # Errors
/// Returns `EINVAL` for an unsupported clock and `ENOMEM` if the registry's
/// non-recycling registration identities are exhausted. The caller validates
/// the supplied `TimeSpec` before converting it to an absolute timestamp.
pub fn register(
    scheme_id: SchemeId,
    event_id: usize,
    clock: usize,
    time: TimeSpec,
    token: &mut CleanLockToken,
) -> Result<()> {
    let clock = match clock {
        CLOCK_MONOTONIC => Clock::Monotonic,
        CLOCK_REALTIME => Clock::Realtime,
        _ => return Err(Error::new(EINVAL)),
    };
    REGISTRY
        .lock(token.token())
        .insert(
            clock,
            Timestamp::new(time.to_nanos()),
            Timeout {
                scheme_id,
                event_id,
            },
        )
        .map(|_| ())
        .map_err(|_| Error::new(ENOMEM))
}

/// Removes due registrations exactly once and delivers their read events.
///
/// Both clock domains use one snapshot. No registry or clock lock remains held
/// when entering event delivery, which may itself register or wake work.
pub fn trigger(token: &mut CleanLockToken) {
    let clocks = clocks(token);
    loop {
        let timeout = { REGISTRY.lock(token.token()).pop_due(clocks) };
        let Some(timeout) = timeout else {
            break;
        };
        event::trigger(timeout.scheme_id, timeout.event_id, EVENT_READ, token);
    }
}
