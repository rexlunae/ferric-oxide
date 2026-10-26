//! Ferric Oxide: `/scheme/sys/tlb`, the per-CPU TLB, CR3 and shootdown counters
//! (`tlb_stats`), in the line format of `ferric_trace::tlb`: a mode line saying whether KPTI
//! and PCID are on, then one line per CPU with every counter.
//!
//! The counters only grow; a read is a snapshot, which a benchmark takes before and after a
//! run. Anyone may read it, as anyone may read `stat`: it holds counts, not addresses.

use alloc::{string::String, vec::Vec};
use core::fmt::Write;

use ferric_trace::tlb::{Counter, CpuLine, ModeLine};

use crate::{
    cpu_stats::CpuStatsData, percpu::all_percpu_blocks, sync::CleanLockToken,
    syscall::error::Result,
};

/// The file's contents now.
pub fn resource(_token: &mut CleanLockToken) -> Result<Vec<u8>> {
    let mut text = String::new();
    let _ = write!(text, "{}", mode());
    for block in all_percpu_blocks() {
        let stats = CpuStatsData::from(&block.stats);
        let counters = block.tlb_stats.snapshot_with(&[
            (Counter::Irqs, stats.irq),
            (Counter::ContextSwitches, stats.context_switches),
        ]);
        let _ = write!(
            text,
            "{}",
            CpuLine {
                cpu: block.cpu_id.get(),
                counters,
            }
        );
    }
    Ok(text.into_bytes())
}

/// Whether KPTI, and KPTI with PCID, are on.
fn mode() -> ModeLine {
    #[cfg(target_arch = "x86_64")]
    return ModeLine {
        kpti: crate::arch::kpti::enabled(),
        pcid: crate::arch::kpti::pcid_enabled(),
    };
    #[cfg(not(target_arch = "x86_64"))]
    ModeLine {
        kpti: false,
        pcid: false,
    }
}
