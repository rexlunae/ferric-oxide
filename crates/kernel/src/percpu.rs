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
    memory::tlb::{TlbAck, TlbAcks, TlbInvalidation, TlbMailbox},
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
    /// Ferric Oxide: what the pending shootdowns ask this CPU to invalidate. Requesters post to
    /// it before they set `wants_tlb_shootdown`; see [`crate::memory::tlb`].
    pub tlb_mailbox: TlbMailbox,

    // TODO: 128-byte align `tlb_mailbox`, which other CPUs write, to avoid false sharing with
    // the fields this CPU uses.
    pub profiling: Option<&'static crate::profiling::RingBuffer>,

    pub ptrace_flags: Cell<PtraceFlags>,
    pub ptrace_session: RefCell<Option<Weak<Session>>>,
    pub inside_syscall: Cell<bool>,

    pub syscall_debug_info: Cell<SyscallDebugInfo>,

    pub misc_arch_info: crate::arch::device::ArchPercpuMisc,

    pub stats: CpuStats,

    pub numa_node: Cell<Option<(NumaNodeID, &'static NumaNode)>>,
}

mod global_flush;

/// Ferric Oxide: global-flush requests from broadcast shootdowns (see [`global_flush`]).
static GLOBAL_FLUSH: global_flush::GlobalFlushRequests<{ MAX_CPU_COUNT as usize }> =
    global_flush::GlobalFlushRequests::new();

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

/// Asks every other CPU to flush its whole TLB, global entries included, and waits until each
/// has (issue #281). Meant for kernel mappings, which may be global; while it waits, this CPU
/// keeps answering shootdowns, so two CPUs broadcasting at once do not wait on each other.
#[cfg_attr(
    not(all(target_arch = "x86_64", shstk_compiled)),
    expect(dead_code, reason = "only shadow-stack teardown broadcasts")
)]
pub fn shootdown_tlb_all() {
    if cfg!(not(feature = "multi_core")) {
        return;
    }
    // Before any request is posted, so the handling that acknowledges one also sees this.
    GLOBAL_FLUSH.request();
    let current = crate::cpu_id();
    let mut acks = TlbAcks::new();
    for id in 0..crate::cpu_count() {
        // TODO: Optimize: send the IPI using destination shorthand "all CPUs".
        let id = LogicalCpuId::new(id);
        if id != current
            && let Some(ack) = request_tlb_shootdown(id, &TlbInvalidation::ALL)
        {
            expect_tlb_ack(&mut acks, id, ack);
        }
    }
    wait_for_tlb_acks(&mut acks);
}

/// Ferric Oxide: adds `ack`, from a request to `cpu`, to what [`wait_for_tlb_acks`] waits for.
pub fn expect_tlb_ack<'a>(acks: &mut TlbAcks<'a>, cpu: LogicalCpuId, ack: TlbAck<'a>) {
    if let Err(ack) = acks.record(cpu, ack) {
        // Not reached: no CPU with a per-CPU block is beyond `TlbAcks`'s capacity.
        while !ack.is_handled() {
            PercpuBlock::current().maybe_handle_tlb_shootdown();
            core::hint::spin_loop();
        }
    }
}

/// Ferric Oxide: waits until every request in `acks` has been handled by its target, answering
/// this CPU's own shootdowns meanwhile: a target may itself be waiting for this CPU.
pub fn wait_for_tlb_acks(acks: &mut TlbAcks<'_>) {
    #[cfg(target_arch = "aarch64")]
    let mut stall_watch = crate::arch::misc::StallWatch::start(2);

    #[allow(unused_variables, reason = "only aarch64 reports a stall")]
    while let pending @ 1.. = acks.poll() {
        PercpuBlock::current().maybe_handle_tlb_shootdown();
        #[cfg(target_arch = "aarch64")]
        if stall_watch.stalled() {
            crate::error!(
                "TLB shootdown stalled on CPU {}: {} CPUs have not acknowledged",
                crate::cpu_id(),
                pending
            );
        }
        core::hint::spin_loop();
    }
}

/// Ferric Oxide: asks `target` to invalidate `inv`. Returns what tells when `target` has done
/// it, or `None` if no request was sent (a single-core kernel, or a CPU without a per-CPU block).
///
/// # Panics
///
/// Panics if `target` is this CPU.
pub fn request_tlb_shootdown(
    target: LogicalCpuId,
    inv: &TlbInvalidation,
) -> Option<TlbAck<'static>> {
    if cfg!(not(feature = "multi_core")) {
        return None;
    }
    assert_ne!(target, PercpuBlock::current().cpu_id);

    let Some(percpublock) = get_percpu_block(target) else {
        crate::warn!("Trying to TLB shootdown a CPU that doesn't exist or isn't initialized.");
        return None;
    };
    // Post before the flag below publishes it with `Release`.
    let ack = percpublock.tlb_mailbox.post(inv);
    // A flag that was already set has an interrupt on the way from the CPU that set it, and the
    // handling that swaps it off comes after this swap, so it sees the post (`memory::tlb`).
    if !percpublock
        .wants_tlb_shootdown
        .swap(true, Ordering::Release)
    {
        crate::ipi::ipi_single(crate::ipi::IpiKind::Tlb, percpublock);
    }
    Some(ack)
}
/// Ferric Oxide: invalidates page number `page` in this CPU's TLB (with KPTI and PCID, for both
/// PCIDs). The page number comes from an address, so the product does not wrap.
pub fn invalidate_tlb_page(page: usize) {
    crate::memory::RmmA::invalidate(rmm::VirtualAddress::new(
        page.wrapping_mul(crate::memory::PAGE_SIZE),
    ));
}

impl PercpuBlock {
    pub fn maybe_handle_tlb_shootdown(&self) {
        #[expect(clippy::bool_comparison)]
        // `Acquire` pairs with the requester's `Release` swap, so the mailbox shows its post.
        if self.wants_tlb_shootdown.swap(false, Ordering::Acquire) == false {
            return;
        }

        // Acknowledges, per request, once it has invalidated (issue #264). A full flush also
        // drops global entries when a broadcast asked for that: the check runs after the
        // mailbox's request counter was loaded, so it sees the generation of every broadcast
        // whose request this handling acknowledges.
        self.tlb_mailbox.receive(invalidate_tlb_page, || {
            GLOBAL_FLUSH.flush(
                self.cpu_id.get() as usize,
                crate::memory::RmmA::invalidate_all_global,
                crate::memory::RmmA::invalidate_all,
            );
        });
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
            return;
        }
        if let Some(prev_addrsp) = &*cur_addrsp {
            prev_addrsp.used_by.atomic_clear(percpu.cpu_id);

            // See [`Flusher::flush`].
            //
            // Without the fence, the `wants_tlb_shootdown` check *may* happen
            // before the CPU is removed from the `used_by` set. A request that
            // saw the CPU in the set and arrived in between would then wait for
            // this CPU's next interrupt or spin point instead of being answered
            // here, before the table changes. Either way the requester waits for
            // this CPU to handle its own request (`memory::tlb`), so leaving the
            // set never acknowledges anything by itself.
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
                drop(next_addrsp);
            }
            _ => {
                crate::memory::RmmA::set_table(rmm::TableKind::User, empty_cr3());
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
            tlb_mailbox: TlbMailbox::new(),
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
