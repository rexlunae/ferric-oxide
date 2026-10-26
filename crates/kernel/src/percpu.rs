use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    cell::{Cell, RefCell},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use rmm::Arch;
use syscall::PtraceFlags;

use crate::{
    arch::device::ArchPercpuMisc,
    context::{
        empty_cr3,
        lazy_tlb::{plan::SwitchPlan, LazyTlb},
        memory::{AddrSpaceSwitchReadGuard, AddrSpaceWrapper},
        switch::ContextSwitchPercpu,
    },
    cpu_set::{LogicalCpuId, MAX_CPU_COUNT},
    cpu_stats::{CpuStats, CpuStatsData},
    numa::{NumaNode, NumaNodeID},
    ptrace::Session,
    syscall::debug::SyscallDebugInfo,
};

/// The percpu block, that stored all percpu variables.
// TODO: consider splitting this struct into a !Sync part (which safe code is not allowed to get
// references to for other hw threads' blocks) and a Sync part
pub struct PercpuBlock {
    /// A unique immutable number that identifies the current CPU - used for scheduling
    pub cpu_id: LogicalCpuId,

    /// Context management
    pub switch_internals: ContextSwitchPercpu,

    pub current_addrsp: RefCell<Option<Arc<AddrSpaceWrapper>>>,
    pub new_addrsp_tmp: Cell<Option<Arc<AddrSpaceWrapper>>>,
    pub new_addrsp_guard: Cell<Option<AddrSpaceSwitchReadGuard>>,
    pub wants_tlb_shootdown: AtomicBool,
    /// Ferric Oxide (`kernel.lazy_tlb`): the address space kept loaded for the idle context.
    pub lazy_tlb: LazyTlb,

    // TODO: Put mailbox queues here, e.g. for TLB shootdown? Just be sure to 128-byte align it
    // first to avoid cache invalidation.
    pub profiling: Option<&'static crate::profiling::RingBuffer>,

    pub ptrace_flags: Cell<PtraceFlags>,
    pub ptrace_session: RefCell<Option<Weak<Session>>>,
    pub inside_syscall: Cell<bool>,

    pub syscall_debug_info: Cell<SyscallDebugInfo>,

    pub misc_arch_info: crate::arch::device::ArchPercpuMisc,

    pub stats: CpuStats,

    pub numa_node: Cell<Option<(NumaNodeID, &'static NumaNode)>>,
}

pub static ALL_PERCPU_BLOCKS: [AtomicPtr<PercpuBlock>; MAX_CPU_COUNT as usize] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPU_COUNT as usize];

#[inline]
pub fn get_percpu_block(idx: LogicalCpuId) -> Option<&'static PercpuBlock> {
    let ptr = ALL_PERCPU_BLOCKS
        .get(idx.get() as usize)?
        .load(Ordering::Relaxed);
    unsafe { ptr.as_ref() }
}
/// Ferric Oxide: the per-CPU block of `cpu` when it is running and can take device interrupts,
/// that is, unless it is the `profiling` feature's profiler CPU
/// ([`crate::profiling::is_profiler_cpu`]), which would drop them.
pub fn interrupt_target(cpu: LogicalCpuId) -> Option<&'static PercpuBlock> {
    get_percpu_block(cpu).filter(|_| !crate::profiling::is_profiler_cpu(cpu))
}
pub fn all_percpu_blocks() -> impl Iterator<Item = &'static PercpuBlock> {
    (0..ALL_PERCPU_BLOCKS.len()).filter_map(|i| get_percpu_block(LogicalCpuId::new(i as u32)))
}

#[allow(unused)]
pub unsafe fn init_tlb_shootdown(id: LogicalCpuId, block: *mut PercpuBlock) {
    ALL_PERCPU_BLOCKS[id.get() as usize].store(block, Ordering::Release)
}

pub fn get_all_stats() -> Vec<(LogicalCpuId, CpuStatsData)> {
    let mut res = (0..ALL_PERCPU_BLOCKS.len())
        .filter_map(|i| get_percpu_block(LogicalCpuId::new(i as u32)))
        .map(|block| {
            let stats = &block.stats;
            (block.cpu_id, stats.into())
        })
        .collect::<Vec<_>>();
    res.sort_unstable_by_key(|(id, _stats)| id.get());
    res
}

// PercpuBlock::current() is implemented somewhere in the arch-specific modules

pub fn shootdown_tlb_ipi(target: Option<LogicalCpuId>) {
    if cfg!(not(feature = "multi_core")) {
        return;
    }

    if let Some(target) = target {
        let my_percpublock = PercpuBlock::current();
        assert_ne!(target, my_percpublock.cpu_id);

        let Some(percpublock) = get_percpu_block(target) else {
            crate::warn!("Trying to TLB shootdown a CPU that doesn't exist or isn't initialized.");
            return;
        };
        #[expect(clippy::bool_comparison)]
        while percpublock
            .wants_tlb_shootdown
            .swap(true, Ordering::Release)
            == true
        {
            // Load is faster than CAS or on x86, LOCK BTS
            while percpublock.wants_tlb_shootdown.load(Ordering::Relaxed) == true {
                my_percpublock.maybe_handle_tlb_shootdown();
                core::hint::spin_loop();
            }
        }

        crate::ipi::ipi_single(crate::ipi::IpiKind::Tlb, percpublock);
    } else {
        let current = crate::cpu_id();
        for id in 0..crate::cpu_count() {
            // TODO: Optimize: use global counter and percpu ack counters, send IPI using
            // destination shorthand "all CPUs".
            let id = LogicalCpuId::new(id);
            if id != current {
                shootdown_tlb_ipi(Some(id));
            }
        }
    }
}
impl PercpuBlock {
    pub fn maybe_handle_tlb_shootdown(&self) {
        // Ferric Oxide (kernel.lazy_tlb): another CPU is dropping the address space kept for
        // the idle context.
        self.lazy_tlb.poll_eviction(self);

        #[expect(clippy::bool_comparison)]
        if self.wants_tlb_shootdown.swap(false, Ordering::Relaxed) == false {
            return;
        }

        // TODO: Finer-grained flush
        crate::memory::RmmA::invalidate_all();

        if let Some(addrsp) = &*self.current_addrsp.borrow() {
            addrsp.tlb_ack.fetch_add(1, Ordering::Release);
        } else {
            // Ferric Oxide (kernel.lazy_tlb): an idle CPU stays in the set of the address space
            // it keeps, so the shootdown counted it.
            self.lazy_tlb.acknowledge();
        }
    }
}

#[cfg(test)]
impl PercpuBlock {
    pub fn current() -> &'static Self {
        let ptr = MOCK_CURRENT_PERCPU.with(|p| p.get());
        assert!(!ptr.is_null());
        unsafe { &*ptr }
    }

    pub fn set_mock_current(block: *const PercpuBlock) {
        MOCK_CURRENT_PERCPU.with(|p| p.set(block));
    }
}

#[cfg(test)]
std::thread_local! {
    pub static MOCK_CURRENT_PERCPU: std::cell::Cell<*const PercpuBlock> = const { std::cell::Cell::new(core::ptr::null()) };
}

pub unsafe fn switch_arch_hook() {
    unsafe {
        let percpu = PercpuBlock::current();

        let cur_addrsp = percpu.current_addrsp.borrow();
        let next_addrsp = percpu.new_addrsp_tmp.take();
        let next_addrsp_guard = percpu.new_addrsp_guard.take();

        // Ferric Oxide (kernel.lazy_tlb): the idle context keeps the previous table if
        // `switch_to` announced it, and the same address space coming back takes it again
        // without a load. See `context::lazy_tlb`.
        match percpu
            .lazy_tlb
            .plan(cur_addrsp.as_ref(), next_addrsp.as_ref())
        {
            // If we are not switching to a different address space, we can simply return early.
            SwitchPlan::Keep => return,
            SwitchPlan::Lazy => {
                drop(cur_addrsp);
                let prev_addrsp = percpu.current_addrsp.borrow_mut().take();
                percpu.lazy_tlb.keep(percpu, prev_addrsp);
                return;
            }
            SwitchPlan::Resume => {
                drop(cur_addrsp);
                *percpu.current_addrsp.borrow_mut() = next_addrsp;
                percpu.lazy_tlb.resume();
                return;
            }
            SwitchPlan::Load => {}
        }
        if let Some(prev_addrsp) = &*cur_addrsp {
            prev_addrsp.used_by.atomic_clear(percpu.cpu_id);

            // See [`Flusher::flush`].
            //
            // Without the fence, `wants_tlb_shootdown` check *may* happen
            // before the CPU is removed from the `used_by` set. Hence, if a
            // shootdown request arises *after* the check and *before* removing
            // the CPU from the set, it would be missed and the CPU who
            // requested the shootdown would spin forever since the request was
            // never ACKed.
            core::sync::atomic::fence(Ordering::SeqCst);

            percpu.maybe_handle_tlb_shootdown();
        }
        // Ferric Oxide (kernel.lazy_tlb): the same for an address space kept for the idle
        // context.
        percpu.lazy_tlb.detach(percpu);

        drop(cur_addrsp);

        // Tell future TLB shootdown handlers that old_addrsp_tmp is no longer the current address
        // space.
        *percpu.current_addrsp.borrow_mut() = next_addrsp;

        if let Some(next_addrsp) = &*percpu.current_addrsp.borrow() {
            next_addrsp.used_by.atomic_set(percpu.cpu_id);
        }
        match next_addrsp_guard {
            Some(next_addrsp) => {
                next_addrsp.table.utable.make_current();
                drop(next_addrsp);
            }
            _ => {
                crate::memory::RmmA::set_table(rmm::TableKind::User, empty_cr3());
            }
        }
        // Ferric Oxide (kernel.lazy_tlb): only once that table is no longer loaded.
        percpu.lazy_tlb.finish();
    }
}
impl PercpuBlock {
    pub const fn init(cpu_id: LogicalCpuId) -> Self {
        Self {
            cpu_id,
            switch_internals: ContextSwitchPercpu::default(),
            current_addrsp: RefCell::new(None),
            new_addrsp_tmp: Cell::new(None),
            new_addrsp_guard: Cell::new(None),
            wants_tlb_shootdown: AtomicBool::new(false),
            lazy_tlb: LazyTlb::new(),
            ptrace_flags: Cell::new(PtraceFlags::empty()),
            ptrace_session: RefCell::new(None),
            inside_syscall: Cell::new(false),

            syscall_debug_info: Cell::new(SyscallDebugInfo::default()),

            profiling: None,

            misc_arch_info: ArchPercpuMisc::default(),

            stats: CpuStats::default(),
            numa_node: Cell::new(None),
        }
    }
}
