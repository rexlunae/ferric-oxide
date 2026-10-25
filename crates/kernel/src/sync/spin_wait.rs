//! Ferric Oxide: spinning for a lock while still answering other CPUs.
//!
//! A CPU that spins for a lock with interrupts off cannot take a TLB shootdown IPI. If the
//! lock's holder, or a CPU the holder waits for, is itself waiting for this CPU to acknowledge
//! a shootdown, none of them can go on. [`until`] retries an attempt to take the lock and runs
//! a service routine between attempts, so the spinning CPU acknowledges what it is asked (the
//! kernel passes `PercpuBlock::maybe_handle_tlb_shootdown`).

/// Calls `attempt` until it returns `Some`, and returns that value. Before each retry it calls
/// `serve`, then hints the CPU that it is spinning.
///
/// `serve` runs only when an attempt failed, so a free lock is taken at once.
pub fn until<T>(mut attempt: impl FnMut() -> Option<T>, mut serve: impl FnMut()) -> T {
    loop {
        if let Some(value) = attempt() {
            return value;
        }
        serve();
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier, Mutex,
    };

    use super::until;

    #[test]
    fn a_free_lock_is_taken_without_serving() {
        let served = AtomicUsize::new(0);
        let value = until(
            || Some(7),
            || {
                served.fetch_add(1, Ordering::Relaxed);
            },
        );
        assert_eq!(value, 7);
        assert_eq!(served.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn serves_once_per_failed_attempt() {
        let mut attempts = 0;
        let mut served = 0;
        let value = until(
            || {
                attempts += 1;
                (attempts == 4).then_some(attempts)
            },
            || served += 1,
        );
        assert_eq!(value, 4);
        assert_eq!(served, 3);
    }

    /// The boot-smoke hang: the holder keeps the lock until the spinning side acknowledges
    /// it (a TLB shootdown). Spinning without serving would never return.
    #[test]
    fn a_holder_waiting_on_the_spinner_finishes() {
        let lock = Arc::new(Mutex::new(0));
        let acknowledged = Arc::new(AtomicBool::new(false));
        let held = Arc::new(Barrier::new(2));
        let holder = {
            let (lock, acknowledged, held) = (lock.clone(), acknowledged.clone(), held.clone());
            std::thread::spawn(move || {
                let mut guard = lock.lock().expect("the lock is not poisoned");
                held.wait();
                while !acknowledged.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                *guard += 1;
            })
        };
        held.wait();
        let guard = until(
            || lock.try_lock().ok(),
            || acknowledged.store(true, Ordering::Release),
        );
        assert_eq!(*guard, 1);
        drop(guard);
        holder.join().expect("the holder does not panic");
    }
}
