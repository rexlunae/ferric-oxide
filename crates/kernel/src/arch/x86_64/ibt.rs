//! Ferric Oxide: kernel indirect branch tracking (Intel CET IBT) for x86_64.
//!
//! With IBT on, an indirect `call` or `jmp` in supervisor mode, an interrupt or exception
//! delivered through the IDT, and `SYSCALL` must all land on an `endbr64` instruction, or the CPU
//! raises a control-protection exception (#CP, vector 21). That stops most jump- and
//! call-oriented gadget chains.
//!
//! The `ibt` CPU feature (`arch.x86_64.features.ibt` in the kernel config, `kernel.cpu.ibt` in
//! `ferric.toml`) chooses: `never` builds it out, `always` requires it, `auto` uses it when
//! `CPUID.(EAX=7,ECX=0):EDX[20]` says the CPU has it. When it is not `never`:
//!
//! - The kernel Makefile builds every crate, including `core`, `alloc` and `compiler_builtins`,
//!   with `-Z cf-protection=branch`, so the compiler puts `endbr64` at every function that may be
//!   reached indirectly. It also passes `-C jump-tables=no`: with IBT, LLVM reaches jump-table
//!   targets with `notrack jmp`, and the kernel keeps `notrack` off (`NO_TRACK_EN` clear) so it
//!   cannot be used to skip the check.
//! - Hand-written entry points get `endbr64` from `ibt_endbr!` (`macros.rs`): the
//!   interrupt and exception stubs (`interrupt/handler.rs`), the generic IRQ stubs
//!   (`x86_shared/interrupt/irq.rs`), the `SYSCALL` entry (`interrupt/syscall.rs`), and the
//!   kernel and AP entry points (`x86_shared/start.rs`).
//! - [`init_cpu`](crate::arch::ibt::init_cpu) writes `IA32_S_CET` with only `ENDBR_EN` and sets CR4.CET on every CPU, after
//!   every other CR4 change, since `x86::controlregs::cr4` drops bits it does not know (CET
//!   among them) and a later read-modify-write would turn IBT off again.
//!
//! Shadow stacks and user-mode CET (`IA32_U_CET`) stay off. A `ret` is not checked by IBT, so
//! code reached only by `ret` (`enter_usermode`, the usercopy fault fixup) needs no `endbr64`.
//!
//! The CPUID, MSR and #CP decoding logic lives in the host-tested `ferric-cet` crate.

use core::arch::x86_64::CpuidResult;

use ferric_cet::{Feature, Policy};

/// Whether the kernel was built for IBT, i.e. hand-written entry points start with `endbr64`.
pub const COMPILED: bool = cfg!(cpu_feature_always = "ibt") || cfg!(cpu_feature_auto = "ibt");

/// The configured policy.
const POLICY: Policy = if cfg!(cpu_feature_always = "ibt") {
    Policy::Always
} else if cfg!(cpu_feature_auto = "ibt") {
    Policy::Auto
} else {
    Policy::Never
};

/// `CPUID.(EAX=7,ECX=0)`, where the CET feature bits are, or `None` on CPUs without leaf 7.
pub(super) fn cpuid_7_0() -> Option<CpuidResult> {
    // Leaf 7 only exists when the maximum basic leaf reaches it.
    let max_leaf = core::arch::x86_64::__cpuid(0).eax;
    (max_leaf >= 7).then(|| core::arch::x86_64::__cpuid_count(7, 0))
}

/// Whether this CPU supports IBT.
fn cpu_has_ibt() -> bool {
    cpuid_7_0().is_some_and(|leaf| ferric_cet::has_ibt(leaf.edx))
}

/// Decides whether to enable IBT on the current CPU.
///
/// # Panics
///
/// When the policy is `always` and the CPU does not support IBT.
fn decide() -> bool {
    if !COMPILED {
        return false;
    }
    match ferric_cet::decide(Feature::Ibt, POLICY, cpu_has_ibt()) {
        Ok(on) => on,
        Err(err) => {
            panic!("IBT: {err}; rebuild with kernel.cpu.ibt = \"auto\" or \"never\"")
        }
    }
}

/// Whether indirect branch tracking is on for the current CPU. Shadow-stack enabling reads this
/// so the combined `IA32_S_CET` value keeps `ENDBR_EN` set when IBT is also on (see
/// `shstk::enable_params`). Cheap: it re-reads CPUID, and [`init_cpu`] has already applied the
/// same decision, so `always`-on-an-unsupported-CPU has already panicked by the time this runs.
pub fn enabled_on_this_cpu() -> bool {
    decide()
}

/// Stops early with a clear message when the build requires IBT and the CPU lacks it. Called on
/// the BSP right after the serial console comes up.
///
/// # Panics
///
/// When the policy is `always` and the CPU does not support IBT.
pub fn check() {
    let _ = decide();
}

/// Turns on supervisor indirect branch tracking on the current CPU, if the policy and the CPU
/// allow it, and logs the outcome on the BSP.
///
/// Must run after paging is up (CR0.WP set) and after every other CR4 change on this CPU.
///
/// # Panics
///
/// When the policy is `always` and the CPU does not support IBT, or when it does but CR0.WP is
/// clear, so CR4.CET cannot be set.
pub fn init_cpu(bsp: bool) {
    if !decide() {
        if bsp && COMPILED {
            crate::info!("IBT: kernel indirect branch tracking unavailable on this CPU");
        }
        return;
    }

    let enabled: u32;
    // SAFETY: CPUID reports CET_IBT, so IA32_S_CET exists. Its value has only ENDBR_EN: no
    // shadow stacks, and `notrack` stays ineffective; IA32_U_CET keeps its reset value of zero,
    // so user space gets no CET. CR4.CET then makes the CPU check that indirect branches land on
    // `endbr64`. The build guarantees that every function that can be reached indirectly starts
    // with one (`-Z cf-protection=branch`, no jump tables, and `ibt_endbr!` in hand-written entry
    // points), so no valid control flow faults. Setting CR4.CET while CR0.WP is clear raises #GP,
    // so the asm checks CR0.WP first and reports whether it set the bit. CR4 is read and written
    // raw because `x86::controlregs` does not know bit 23.
    unsafe {
        x86::msr::wrmsr(ferric_cet::IA32_S_CET, ferric_cet::S_CET_KERNEL_IBT);
        core::arch::asm!(
            "xor {ok:e}, {ok:e}",
            "mov {tmp}, cr0",
            "bt {tmp}, {wp}",
            "jnc 2f",
            "mov {tmp}, cr4",
            "bts {tmp}, {cet}",
            "mov cr4, {tmp}",
            "mov {ok:e}, 1",
            "2:",
            ok = out(reg) enabled,
            tmp = out(reg) _,
            wp = const ferric_cet::CR0_WP_BIT,
            cet = const ferric_cet::CR4_CET_BIT,
            options(nostack),
        );
    }
    if enabled == 0 {
        failed("CR0.WP is clear");
        return;
    }

    if bsp {
        crate::info!("IBT: kernel indirect branch tracking enabled");
    } else {
        crate::debug!("IBT: enabled on this CPU");
    }
}

/// Reports a CPU that has IBT but could not turn it on: fatal for `always`, a warning for `auto`.
fn failed(why: &str) {
    if POLICY == Policy::Always {
        panic!("IBT: could not enable kernel indirect branch tracking: {why}");
    }
    crate::warn!("IBT: kernel indirect branch tracking left off: {why}");
}
