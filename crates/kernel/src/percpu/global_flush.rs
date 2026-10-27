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

    /// Flushes the TLB of `cpu`, which is handling a shootdown it has just claimed: with
    /// `global` when a request came since `cpu` last flushed global entries, and with `local`
    /// otherwise. An unknown CPU always runs `global`.
    ///
    /// The generation is recorded as handled only after `global` returns. A shootdown handled
    /// inside `global` (an interrupt arriving while a caller with interrupts enabled flushes)
    /// therefore still sees the request pending and flushes global entries itself before it
    /// acknowledges, instead of acknowledging a flush that has not happened yet.
    pub fn flush(&self, cpu: usize, global: impl FnOnce(), local: impl FnOnce()) {
        // Upgrades the relaxed claim of the shootdown to an acquire, so a generation bumped before
        // the request was signalled is visible here.
        fence(Ordering::Acquire);
        let generation = self.generation.load(Ordering::Acquire);
        let Some(seen) = self.seen.get(cpu) else {
            global();
            return;
        };
        // Only `cpu` itself writes its entry, so a plain load and store suffice and the common
        // case, no new request, costs no locked instruction.
        if seen.load(Ordering::Relaxed) == generation {
            local();
            return;
        }
        global();
        // A nested handling may have recorded a newer generation; recording this older one again
        // only costs one extra global flush later.
        seen.store(generation, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::GlobalFlushRequests;

    /// Runs `flush` for `cpu` and reports whether it chose the global flush.
    fn flushed_global<const N: usize>(requests: &GlobalFlushRequests<N>, cpu: usize) -> bool {
        let global = Cell::new(false);
        requests.flush(cpu, || global.set(true), || {});
        global.get()
    }

    #[test]
    fn nothing_requested_means_no_global_flush() {
        let requests = GlobalFlushRequests::<2>::new();
        assert!(!flushed_global(&requests, 0));
        assert!(!flushed_global(&requests, 1));
    }

    #[test]
    fn each_cpu_flushes_once_per_request() {
        let requests = GlobalFlushRequests::<2>::new();
        requests.request();
        assert!(flushed_global(&requests, 0));
        assert!(
            !flushed_global(&requests, 0),
            "already flushed for this generation"
        );
        assert!(flushed_global(&requests, 1));

        requests.request();
        requests.request();
        assert!(
            flushed_global(&requests, 1),
            "two requests need only one flush"
        );
        assert!(!flushed_global(&requests, 1));
        assert!(flushed_global(&requests, 0));
    }

    #[test]
    fn unknown_cpus_always_flush() {
        let requests = GlobalFlushRequests::<1>::new();
        assert!(flushed_global(&requests, 5));
        assert!(flushed_global(&requests, 5));
    }

    /// A shootdown handled while the global flush of an earlier one is under way must not take
    /// that unfinished flush as its own.
    #[test]
    fn a_nested_handling_flushes_global_entries_itself() {
        let requests = GlobalFlushRequests::<1>::new();
        requests.request();
        let nested_global = Cell::new(false);
        requests.flush(
            0,
            || nested_global.set(flushed_global(&requests, 0)),
            || panic!("a pending request needs the global flush"),
        );
        assert!(nested_global.get());
        assert!(
            !flushed_global(&requests, 0),
            "both handled the one request"
        );
    }
}
