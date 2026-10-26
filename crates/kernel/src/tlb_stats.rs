//! Ferric Oxide: TLB, CR3 and shootdown accounting (`tlb_stats`, `kernel.tlb_stats`), and the
//! debug-level `mm.tlb_flush`, `mm.tlb_ipi`, `mm.cr3_switch` and `iommu.iotlb_flush`
//! tracepoints (`kernel.trace`).
//!
//! Each CPU keeps a `ferric_trace::tlb::CpuCounters` in its per-CPU block
//! (`PercpuBlock::tlb_stats`), and `tlb_count!` adds to the current CPU's. The counters are
//! plain per-CPU words: only the owning CPU writes them, with a relaxed load and store, and
//! the `sys` scheme reads every CPU's for `/scheme/sys/tlb` (`scheme::sys::tlb`). What each
//! counts, and where, is documented on `ferric_trace::tlb::Counter`.
//!
//! The two mechanisms are independent: without `tlb_stats`, `tlb_count!` expands to nothing
//! and never evaluates its argument; without `trace` (or with `kernel.trace_max_level` below
//! `debug`), so do the tracepoints. The helpers here hold both, so the call sites in the
//! memory code stay one line each. [`Stopwatch`](crate::tlb_stats::Stopwatch) reads the TSC
//! only when one of the two is built.

use crate::memory::RmmA;

/// Adds to one of the current CPU's TLB counters (`ferric_trace::tlb::Counter`, by variant
/// name): one, or the `u64` given. Without `tlb_stats` it expands to nothing and the amount
/// is not evaluated.
///
/// Use it where interrupts are off, as the kernel's TLB code runs (an interrupt adding to the
/// same counter between the load and the store would lose a count).
macro_rules! tlb_count {
    ($counter:ident) => {
        $crate::tlb_stats::tlb_count!($counter, 1_u64)
    };
    ($counter:ident, $n:expr) => {{
        #[cfg(feature = "tlb_stats")]
        $crate::percpu::PercpuBlock::current()
            .tlb_stats
            .add(::ferric_trace::tlb::Counter::$counter, $n);
        #[cfg(not(feature = "tlb_stats"))]
        {
            // Mentions the amount without evaluating it, so a value computed only for the
            // count is not an unused variable.
            let _ = || $n;
        }
    }};
}

pub(crate) use tlb_count;

/// A start time for a cycle count: the TSC when `tlb_stats` or `trace` is built on x86_64,
/// otherwise nothing, and [`Stopwatch::elapsed`] is then 0.
#[derive(Clone, Copy, Debug)]
pub struct Stopwatch {
    #[cfg(all(target_arch = "x86_64", any(feature = "trace", feature = "tlb_stats")))]
    start: u64,
}

impl Stopwatch {
    /// Starts counting.
    #[inline(always)]
    pub fn start() -> Self {
        Self {
            #[cfg(all(target_arch = "x86_64", any(feature = "trace", feature = "tlb_stats")))]
            start: crate::arch::misc::rdtsc(),
        }
    }

    /// TSC ticks since [`Stopwatch::start`].
    #[inline(always)]
    pub fn elapsed(self) -> u64 {
        #[cfg(all(target_arch = "x86_64", any(feature = "trace", feature = "tlb_stats")))]
        return crate::arch::misc::rdtsc().wrapping_sub(self.start);
        #[cfg(not(all(target_arch = "x86_64", any(feature = "trace", feature = "tlb_stats"))))]
        0
    }
}

/// Invalidates one page in this CPU's TLB (`PageFlush::flush`: `invlpg`, plus `invpcid` for
/// the user PCID with KPTI and PCID) and counts it as `flush_page`.
#[inline(always)]
pub fn flush_page(flush: rmm::PageFlush<RmmA>) {
    flush.flush();
    tlb_count!(FlushPage);
}

/// Accounts for a finished `Flusher` batch of `pages` queued entries: `targets` other CPUs
/// made to flush and acknowledged after `wait_cycles` TSC ticks, and whether this CPU flushed
/// its own TLB (`local`). Counts it, and records `mm.tlb_flush`.
#[inline(always)]
pub fn flush_batch(targets: u32, pages: usize, local: bool, wait_cycles: u64) {
    let pages = pages as u64;
    tlb_count!(FlushBatches);
    tlb_count!(FlushBatchPages, pages);
    if local {
        tlb_count!(FlushFull);
    }
    if targets > 0 {
        tlb_count!(Shootdowns);
        tlb_count!(ShootdownTargets, u64::from(targets));
        tlb_count!(ShootdownPages, pages);
        tlb_count!(ShootdownWaitCycles, wait_cycles);
    }
    crate::trace::trace_event!(
        ferric_trace::kernel::MM_TLB_FLUSH,
        targets,
        pages,
        local,
        wait_cycles
    );
}

/// Accounts for a shootdown request this CPU answered with a full flush.
#[inline(always)]
pub fn shootdown_handled() {
    tlb_count!(ShootdownsHandled);
    tlb_count!(FlushFull);
}

/// Accounts for an IOMMU domain operation that invalidated the IOTLB of `units` units for
/// `bytes` bytes at `iova` of domain `did`, in `cycles` TSC ticks. Counts it, and records
/// `iommu.iotlb_flush`. Nothing is counted when no unit was invalidated.
#[cfg(iommu_enabled)]
#[cfg_attr(not(feature = "trace"), expect(unused_variables))]
#[inline(always)]
pub fn iotlb_flush(did: u16, iova: u64, bytes: u64, units: usize, cycles: u64) {
    if units == 0 {
        return;
    }
    tlb_count!(IotlbFlushes);
    tlb_count!(IotlbUnitFlushes, units as u64);
    tlb_count!(IotlbBytes, bytes);
    tlb_count!(IotlbCycles, cycles);
    crate::trace::trace_event!(
        ferric_trace::kernel::IOMMU_IOTLB_FLUSH,
        did,
        iova,
        bytes,
        units,
        cycles
    );
}
