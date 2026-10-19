//! The need-wakeup protocol: how a side that wants to sleep and a side that publishes
//! entries agree on whether a wakeup is needed, without a system call while both are awake.
//!
//! Every notify mode uses it. A consumer that finds its queue empty and wants to sleep calls
//! [`Consumer::prepare_sleep`]: it sets [`NEED_WAKEUP`] in its flags word, issues a full fence,
//! and looks at the producer's tail again. Only if the queue is still empty may it sleep
//! ([`Sleep::Sleep`]); otherwise the flag is cleared and it carries on ([`Sleep::Retry`]). A
//! producer publishes its tail and then calls [`Producer::needs_wakeup`], which issues a full
//! fence and reads the consumer's flag: only if it is set does the producer wake the consumer
//! (an `Enter`, an event, a store to a `UMWAIT` doorbell).
//!
//! Because each side stores before it fences and loads after, at least one of them sees the
//! other's store: either the consumer sees the new tail and does not sleep, or the producer
//! sees the flag and wakes it. No wakeup is lost. A woken consumer calls [`Consumer::woken`].
//!
//! The flags word belongs to the consumer. A producer only reads it, so a producer that
//! scribbles on it can at worst cause an extra or a missing wakeup of its own requests.

use core::sync::atomic::{Ordering, fence};

use crate::entry::Entry;
use crate::queue::{Consumer, Memory, Producer, RingError, word};

/// The consumer's flag: it is about to sleep, or sleeping, and must be woken for new entries.
pub const NEED_WAKEUP: u64 = 1 << 0;

/// What a consumer that wants to sleep should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sleep {
    /// Entries arrived meanwhile: take them and do not sleep.
    Retry,
    /// The queue is empty and [`NEED_WAKEUP`] is set: sleep until woken.
    Sleep,
}

impl<E: Entry> Consumer<E> {
    /// Asks the producer for a wakeup and checks the queue one last time.
    ///
    /// # Errors
    ///
    /// [`RingError`] when the queue cannot be used.
    pub fn prepare_sleep<M: Memory + ?Sized>(&self, mem: &M) -> Result<Sleep, RingError> {
        let flags = word(mem, self.queue().flags)?;
        flags.fetch_or(NEED_WAKEUP, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.pending(mem)? == 0 {
            Ok(Sleep::Sleep)
        } else {
            flags.fetch_and(!NEED_WAKEUP, Ordering::Relaxed);
            Ok(Sleep::Retry)
        }
    }

    /// Withdraws the request for a wakeup, after waking up.
    ///
    /// # Errors
    ///
    /// [`RingError::OutOfBounds`] when the memory has no flags word.
    pub fn woken<M: Memory + ?Sized>(&self, mem: &M) -> Result<(), RingError> {
        word(mem, self.queue().flags)?.fetch_and(!NEED_WAKEUP, Ordering::Relaxed);
        Ok(())
    }
}

impl<E: Entry> Producer<E> {
    /// Whether the consumer asked to be woken, checked after [`Producer::publish`].
    ///
    /// # Errors
    ///
    /// [`RingError::OutOfBounds`] when the memory has no flags word.
    pub fn needs_wakeup<M: Memory + ?Sized>(&self, mem: &M) -> Result<bool, RingError> {
        fence(Ordering::SeqCst);
        Ok(word(mem, self.queue().flags)?.load(Ordering::Relaxed) & NEED_WAKEUP != 0)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::AtomicU64;
    use std::vec::Vec;

    use super::*;
    use crate::abi::Layout;
    use crate::entry::Sqe;

    #[test]
    fn a_sleeping_consumer_asks_for_a_wakeup() {
        let layout = Layout::new(8, 0).expect("valid");
        let mem: Vec<AtomicU64> = (0..layout.words()).map(|_| AtomicU64::new(0)).collect();
        let mem = &mem[..];
        let mut tx = Producer::<Sqe>::new(layout.sq());
        let rx = Consumer::<Sqe>::new(layout.sq());

        tx.push(mem, &Sqe::nop(1)).expect("room");
        assert!(!tx.needs_wakeup(mem).expect("in bounds"));
        // Entries are pending, so the consumer must not sleep, and the flag is withdrawn.
        assert_eq!(rx.prepare_sleep(mem), Ok(Sleep::Retry));
        assert!(!tx.needs_wakeup(mem).expect("in bounds"));

        let mut rx = rx;
        rx.pop(mem).expect("valid").expect("pending");
        assert_eq!(rx.prepare_sleep(mem), Ok(Sleep::Sleep));
        tx.push(mem, &Sqe::nop(2)).expect("room");
        assert!(tx.needs_wakeup(mem).expect("in bounds"));
        rx.woken(mem).expect("in bounds");
        assert!(!tx.needs_wakeup(mem).expect("in bounds"));
    }
}
