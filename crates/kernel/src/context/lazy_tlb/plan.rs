//! The hardware-independent half of lazy TLB: what a context switch does with the page table,
//! and the per-CPU slot through which another CPU asks this one to stop keeping a table.
//!
//! Both are plain logic over identities and atomics, so the host test package in
//! `crates/kernel/tests` runs them directly.

use core::sync::atomic::{AtomicUsize, Ordering};

/// What a context switch does with the user page table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchPlan {
    /// The loaded table stays, and so does everything about it.
    Keep,
    /// The next context is the idle one: keep the previous table loaded, and keep this CPU in
    /// that address space's CPU set so that shootdowns still reach it.
    Lazy,
    /// The next context's table is the one kept for the idle context: take it back, with no
    /// table load.
    Resume,
    /// Load the next context's table (or the empty one), leaving the current or kept address
    /// space first.
    Load,
}

/// Decides what a switch from an address space `cur` to `next` does, while this CPU keeps
/// `kept` for the idle context. `announced` says whether the switch announced `cur` as kept for
/// the idle context, and is only asked when it matters.
///
/// A CPU keeps a table only while it runs the idle context, so `kept` is `None` whenever `cur`
/// is `Some`.
pub(crate) fn plan<T: PartialEq>(
    cur: Option<T>,
    next: Option<T>,
    kept: Option<T>,
    announced: impl FnOnce(&T) -> bool,
) -> SwitchPlan {
    match (cur, next) {
        (Some(cur), Some(next)) if cur == next => SwitchPlan::Keep,
        // From the idle context to another kernel-only one, which must not see a user table.
        (None, None) if kept.is_some() => SwitchPlan::Load,
        (None, None) => SwitchPlan::Keep,
        (Some(cur), None) if announced(&cur) => SwitchPlan::Lazy,
        (None, Some(next)) if kept.is_some_and(|kept| kept == next) => SwitchPlan::Resume,
        _ => SwitchPlan::Load,
    }
}

/// A CPU's announced lazy-TLB state: which address space it keeps (or is about to keep) for its
/// idle context, and which one another CPU asked it to let go of. Identities are nonzero (the
/// address of the address space); 0 means none.
///
/// The owning CPU announces before it relies on the table and withdraws only after it loaded
/// another one and dropped its reference. Another CPU that sees the announcement files a
/// request, interrupts the owner and waits for the withdrawal, filing the request again if a
/// request about another address space replaced it.
#[derive(Debug, Default)]
pub(crate) struct LazySlot {
    kept: AtomicUsize,
    evict: AtomicUsize,
}

impl LazySlot {
    /// An empty slot.
    pub(crate) const fn new() -> Self {
        Self {
            kept: AtomicUsize::new(0),
            evict: AtomicUsize::new(0),
        }
    }

    /// Owner: announces that this CPU keeps `id`.
    pub(crate) fn announce(&self, id: usize) {
        debug_assert_ne!(id, 0);
        self.kept.store(id, Ordering::SeqCst);
    }

    /// Owner: the announced identity, if another CPU asked this one to let go of it.
    pub(crate) fn eviction_due(&self) -> Option<usize> {
        let kept = self.kept.load(Ordering::Relaxed);
        (kept != 0 && self.evict.load(Ordering::SeqCst) == kept).then_some(kept)
    }

    /// Owner: no table is kept any more. Called only once the kept table is no longer loaded and
    /// the owner no longer references the address space. Consumes a request for it.
    pub(crate) fn withdraw(&self) {
        let kept = self.kept.swap(0, Ordering::SeqCst);
        if kept != 0 {
            let _ = self
                .evict
                .compare_exchange(kept, 0, Ordering::SeqCst, Ordering::Relaxed);
        }
    }

    /// Other CPU: asks the owner to let go of `id`.
    pub(crate) fn request(&self, id: usize) {
        debug_assert_ne!(id, 0);
        self.evict.store(id, Ordering::SeqCst);
    }

    /// Other CPU, while it waits for the owner: files the request for `id` again if a request
    /// about another address space replaced it (from a CPU that saw an announcement the owner
    /// has since withdrawn), so the owner still sees it. Returns whether it had to.
    pub(crate) fn renew(&self, id: usize) -> bool {
        if self.evict.load(Ordering::SeqCst) == id {
            return false;
        }
        self.request(id);
        true
    }

    /// Whether the owner announced that it keeps `id`.
    pub(crate) fn holds(&self, id: usize) -> bool {
        self.kept.load(Ordering::SeqCst) == id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn not_asked(_: &u8) -> bool {
        panic!("only a switch from an address space to none asks whether it was announced")
    }

    #[test]
    fn same_address_space_keeps_the_table() {
        assert_eq!(plan(Some(1), Some(1), None, not_asked), SwitchPlan::Keep);
        assert_eq!(plan(None::<u8>, None, None, not_asked), SwitchPlan::Keep);
    }

    #[test]
    fn only_an_announced_table_is_kept() {
        assert_eq!(plan(Some(1), None, None, |&cur| cur == 1), SwitchPlan::Lazy);
        assert_eq!(plan(Some(1), None, None, |_| false), SwitchPlan::Load);
    }

    #[test]
    fn the_kept_address_space_resumes_without_a_load() {
        assert_eq!(plan(None, Some(1), Some(1), not_asked), SwitchPlan::Resume);
        assert_eq!(plan(None, Some(2), Some(1), not_asked), SwitchPlan::Load);
        assert_eq!(plan(None, Some(2), None, not_asked), SwitchPlan::Load);
    }

    #[test]
    fn a_kernel_only_context_after_idle_gets_the_empty_table() {
        assert_eq!(plan(None, None, Some(1), not_asked), SwitchPlan::Load);
    }

    #[test]
    fn different_address_spaces_load() {
        assert_eq!(plan(Some(1), Some(2), None, not_asked), SwitchPlan::Load);
    }

    #[test]
    fn a_request_is_due_until_withdrawn_and_then_consumed() {
        let slot = LazySlot::new();
        slot.announce(7);
        assert!(slot.holds(7));
        assert_eq!(slot.eviction_due(), None);
        slot.request(7);
        assert_eq!(slot.eviction_due(), Some(7));
        slot.withdraw();
        assert!(!slot.holds(7));
        assert_eq!(slot.eviction_due(), None);
        // Consumed: keeping 7 again is not evicted at once.
        slot.announce(7);
        assert_eq!(slot.eviction_due(), None);
    }

    #[test]
    fn a_request_for_another_address_space_is_not_due() {
        let slot = LazySlot::new();
        slot.request(9);
        slot.announce(7);
        assert_eq!(slot.eviction_due(), None);
        slot.withdraw();
        assert!(!slot.holds(7));
    }

    #[test]
    fn a_replaced_request_is_filed_again() {
        let slot = LazySlot::new();
        slot.announce(7);
        slot.request(7);
        assert!(!slot.renew(7));
        // A CPU dropping another address space asks too, from a withdrawn announcement.
        slot.request(9);
        assert_eq!(slot.eviction_due(), None);
        assert!(slot.renew(7));
        assert_eq!(slot.eviction_due(), Some(7));
    }

    #[test]
    fn a_waiting_requester_sees_the_withdrawal() {
        use std::{sync::Arc, thread};

        for _ in 0..if cfg!(miri) { 10 } else { 200 } {
            let slot = Arc::new(LazySlot::new());
            slot.announce(5);
            let owner = {
                let slot = Arc::clone(&slot);
                thread::spawn(move || {
                    while slot.eviction_due().is_none() {
                        std::hint::spin_loop();
                    }
                    slot.withdraw();
                })
            };
            slot.request(5);
            while slot.holds(5) {
                slot.renew(5);
                std::hint::spin_loop();
            }
            owner.join().unwrap();
            assert_eq!(slot.eviction_due(), None);
        }
    }
}
