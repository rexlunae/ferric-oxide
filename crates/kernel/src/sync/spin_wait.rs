//! Ferric Oxide: spinning for a lock while still answering other CPUs.
//!
//! A CPU that spins for a lock with interrupts off cannot take a TLB shootdown IPI. If the
//! lock's holder, or a CPU the holder waits for, is itself waiting for this CPU to acknowledge
//! a shootdown, none of them can go on. [`until`] retries an attempt to take the lock and runs
//! a service routine between attempts, so the spinning CPU acknowledges what it is asked (the
//! kernel passes `PercpuBlock::maybe_handle_tlb_shootdown`). [`upgrade`] does the same for a
//! guard that waits for the other readers to leave.

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

/// Upgrades `guard` with `try_upgrade`, which hands the guard back while the upgrade is not
/// possible yet, and calls `serve` between tries as [`until`] does.
pub fn upgrade<G, W>(
    guard: G,
    mut try_upgrade: impl FnMut(G) -> Result<W, G>,
    serve: impl FnMut(),
) -> W {
    let mut pending = Some(guard);
    until(
        || match try_upgrade(pending.take()?) {
            Ok(upgraded) => Some(upgraded),
            Err(guard) => {
                pending = Some(guard);
                None
            }
        },
        serve,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier, Mutex,
    };

    use super::{until, upgrade};

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

    /// An upgrade waits for a reader that leaves only once the upgrading side has
    /// acknowledged it, and it keeps the guard it was handed throughout.
    #[test]
    fn an_upgrade_waits_for_a_reader_that_needs_an_acknowledgement() {
        let readers = Arc::new(AtomicUsize::new(1));
        let acknowledged = Arc::new(AtomicBool::new(false));
        let reader = {
            let (readers, acknowledged) = (readers.clone(), acknowledged.clone());
            std::thread::spawn(move || {
                while !acknowledged.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                readers.fetch_sub(1, Ordering::Release);
            })
        };
        let mut tries = 0;
        let upgraded = upgrade(
            "guard",
            |guard| {
                tries += 1;
                if readers.load(Ordering::Acquire) == 0 {
                    Ok((guard, "write"))
                } else {
                    Err(guard)
                }
            },
            || acknowledged.store(true, Ordering::Release),
        );
        assert_eq!(upgraded, ("guard", "write"));
        assert!(tries >= 2);
        reader.join().expect("the reader does not panic");
    }
}
