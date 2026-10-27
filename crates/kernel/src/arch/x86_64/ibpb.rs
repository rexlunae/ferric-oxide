//! Ferric Oxide: indirect branch prediction barriers (IBPB) between user address spaces.
//!
//! Without a barrier, indirect branch predictions that one process trains stay in the branch
//! target buffer when the CPU switches to another process, which can then be steered into
//! speculatively running gadgets of the attacker's choosing (Spectre v2 user-to-user,
//! CVE-2017-5715). Writing bit 0 of `IA32_PRED_CMD` (MSR 0x49) discards those predictions.
//!
//! The kernel issues one when a CPU switches to a user address space other than the last one it
//! ran: in `percpu::switch_arch_hook` for context switches and in `Context::set_addr_space` for a
//! context that changes address space (exec). Switches to kernel and idle contexts, and between
//! threads of one process, issue none. [`policy`](crate::arch::ibpb::policy) holds the decision
//! as pure functions; this module adds the boot-time choice, the per-CPU record and a count.
//!
//! The `ibpb` CPU feature (`arch.x86_64.features.ibpb` in the kernel config, `kernel.cpu.ibpb`
//! in `ferric.toml`) chooses: `never` builds it out, `auto` turns it on when the CPU reports
//! IBPB support ([`Cpuid::supports_ibpb`](crate::arch::ibpb::policy::Cpuid::supports_ibpb)), and
//! `always` also panics at boot on CPUs without it. Like KPTI the choice is made once, on the
//! bootstrap processor.

pub mod policy;

use alloc::sync::{Arc, Weak};
use core::{
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::context::memory::AddrSpaceWrapper;

/// The prediction command MSR.
pub const IA32_PRED_CMD: u32 = 0x49;

/// The `IA32_PRED_CMD` bit that issues an IBPB.
pub const PRED_CMD_IBPB: u64 = 1;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Barriers issued on all CPUs since boot.
static BARRIERS: AtomicU64 = AtomicU64::new(0);

/// Whether IBPB support was compiled in.
const fn compiled() -> bool {
    cfg!(cpu_feature_always = "ibpb") || cfg!(cpu_feature_auto = "ibpb")
}

/// Whether context switches issue barriers.
#[inline]
pub fn enabled() -> bool {
    compiled() && ENABLED.load(Ordering::Relaxed)
}

/// The CPUID values that decide support, read from the current CPU.
fn read_cpuid() -> policy::Cpuid {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    let max_basic_leaf = __cpuid(0).eax;
    let max_extended_leaf = __cpuid(0x8000_0000).eax;
    policy::Cpuid {
        max_basic_leaf,
        leaf7_edx: if max_basic_leaf >= 7 {
            __cpuid_count(7, 0).edx
        } else {
            0
        },
        max_extended_leaf,
        ext8_ebx: if max_extended_leaf >= 0x8000_0008 {
            __cpuid(0x8000_0008).ebx
        } else {
            0
        },
    }
}

/// Decides whether context switches issue barriers, on the bootstrap processor before any user
/// code runs. Returns the decision.
///
/// # Panics
///
/// When the `ibpb` feature is `always` and the CPU does not report IBPB support.
pub fn decide() -> bool {
    if !compiled() {
        return false;
    }
    let supported = read_cpuid().supports_ibpb();
    if cfg!(cpu_feature_always = "ibpb") && !supported {
        panic!(
            "IBPB: the CPU does not report IBPB support; rebuild with kernel.cpu.ibpb = \"auto\" or \"never\""
        );
    }
    ENABLED.store(supported, Ordering::Relaxed);
    if supported {
        crate::info!("IBPB: barriers on switches between user address spaces");
    } else {
        crate::info!("IBPB: not supported by this CPU, switches issue no barrier");
    }
    supported
}

/// Decides whether switching this CPU to `next` (`None` for a kernel or idle context) needs a
/// barrier, given `last_user`, the CPU's record of the last user address space it ran. When it
/// does, records `next` and counts the barrier; the caller must then write [`PRED_CMD_IBPB`] to
/// [`IA32_PRED_CMD`] before `next` runs user code. Always false while barriers are off.
///
/// `last_user` holds a weak reference, which keeps the address space's allocation alive, so a
/// new address space can never reuse the address of the recorded one and be mistaken for it.
pub fn needed_for_switch(
    last_user: &Cell<Option<Weak<AddrSpaceWrapper>>>,
    next: Option<&Arc<AddrSpaceWrapper>>,
) -> bool {
    if !enabled() {
        return false;
    }
    let last = last_user.take();
    let needed = policy::needs_barrier(last.as_ref().map(Weak::as_ptr), next.map(Arc::as_ptr));
    match next {
        Some(next) if needed => {
            last_user.set(Some(Arc::downgrade(next)));
            BARRIERS.fetch_add(1, Ordering::Relaxed);
        }
        _ => last_user.set(last),
    }
    needed
}

/// The number of barriers issued on all CPUs since boot, shown in `sys:stat`.
pub fn barriers_issued() -> u64 {
    BARRIERS.load(Ordering::Relaxed)
}
