//! Ferric Oxide (`kernel.lazy_tlb`): lazy TLB for the idle context.
//!
//! The idle context has no address space. Without this module a switch to it loads the empty
//! table, and the switch back loads the process's table again: two full flushes of the
//! non-global TLB for every wait in which the CPU idles, such as one storage request waiting for
//! its interrupt. With `kernel.lazy_tlb` (x86_64), a switch to the idle context leaves the
//! previous table loaded and the CPU in that address space's `used_by` set, and a switch back to
//! the same address space loads nothing. Only the idle context runs on a kept table: the idle
//! loop never touches user memory, while other kernel-only contexts (ring workers, the end of an
//! exiting context) still get the empty table, so a stray user access there faults instead of
//! reaching another process.
//!
//! # Shootdowns
//!
//! A CPU that keeps a table stays in its CPU set, so every `Flusher` shootdown of that address
//! space reaches it and counts it. The shootdown handler flushes as usual and, with no current
//! address space, acknowledges on the kept one (`LazyTlb::acknowledge`); entries the CPU caches
//! while idle are therefore exactly as fresh as on a CPU running the process. Leaving the kept
//! address space uses the same order as leaving a current one in `percpu::switch_arch_hook`:
//! clear the CPU's bit, a sequentially consistent fence, answer any shootdown that raced with
//! the clear, then load the next table.
//!
//! # Lifetime
//!
//! The CPU holds a strong reference (`Arc`) to the kept address space, so its tables cannot be
//! freed while they are loaded. That reference must not outlive the contexts that use the
//! address space, or it would delay the teardown (exit's `Arc::try_unwrap` would fail and the
//! last drop would happen on the idle CPU, in the switch path, much later). So a context that
//! drops its address space (`Context::set_addr_space`, which exit, exec and the ring workers go
//! through) first calls [`evict`](crate::context::lazy_tlb::evict): every other CPU in the set
//! that announced the address space in its `LazySlot` gets a request and an interrupt, and the
//! dropping CPU waits until that CPU has loaded the empty table, dropped its reference and
//! withdrawn the announcement. The kept reference is therefore not the last one when the
//! interrupt handler drops it, so no address space is torn down in an interrupt handler.
//!
//! A CPU keeps an address space only after switching away from a context that uses it, and it
//! announces it in `switch_to` (`LazyTlb::announce`) while that context's lock is still
//! held. Changing a context's address space needs the same lock, so that context's drop is
//! ordered after the announcement and finds it; drops by other contexts may miss it, which is
//! harmless because each context's own drop covers the CPUs that keep the address space on its
//! behalf. The one reference this module can still drop last is the kept one when a CPU leaves it
//! on its own, in the same place and the same way as `switch_arch_hook` already drops the
//! previous context's reference.
//!
//! # KPTI and PCID
//!
//! Nothing changes on the per-CPU KPTI fields: `kpti_cr3_noflush` and `kpti_cr3_user` are bits
//! ORed into whatever table is loaded, and the idle context never returns to user space. Every
//! shootdown flush (`invalidate_all`) also invalidates the user PCID, so when the same address
//! space comes back, its user-PCID entries are as current as the kernel-PCID ones.
//!
//! # Other architectures
//!
//! The logic is architecture-independent, but it relies on TLB shootdown interrupts reaching an
//! idle CPU. riscv64 does not send interrupts between CPUs yet, and aarch64 boots are not tested
//! in CI, so only x86_64 keeps tables; elsewhere the option changes nothing.

pub(crate) mod plan;

use alloc::sync::Arc;
use core::{
    cell::Cell,
    sync::atomic::{fence, Ordering},
};

use self::plan::{LazySlot, SwitchPlan};
use crate::{
    context::memory::AddrSpaceWrapper,
    ipi::{ipi_single, IpiKind},
    percpu::{get_percpu_block, PercpuBlock},
};

/// Whether this kernel keeps tables for the idle context.
const ENABLED: bool = cfg!(all(feature = "lazy_tlb", target_arch = "x86_64"));

/// The identity of an address space in a `LazySlot`: its address, which is never 0.
fn id_of(space: &Arc<AddrSpaceWrapper>) -> usize {
    Arc::as_ptr(space).addr()
}

/// A CPU's lazy-TLB state, in its [`PercpuBlock`].
pub struct LazyTlb {
    slot: LazySlot,
    /// The address space whose table this CPU keeps for its idle context. Only this CPU uses
    /// it, with interrupts disabled; it is taken out only for moments in which no shootdown can
    /// be answered.
    kept: Cell<Option<Arc<AddrSpaceWrapper>>>,
}

impl LazyTlb {
    /// A CPU that keeps nothing.
    pub const fn new() -> Self {
        Self {
            slot: LazySlot::new(),
            kept: Cell::new(None),
        }
    }

    fn kept_id(&self) -> Option<usize> {
        let kept = self.kept.take();
        let id = kept.as_ref().map(id_of);
        self.kept.set(kept);
        id
    }

    /// Called by `switch_to` while it still holds the previous context's lock, with the next
    /// context already current: if that is the idle context, announces the address space the
    /// previous one used, which `switch_arch_hook` will then keep.
    pub fn announce(&self, percpu: &PercpuBlock) {
        if !ENABLED {
            return;
        }
        let next_is_idle = percpu
            .switch_internals
            .try_idle_context()
            .is_some_and(|idle| crate::context::is_current(&idle));
        if next_is_idle && let Some(cur) = &*percpu.current_addrsp.borrow() {
            self.slot.announce(id_of(cur));
        }
    }

    /// Decides what `switch_arch_hook` does with the page table.
    pub(crate) fn plan(
        &self,
        cur: Option<&Arc<AddrSpaceWrapper>>,
        next: Option<&Arc<AddrSpaceWrapper>>,
    ) -> SwitchPlan {
        plan::plan(cur.map(id_of), next.map(id_of), self.kept_id(), |&cur| {
            self.slot.holds(cur)
        })
    }

    /// Keeps `space`, the announced address space, whose table is loaded and whose set holds
    /// this CPU, for the idle context. Lets go of it again at once if a CPU dropping it asked
    /// in the meantime.
    pub(crate) fn keep(&self, percpu: &PercpuBlock, space: Option<Arc<AddrSpaceWrapper>>) {
        debug_assert!(space
            .as_ref()
            .is_some_and(|space| self.slot.holds(id_of(space))));
        self.kept.set(space);
        self.poll_eviction(percpu);
    }

    /// The kept address space became the current one again.
    pub(crate) fn resume(&self) {
        // Not the last reference: the next context holds one.
        drop(self.kept.take());
        self.slot.withdraw();
    }

    /// Before another table is loaded: leaves the kept address space's CPU set, answering any
    /// shootdown that raced with that.
    pub(crate) fn detach(&self, percpu: &PercpuBlock) {
        let Some(space) = self.kept.take() else {
            return;
        };
        space.used_by.atomic_clear(percpu.cpu_id);
        // See [`crate::percpu::switch_arch_hook`]: a shootdown that saw the bit must be
        // answered, on the kept address space.
        fence(Ordering::SeqCst);
        self.kept.set(Some(space));
        percpu.maybe_handle_tlb_shootdown();
    }

    /// After another table is loaded: drops the kept address space, then tells a CPU waiting in
    /// [`evict`] that this one no longer has it.
    pub(crate) fn finish(&self) {
        drop(self.kept.take());
        self.slot.withdraw();
    }

    /// Shootdown handler, with no current address space: acknowledges on the kept one, which
    /// counted this CPU.
    pub(crate) fn acknowledge(&self) {
        if let Some(space) = self.kept.take() {
            space.tlb_ack.fetch_add(1, Ordering::Release);
            self.kept.set(Some(space));
        }
    }

    /// Shootdown handler: if another CPU asked this one to let go of the kept address space,
    /// loads the empty table and drops the reference.
    pub(crate) fn poll_eviction(&self, percpu: &PercpuBlock) {
        if self.slot.eviction_due().is_none() {
            return;
        }
        // Announced but not kept yet: `keep` polls again.
        let Some(space) = self.kept.take() else {
            return;
        };
        space.used_by.atomic_clear(percpu.cpu_id);
        fence(Ordering::SeqCst);
        crate::context::load_empty_table();
        // A shootdown that counted this CPU before the bit was cleared: the load just flushed.
        if percpu.wants_tlb_shootdown.swap(false, Ordering::Relaxed) {
            space.tlb_ack.fetch_add(1, Ordering::Release);
        }
        // Not the last reference: the CPU in `evict` holds one until the withdrawal.
        drop(space);
        self.slot.withdraw();
    }
}

impl Default for LazyTlb {
    fn default() -> Self {
        Self::new()
    }
}

/// Makes every other CPU that keeps `space` for its idle context, or announced that it will,
/// let go of it, and waits until each has. Called by a context dropping its reference to
/// `space`, before it does; see the [module documentation](self).
pub fn evict(space: &Arc<AddrSpaceWrapper>) {
    if !ENABLED {
        return;
    }
    let id = id_of(space);
    let me = PercpuBlock::current();
    for cpu in space.used_by.iter() {
        if cpu == me.cpu_id {
            continue;
        }
        let Some(block) = get_percpu_block(cpu) else {
            continue;
        };
        let slot = &block.lazy_tlb.slot;
        if !slot.holds(id) {
            continue;
        }
        slot.request(id);
        ipi_single(IpiKind::Tlb, block);
        while slot.holds(id) {
            if slot.renew(id) {
                ipi_single(IpiKind::Tlb, block);
            }
            me.maybe_handle_tlb_shootdown();
            core::hint::spin_loop();
        }
    }
}
