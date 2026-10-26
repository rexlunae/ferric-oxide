//! Ferric Oxide: requests for TLB shootdowns that must also drop global entries.
//!
//! The usual shootdown handler reloads the page table (`RmmA::invalidate_all`), which keeps
//! global TLB entries. Kernel mappings are global while KPTI is off, so a broadcast shootdown
//! for a kernel mapping, such as a freed shadow stack, asks for a global flush through a
//! generation count: the broadcaster bumps it before it signals any CPU, and a CPU handling a
//! shootdown flushes global entries too when the count has moved since it last did
//! ([#260](https://github.com/rexlunae/ferric-oxide/issues/260)).
//!
//! A generation, rather than a per-CPU flag next to `wants_tlb_shootdown`, cannot lose a request
//! when it races with the handling of an earlier one: a CPU only skips the global flush when it
//! already did one after loading the current generation, and so after every page-table change
//! made before that generation was published.

use core::sync::atomic::{fence, AtomicUsize, Ordering};

/// Global-flush requests for `N` CPUs.
pub struct GlobalFlushRequests<const N: usize> {
    generation: AtomicUsize,
    seen: [AtomicUsize; N],
}

impl<const N: usize> GlobalFlushRequests<N> {
    /// No request pending for any CPU.
    pub const fn new() -> Self {
        Self {
            generation: AtomicUsize::new(0),
            seen: [const { AtomicUsize::new(0) }; N],
        }
    }

    /// Asks every CPU to drop global entries on its next shootdown. Call after changing the
    /// mapping and before signalling the CPUs.
    pub fn request(&self) {
        // Release: a CPU that loads the new generation also sees the page-table change.
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Whether `cpu`, which is handling a shootdown it has just claimed, must flush global entries
    /// as well. Records the current generation as handled, so it answers `true` once per
    /// request. An unknown CPU always flushes.
    pub fn take(&self, cpu: usize) -> bool {
        // Upgrades the relaxed claim of the shootdown to an acquire, so a generation bumped before
        // the request was signalled is visible here.
        fence(Ordering::Acquire);
        let generation = self.generation.load(Ordering::Acquire);
        // Only `cpu` itself writes its entry, so a plain load and store suffice and the common
        // case, no new request, costs no locked instruction.
        self.seen.get(cpu).is_none_or(|seen| {
            let pending = seen.load(Ordering::Relaxed) != generation;
            if pending {
                seen.store(generation, Ordering::Relaxed);
            }
            pending
        })
    }
}

#[cfg(test)]
mod tests {
    use super::GlobalFlushRequests;

    #[test]
    fn nothing_requested_means_no_global_flush() {
        let requests = GlobalFlushRequests::<2>::new();
        assert!(!requests.take(0));
        assert!(!requests.take(1));
    }

    #[test]
    fn each_cpu_flushes_once_per_request() {
        let requests = GlobalFlushRequests::<2>::new();
        requests.request();
        assert!(requests.take(0));
        assert!(!requests.take(0), "already flushed for this generation");
        assert!(requests.take(1));

        requests.request();
        requests.request();
        assert!(requests.take(1), "two requests need only one flush");
        assert!(!requests.take(1));
        assert!(requests.take(0));
    }

    #[test]
    fn unknown_cpus_always_flush() {
        let requests = GlobalFlushRequests::<1>::new();
        assert!(requests.take(5));
        assert!(requests.take(5));
    }
}
