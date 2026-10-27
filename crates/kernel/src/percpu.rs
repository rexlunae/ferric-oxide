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

    /// The per-CPU one-shot scheduler timer (tickless scheduling, #186,
    /// `kernel.time.one_shot`).
    #[cfg(feature = "time_one_shot")]
    pub one_shot: OneShotPercpu,

    /// Ferric Oxide: this CPU's TLB, CR3 and shootdown counters (`tlb_stats`), served as
    /// `/scheme/sys/tlb`; see `crate::tlb_stats`.
    #[cfg(feature = "tlb_stats")]
    pub tlb_stats: ferric_trace::tlb::CpuCounters,
}

/// The per-CPU state of the one-shot scheduler timer (arch/x86_shared/timer.rs).
///
/// The armed deadline cell and the fired flag are written by this CPU's timer
/// path (interrupts off) and read by its switch path; the previous LVT entry
/// is what a periodic timer (the profiling timer) left behind, restored when
/// the one-shot is disarmed.
#[cfg(feature = "time_one_shot")]
#[derive(Debug)]
pub struct OneShotPercpu {
    /// This CPU's timer backend, chosen at boot (`None` until then, or when it
    /// has none).
    pub backend: Cell<Option<ferric_timed::oneshot::Backend>>,
    /// Which deadline (if any) the hardware has, with the rearm logic.
    pub armed: ferric_timed::ArmedDeadline,
    /// The armed deadline as a TSC reading, for the handler's due check.
    pub tsc_deadline: Cell<u64>,
    /// How late the last fire was served, in nanoseconds.
    pub late_ns: Cell<u64>,
    /// Early countdown interrupts re-armed since the selftest reset it.
    pub early: Cell<u32>,
    /// Whether a fire happened since the last consume.
    pub fired: core::sync::atomic::AtomicBool,
    /// How many timer interrupts this CPU's handler has seen (diagnostics).
    pub entries: core::sync::atomic::AtomicU64,
    /// The LVT timer entry that was replaced, or `u32::MAX` when none was.
    pub previous_lvt: Cell<u32>,
}

#[cfg(feature = "time_one_shot")]
impl OneShotPercpu {
    /// A disarmed one-shot: nothing armed, nothing fired, nothing to restore.
    pub const fn new() -> Self {
        Self {
            backend: Cell::new(None),
            armed: ferric_timed::ArmedDeadline::new(),
            tsc_deadline: Cell::new(0),
            late_ns: Cell::new(0),
            early: Cell::new(0),
            fired: core::sync::atomic::AtomicBool::new(false),
            entries: core::sync::atomic::AtomicU64::new(0),
            previous_lvt: Cell::new(u32::MAX),
        }
    }
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
        crate::tlb_stats::tlb_count!(TlbIpiSent);
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
        #[expect(clippy::bool_comparison)]
        if self.wants_tlb_shootdown.swap(false, Ordering::Relaxed) == false {
            return;
        }

        // TODO: Finer-grained flush
        crate::memory::RmmA::invalidate_all();
        crate::tlb_stats::shootdown_handled();

        if let Some(addrsp) = &*self.current_addrsp.borrow() {
            addrsp.tlb_ack.fetch_add(1, Ordering::Release);
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

        let retain_pgtbl = match (&*cur_addrsp, &next_addrsp) {
            (Some(p), Some(n)) => Arc::ptr_eq(p, n),
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => true,
        };
        if retain_pgtbl {
            // If we are not switching to a different address space, we can simply return early.
            crate::tlb_stats::tlb_count!(Cr3Same);
            return;
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
                crate::tlb_stats::tlb_count!(Cr3Switch);
                crate::trace::trace_event!(
                    ferric_trace::kernel::MM_CR3_SWITCH,
                    ferric_trace::kernel::Cr3SwitchKind::AddrSpace.raw(),
                    next_addrsp.table.utable.table().phys().data(),
                );
                drop(next_addrsp);
            }
            _ => {
                crate::memory::RmmA::set_table(rmm::TableKind::User, empty_cr3());
                crate::tlb_stats::tlb_count!(Cr3Empty);
                crate::trace::trace_event!(
                    ferric_trace::kernel::MM_CR3_SWITCH,
                    ferric_trace::kernel::Cr3SwitchKind::Empty.raw(),
                    0,
                );
            }
        }
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
            ptrace_flags: Cell::new(PtraceFlags::empty()),
            ptrace_session: RefCell::new(None),
            inside_syscall: Cell::new(false),

            syscall_debug_info: Cell::new(SyscallDebugInfo::default()),

            profiling: None,

            misc_arch_info: ArchPercpuMisc::default(),

            stats: CpuStats::default(),
            numa_node: Cell::new(None),
            #[cfg(feature = "time_one_shot")]
            one_shot: OneShotPercpu::new(),

            #[cfg(feature = "tlb_stats")]
            tlb_stats: ferric_trace::tlb::CpuCounters::new(),
        }
    }
}
