//! Ferric Oxide: indirect branch prediction barriers (IBPB) between user address spaces.
//!
//! Without a barrier, indirect branch predictions that one process trains stay in the branch
//! target buffer when the CPU switches to another process, which can then be steered into
//! speculatively running gadgets of the attacker's choosing (Spectre v2 user-to-user,
//! CVE-2017-5715). Writing bit 0 of `IA32_PRED_CMD` (MSR 0x49) discards those predictions.
//!
//! The kernel issues one, through [`before_switch`](crate::arch::ibpb::before_switch), when a
//! CPU switches to a context that runs user code in an address space other than the last one
//! whose user code it ran: in `context::arch::switch_to` for context switches and in
//! `Context::set_addr_space` for a context that changes address space (exec). Switches to kernel
//! and idle contexts, to kernel contexts that borrow a process's address space (ring workers),
//! and between threads of one process issue none. [`policy`](crate::arch::ibpb::policy) holds
//! the decision as pure functions; this module adds the boot-time choice, the per-CPU state and
//! a count.
//!
//! The `ibpb` CPU feature (`arch.x86_64.features.ibpb` in the kernel config, `kernel.cpu.ibpb`
//! in `ferric.toml`) chooses: `never` builds it out, `auto` turns it on when the CPU reports
//! IBPB support ([`Cpuid::supports_ibpb`](crate::arch::ibpb::policy::Cpuid::supports_ibpb)), and
//! `always` also panics at boot on CPUs without it. Like KPTI the choice is made once, on the
//! bootstrap processor, and every other CPU checks its own support as it starts
//! ([`init_cpu`](crate::arch::ibpb::init_cpu)): under `always` a CPU without IBPB panics, under
//! `auto` it runs without barriers and says so.

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

/// Per-CPU IBPB state, kept in the CPU's [`PercpuBlock`](crate::percpu::PercpuBlock).
pub struct PerCpu {
    /// Whether this CPU issues barriers: they are on and this CPU reports IBPB support.
    active: Cell<bool>,
    /// The last address space whose user code this CPU ran, kept across kernel and idle contexts
    /// so switching back to it needs no barrier. The weak reference keeps the address space's
    /// allocation alive, so a new address space can never reuse its address and be mistaken for
    /// it.
    last_user: Cell<Option<Weak<AddrSpaceWrapper>>>,
}

impl PerCpu {
    /// State for a CPU that has not yet run [`init_cpu`]: it issues no barriers.
    pub const fn new() -> Self {
        Self {
            active: Cell::new(false),
            last_user: Cell::new(None),
        }
    }
}

impl Default for PerCpu {
    fn default() -> Self {
        Self::new()
    }
}

/// Decides whether context switches issue barriers, on the bootstrap processor before any user
/// code runs, and sets up the bootstrap processor's state. Returns the decision.
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
    init_cpu(&crate::percpu::PercpuBlock::current().ibpb);
    supported
}

/// Sets up `state`, the current CPU's IBPB state, after [`decide`] and before the CPU runs any
/// user code. The bootstrap processor's decision is global, but `IA32_PRED_CMD` is written by
/// whichever CPU switches, so each CPU must check that it implements the MSR itself.
///
/// # Panics
///
/// When the `ibpb` feature is `always`, barriers are on and this CPU, unlike the bootstrap
/// processor, does not report IBPB support.
pub fn init_cpu(state: &PerCpu) {
    if !enabled() {
        return;
    }
    let supported = read_cpuid().supports_ibpb();
    if !supported {
        if cfg!(cpu_feature_always = "ibpb") {
            panic!("IBPB: this CPU does not report IBPB support, unlike the bootstrap processor");
        }
        crate::warn!(
            "IBPB: this CPU does not report IBPB support, unlike the bootstrap processor; it issues no barriers"
        );
    }
    state.active.set(supported);
}

/// Issues a barrier when switching the current CPU, whose IBPB state is `state`, to a context
/// whose user code runs in `next` needs one, and records `next` as the last user address space.
/// `next` is `None` for a context that runs no user code: kernel and idle contexts, and ring
/// workers, which borrow a submitter's address space to reach its buffers but never enter user
/// mode. Call it before `next`'s user code runs; it does nothing while barriers are off.
pub fn before_switch(state: &PerCpu, next: Option<&Arc<AddrSpaceWrapper>>) {
    if !compiled() || !state.active.get() {
        return;
    }
    let last = state.last_user.take();
    if !policy::needs_barrier(last.as_ref().map(Weak::as_ptr), next.map(Arc::as_ptr)) {
        state.last_user.set(last);
        return;
    }
    state.last_user.set(next.map(Arc::downgrade));
    // `init_cpu` activated this CPU only if it reports IBPB, so the write should not fault; if
    // the CPU disagrees with its CPUID anyway, it stops issuing barriers rather than crashing.
    match crate::arch::misc::wrmsr_safe(IA32_PRED_CMD, PRED_CMD_IBPB) {
        Ok(()) => {
            BARRIERS.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => state.active.set(false),
    }
}

/// The number of barriers issued on all CPUs since boot, shown in `sys:stat`.
pub fn barriers_issued() -> u64 {
    BARRIERS.load(Ordering::Relaxed)
}
