//! Ferric Oxide: kernel shadow stacks (Intel CET SS) for x86_64.
//!
//! With supervisor shadow stacks on, every `call` also pushes the return address to a separate,
//! write-protected stack, and every `ret` checks the two copies match, raising a
//! control-protection exception (#CP) when they do not. That stops return-oriented gadget
//! chains.
//!
//! The `shstk` CPU feature (`arch.x86_64.features.shstk` in the kernel config, `kernel.cpu.shstk`
//! in `ferric.toml`) is `never` (the default, which builds all of this out and logs nothing),
//! `auto` (enable where the CPU supports it) or `always` (require it; boot panics if the CPU
//! lacks it). Both `auto` and `always` need the `self_modifying` kernel feature (build.rs turns
//! an `auto` into `never` otherwise); the build then sets `cfg(shstk_compiled)`. At boot
//! [`decision`] applies the policy to `CPUID.(EAX=7,ECX=0):ECX[7]` on the bootstrap processor,
//! and [`report`] logs the result.
//!
//! # What this step does
//!
//! The shadow stacks are allocated and kept consistent (see below), and CR4.CET with
//! `IA32_S_CET.SH_STK_EN` is turned on per-CPU once SSP is loaded, at a point with no pending
//! returns ([`enable_params`]/[`confirm_enabled`], called from the startup path right before the
//! diverging `kmain`/`kmain_ap` calls). When the decision is on:
//!
//! - **Allocation.** Every [`Kstack`](crate::context::context::Kstack) gets a [`ShadowStack`]:
//!   each context's kernel stack, the bootstrap processor's kmain stack, each application
//!   processor's boot stack, the per-CPU backup (IST) stacks and the KPTI entry stacks, which are
//!   all `Kstack`s. It is freed with the stack.
//! - **Mapping.** Shadow-stack pages need their own page-table entries, present with R/W = 0 and
//!   Dirty = 1 ([`ferric_cet::shstk::SHADOW_STACK_PTE_FLAGS`]), so they cannot live in the
//!   writable physical memory map. They are mapped in a window in the upper half of the kernel
//!   heap's 512 GiB ([`consts::shadow_stack_window_offset`](super::consts)). The window has one
//!   16 KiB slot for every 64 KiB-aligned physical block, found from the kernel stack's physical
//!   address ([`ferric_cet::shstk::SlotWindow`]), so no allocator is needed and slots never
//!   collide. The 8 KiB shadow stack sits in the upper half of its slot; the lower half stays
//!   unmapped, so an overflowing shadow stack page-faults rather than running into another one.
//!   The window covers kernel stacks below 1 TiB of physical address: [`decision`] stays off
//!   on machines with RAM above that, and [`ShadowStack::for_kstack`] refuses such stacks. The kernel half of the page table is shared by every address space, so the
//!   mappings are visible everywhere at once.
//! - **Tokens.** The highest entry of each shadow stack holds its supervisor shadow-stack token
//!   (its own address, not busy), which the CPU checks when it loads `SSP` from `IA32_PL0_SSP`
//!   or the interrupt SSP table. It is written, like a new context's initial entries, with
//!   `WRSS` through the shadow-stack mapping once CET is on for the running CPU
//!   ([`ShadowStack`]'s `write_entry`). Before that (the boot stacks, and boots where CET stays
//!   off) the kernel writes through the physical memory map instead. The physical memory map's
//!   alias of every shadow-stack frame stays writable, so a kernel write primitive can still
//!   forge a shadow stack through it; removing the alias needs the physical map's huge pages
//!   split around each frame, which is not done yet (see "What this step does not do").
//! - **SSP MSRs.** [`init_cpu`] points `IA32_INTERRUPT_SSP_TABLE_ADDR` at the CPU's table in its
//!   processor control region, whose IST 1 entry ([`set_ist_ssp`]) is the backup stack's token,
//!   and zeroes `IA32_PL0_SSP` to `IA32_PL2_SSP`. [`set_entry_ssp`] then points `IA32_PL0_SSP` at
//!   the next context's token on every context switch, as the TSS stack follows the context. The
//!   writes only happen when the decision is on, and have no effect until CET is enabled.
//! - **Context switch.** `switch_to_inner` (`context/arch/x86_64.rs`) saves the outgoing
//!   context's `SSP` and switches to the incoming one with the token protocol: `RDSSP` reads the
//!   current `SSP`; the outgoing context saves `SSP - 8`, where `SAVEPREVSSP` will leave its
//!   restore token; `RSTORSSP` checks the incoming context's restore token at its saved address,
//!   switches `SSP` there and replaces the token with a "previous SSP" token; `SAVEPREVSSP` pops
//!   that and writes the outgoing context's restore token. `SSP` then points at the return
//!   address of the incoming context's own `call` into the switch, which `switch_finish_hook`'s
//!   `ret` pops along with the normal stack's copy. These instructions raise #UD while CET is
//!   off, so the sequence is an `alternative!` for the `shstk` feature, which
//!   `alternative::early_init` selects once [`enabled_for_cpu`] is true; otherwise the slot
//!   holds NOPs.
//! - **New contexts.** `setup_initial_call` builds a new context's kernel stack by hand, with
//!   `func` and, for user contexts, `enter_usermode` as return addresses no `call` pushed. It
//!   writes the matching shadow stack ([`ShadowStack::install_initial`],
//!   [`ferric_cet::shstk::InitialShadowStack`]): a restore token, `func`, `enter_usermode` and
//!   the supervisor token, and saves the restore token's address as the context's `SSP`. The
//!   first switch to it then lands on `func`, `func`'s `ret` on `enter_usermode`, and `SSP` is
//!   back at the supervisor token when the context leaves for user space.
//!
//! - **`SYSCALL` path.** Hardware interrupts from ring 3 reload `SSP` from `IA32_PL0_SSP` (or the
//!   IST SSP table) automatically, but `SYSCALL` does not. The entry stub marks the supervisor
//!   token busy with `SETSSBSY` after switching to the kernel stack, and the `enter_usermode`
//!   exit path clears it with `RDSSPQ`/`CLRSSBSY` before returning to ring 3, both as
//!   `alternative!`s for the `shstk` feature.
//! - **Non-local returns.** `recover_and_efault` restarts the faulting `rep movsb` as a no-op
//!   (`rcx = 0`) and returns normally, keeping both stacks consistent instead of editing a saved
//!   RIP, so the usercopy fault path works with CET on.
//! - **Saved-RIP edits.** A handler that resumes interrupted ring-0 code at a different RIP
//!   (the `rdmsr_safe`/`wrmsr_safe` #GP recovery, and the breakpoint handler for a ring-0
//!   `int3`) also moves the return address in the CPU's shadow-stack interrupt frame, with
//!   [`retarget_kernel_return`], or `IRETQ` would raise #CP. The frame is found by its layout
//!   ([`ferric_cet::shstk::find_supervisor_interrupt_frame`]) between `SSP` and the top of the
//!   running shadow stack, and rewritten with `WRSS`. Interrupts from ring 3 push no shadow
//!   frame, so their RIP edits need nothing.
//! - **Separate IST stacks.** The NMI, double-fault and machine-check handlers run on their own
//!   IST stacks, each with its own shadow stack and SSP-table entry, so a fault while the normal
//!   shadow stack is inconsistent still has a valid shadow stack to switch to.
//!
//! # What this step does not do (deferred, hardware-unvalidated)
//!
//! - **KPTI.** KPTI's user-mode page tables do not map the kernel shadow stacks, and switching
//!   SSP alongside RSP on the KPTI entry trampoline is not implemented. Shadow stacks are
//!   therefore left off whenever KPTI is on ([`enabled_for_cpu`] is `decision() && !kpti`), so on
//!   a Meltdown-affected Intel CPU with `kpti = "auto"` the stacks are allocated but CET stays
//!   off. `shstk = "always"` with KPTI not `never` is rejected at config time and panics at boot.
//! - **Physical-map alias.** Shadow-stack frames are still mapped writable in the physical memory
//!   map. The kernel itself no longer writes through it once CET is on for a CPU, but an attacker
//!   with a kernel write primitive could. Removing it means splitting the 1 GiB / 2 MiB pages of
//!   the physical map around each frame (or carving a frame pool out of it at boot), which `rmm`
//!   does not support yet.
//! - **Hardware validation.** None of the CET paths (enable, SYSCALL token handling, the
//!   single-step IRETQ exit, the saved-RIP shadow sync, `WRSS` writes) has run on CET hardware;
//!   QEMU does not emulate CET and the CI runners' KVM does not expose it.
//!
//! The `SYSCALL` single-step fallback returns with `IRETQ` and skips the exit `CLRSSBSY`, leaving
//! `SSP` at the busy supervisor token as after an interrupt from ring 3; `IRETQ` to ring 3 frees
//! that token and checks no shadow frame.
//!
//! The CPUID, policy, page-encoding and token logic lives in the host-tested `ferric-cet` crate.

use ferric_cet::{Feature, Policy};
use spin::Once;

#[cfg(shstk_compiled)]
use self::stacks::window_covers_ram;
#[cfg(shstk_compiled)]
pub use self::stacks::*;

/// Whether the kernel is built with shadow-stack support (`kernel.cpu.shstk` is `auto` or
/// `always`). Like `ibt::COMPILED`, it decides whether the #CP handler is installed.
pub const COMPILED: bool = cfg!(shstk_compiled);

/// The configured policy.
const POLICY: Policy = if cfg!(cpu_feature_always = "shstk") {
    Policy::Always
} else if cfg!(cpu_feature_auto = "shstk") {
    Policy::Auto
} else {
    Policy::Never
};

/// The decision, made on first use (on the bootstrap processor, when it allocates its first
/// kernel stack).
static DECISION: Once<bool> = Once::new();

/// Whether this CPU supports shadow stacks.
fn cpu_has_shstk() -> bool {
    super::ibt::cpuid_7_0().is_some_and(|leaf| ferric_cet::has_shstk(leaf.ecx))
}

/// Whether this boot uses shadow stacks: the policy applied to the bootstrap processor's CPUID.
/// Always false when the build has no shadow-stack support.
///
/// "Uses" means they are allocated, mapped and the SSP MSRs kept up to date; CET itself is not
/// turned on yet (see the module documentation).
pub fn decision() -> bool {
    if POLICY == Policy::Never {
        return false;
    }
    *DECISION.call_once(|| {
        match ferric_cet::decide(Feature::ShadowStack, POLICY, cpu_has_shstk()) {
            // `always` on a CPU without shadow stacks: stop with a clear message, like IBT.
            Err(err) => panic!("CET: {err}; rebuild with kernel.cpu.shstk = \"auto\" or \"never\""),
            Ok(on) => on && window_covers_ram(),
        }
    })
}

/// Whether this CPU actually turns CET shadow stacks on, as opposed to only allocating them. It
/// is [`decision`] minus the one combination this step does not yet support: KPTI, whose
/// user-mode page tables do not map the kernel shadow stacks, so an interrupt from user space
/// could not reach its supervisor token (see the module docs). The shadow stacks are still
/// allocated and kept consistent in that case; the CPU just never uses them.
///
/// # Panics
///
/// When the policy is `always` but KPTI is on, so the requirement cannot be met.
#[cfg(shstk_compiled)]
pub fn enabled_for_cpu() -> bool {
    if !decision() {
        return false;
    }
    if crate::arch::kpti::enabled() {
        assert!(
            POLICY != Policy::Always,
            "CET: kernel.cpu.shstk = \"always\" cannot be met with KPTI on; this step does not \
             yet enable shadow stacks alongside KPTI. Use \"auto\", or kernel.cpu.kpti = \"never\""
        );
        return false;
    }
    true
}

/// Without shadow-stack support the CPU never turns them on.
#[cfg(not(shstk_compiled))]
pub fn enabled_for_cpu() -> bool {
    false
}

/// Without shadow-stack support there is no window; [`decision`] never gets this far.
#[cfg(not(shstk_compiled))]
fn window_covers_ram() -> bool {
    false
}

/// Without shadow-stack support there is no shadow frame to move.
#[cfg(not(shstk_compiled))]
pub fn retarget_kernel_return(_cs: usize, _from: usize, _to: usize) -> Result<(), FrameError> {
    Ok(())
}

/// Why [`retarget_kernel_return`] could not move an interrupt's shadow-stack return address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// `SSP` is not inside a shadow stack in the shadow-stack window.
    SspOutsideWindow {
        /// The running CPU's `SSP`.
        ssp: usize,
    },
    /// No interrupt frame returning to `cs:rip` lies between `SSP` and the top of its stack.
    NotFound {
        /// The code segment of the interrupted code.
        cs: usize,
        /// The return address looked for.
        rip: usize,
    },
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SspOutsideWindow { ssp } => {
                write!(f, "SSP {ssp:#x} is not in a kernel shadow stack")
            }
            Self::NotFound { cs, rip } => {
                write!(
                    f,
                    "no shadow-stack interrupt frame returns to {cs:#x}:{rip:#x}"
                )
            }
        }
    }
}

impl core::error::Error for FrameError {}

/// Logs whether the CPU supports shadow stacks and what the policy decided. Called once, on the
/// BSP, before [`enable_cpu`] turns them on. Does nothing when the policy is `never`.
pub fn report() {
    if POLICY == Policy::Never {
        return;
    }
    let supported = cpu_has_shstk();
    let on = decision();
    if supported && !on && !window_covers_ram() {
        warn!("CET: RAM extends beyond the shadow-stack window (1 TiB); shadow stacks stay off");
    }
    let decision = if on { "on" } else { "off" };
    info!("CET: shadow stacks supported={supported} policy={POLICY} decision={decision}");
    if !on {
        // The skip case, including CI on CPUs without CET_SS: nothing is enabled, behaviour
        // matches a build without shadow stacks.
        info!("CET: shadow stacks not enabled on this boot");
        return;
    }
    // `enabled_for_cpu` panics here for `always` + KPTI; `auto` + KPTI falls through to the log.
    if !enabled_for_cpu() {
        info!(
            "CET: supervisor shadow stacks allocated but left off (KPTI is on; enabling shadow \
             stacks alongside KPTI is not yet supported)"
        );
    } else {
        info!("CET: supervisor shadow stacks allocated for every kernel stack; enabling CET");
    }
}

#[cfg(shstk_compiled)]
mod stacks {
    use core::fmt;

    use ferric_cet::shstk::{
        self, InitialShadowStack, LayoutError, SlotWindow, ENTRY_BYTES, INTERRUPT_SSP_TABLE_ENTRIES,
    };

    use super::FrameError;
    use rmm::Arch;

    use crate::{
        arch::consts::{shadow_stack_window_offset, SHADOW_STACK_WINDOW_SIZE},
        memory::{
            allocate_p2frame, deallocate_p2frame, Enomem, Frame, KernelMapper, PageFlags,
            PhysicalAddress, RmmA, VirtualAddress, PAGE_SIZE,
        },
    };

    /// Frame order of a shadow stack: 8 KiB, 1024 return addresses.
    const ORDER: u32 = 1;

    /// Size of a shadow stack in bytes.
    const SIZE: usize = PAGE_SIZE << ORDER;

    /// Log2 of a kernel stack's size and alignment (64 KiB, see `Kstack`).
    const KSTACK_SHIFT: u32 = 16;

    /// Log2 of a window slot's size: the shadow stack plus an unmapped guard of the same size.
    const SLOT_SHIFT: u32 = 14;

    const _: () = assert!(SIZE * 2 == 1 << SLOT_SHIFT);

    /// Whether every kernel stack can have a slot: all RAM lies below the window's limit. Kernel
    /// stacks come from RAM, so a machine with more would fail to allocate some of them.
    pub(super) fn window_covers_ram() -> bool {
        crate::startup::memory::ram_end() as u64 <= window().key_limit()
    }

    /// The shadow-stack window, from the kernel's memory map.
    fn window() -> SlotWindow {
        SlotWindow {
            base: shadow_stack_window_offset() as u64,
            len: SHADOW_STACK_WINDOW_SIZE as u64,
            key_shift: KSTACK_SHIFT,
            slot_shift: SLOT_SHIFT,
        }
    }

    /// Page flags of a mapped shadow-stack page: [`shstk::SHADOW_STACK_PTE_FLAGS`] in `rmm`
    /// terms. Global like the rest of the kernel half (KPTI turns global pages off itself).
    fn page_flags() -> PageFlags<RmmA> {
        PageFlags::new()
            .write(false)
            .execute(false)
            .global(true)
            .custom_flag((shstk::PTE_ACCESSED | shstk::PTE_DIRTY) as usize, true)
    }

    /// Why a shadow stack could not be set up.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum ShadowStackError {
        /// The kernel stack has no slot in the window, or a layout was invalid.
        Layout(LayoutError),
        /// No physical memory for the shadow stack.
        NoFrames,
        /// No physical memory for the page tables mapping the page at `addr`.
        NoPageTables {
            /// The page being mapped.
            addr: usize,
        },
    }

    impl From<LayoutError> for ShadowStackError {
        fn from(err: LayoutError) -> Self {
            Self::Layout(err)
        }
    }

    impl fmt::Display for ShadowStackError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Layout(_) => f.write_str("invalid shadow-stack layout"),
                Self::NoFrames => f.write_str("out of memory for a shadow stack"),
                Self::NoPageTables { addr } => {
                    write!(f, "out of memory for page tables mapping {addr:#x}")
                }
            }
        }
    }

    impl core::error::Error for ShadowStackError {
        fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
            match self {
                Self::Layout(err) => Some(err),
                Self::NoFrames | Self::NoPageTables { .. } => None,
            }
        }
    }

    /// `Kstack::new` can only report running out of memory; a missing shadow stack is that.
    impl From<ShadowStackError> for Enomem {
        fn from(_: ShadowStackError) -> Self {
            Enomem
        }
    }

    /// A kernel stack's supervisor shadow stack, mapped in the shadow-stack window, with its
    /// supervisor token in the highest entry. Unmapped and freed on drop.
    pub struct ShadowStack {
        frame: Frame,
        /// Lowest mapped address.
        base: usize,
        /// End (exclusive) of the mapping, the top of the slot.
        top: usize,
    }

    impl ShadowStack {
        /// Allocates and maps the shadow stack for the kernel stack at physical address
        /// `kstack`, or returns `None` when this boot does not use shadow stacks
        /// ([`decision`](super::decision)).
        ///
        /// # Errors
        ///
        /// [`ShadowStackError::Layout`] when the stack has no slot in the window (it is not
        /// 64 KiB-aligned or lies above 1 TiB), [`ShadowStackError::NoFrames`] and
        /// [`ShadowStackError::NoPageTables`] when memory runs out.
        pub fn for_kstack(kstack: PhysicalAddress) -> Result<Option<Self>, ShadowStackError> {
            if !super::decision() {
                return Ok(None);
            }
            let top = window().slot_top(kstack.data() as u64)? as usize;
            let base = top.checked_sub(SIZE).ok_or(LayoutError::Overflow)?;
            let frame = allocate_p2frame(ORDER, true).ok_or(ShadowStackError::NoFrames)?;
            let stack = Self { frame, base, top };
            // Mapped first: once CET is on, `write_entry` writes through this mapping.
            stack.map()?;
            stack.write_entry(
                stack.supervisor_token(),
                shstk::supervisor_token(stack.supervisor_token() as u64)?,
            );
            Ok(Some(stack))
        }

        /// Address of the supervisor shadow-stack token, the value for `IA32_PL0_SSP` and the
        /// interrupt SSP table.
        pub fn supervisor_token(&self) -> usize {
            // `top` is at least `SIZE` above 0 (see `for_kstack`).
            self.top.wrapping_sub(ENTRY_BYTES as usize)
        }

        /// Writes the initial shadow stack of a new context whose hand-built kernel stack
        /// returns to `returns` in order, and returns the `SSP` to save for it (see
        /// [`InitialShadowStack`]).
        ///
        /// # Errors
        ///
        /// [`ShadowStackError::Layout`] for more than
        /// [`ferric_cet::shstk::MAX_INITIAL_RETURNS`] return addresses.
        pub fn install_initial(&self, returns: &[u64]) -> Result<usize, ShadowStackError> {
            let image = InitialShadowStack::new(self.top as u64, returns)?;
            let ssp = image.ssp() as usize;
            for (addr, &value) in (ssp..).step_by(ENTRY_BYTES as usize).zip(image.entries()) {
                self.write_entry(addr, value);
            }
            // The supervisor token is written busy, so the context's first return to user space
            // (through `enter_usermode`, which runs `CLRSSBSY`) finds it busy, as it would after a
            // `SYSCALL` entry's `SETSSBSY`. For a context entered by `SETSSBSY` or by hardware
            // interrupt delivery, the token is busy the same way; between the two, it is clear.
            self.write_entry(
                self.supervisor_token(),
                shstk::supervisor_token(self.supervisor_token() as u64)?
                    | shstk::SUPERVISOR_TOKEN_BUSY,
            );
            Ok(ssp)
        }

        /// The mapped shadow-stack range, `base..top`: the lowest mapped address and the
        /// exclusive top (where the supervisor token sits at `top - 8`). Used by the boot
        /// self-test to check `SSP` lands inside the running stack's shadow.
        pub fn region(&self) -> core::ops::Range<usize> {
            self.base..self.top
        }

        /// Writes one entry. Once CET shadow stacks are on for the running CPU this is a `WRSS`
        /// through the shadow-stack mapping ([`wrss`]); before that (or on boots where CET stays
        /// off) `WRSS` would raise #UD, so it writes through the physical memory map, whose alias
        /// of the frame is writable.
        ///
        /// # Panics
        ///
        /// When `addr` is not an aligned entry of this shadow stack.
        fn write_entry(&self, addr: usize, value: u64) {
            assert!(
                (self.base..self.top).contains(&addr) && addr.is_multiple_of(8),
                "shadow-stack entry {addr:#x} outside its stack"
            );
            if current_ssp().is_some() {
                wrss(addr, value);
                return;
            }
            // Cannot wrap: `addr` is at or above `base` (asserted).
            let phys = self.frame.base().add(addr.wrapping_sub(self.base));
            let ptr = RmmA::phys_to_virt(phys).data() as *mut u64;
            // SAFETY: the frame belongs to this shadow stack alone and the physical memory map
            // covers it; the assertion keeps the 8-byte-aligned entry inside the frame.
            unsafe { ptr.write_volatile(value) };
        }

        /// Maps the frame at `base..top` with the shadow-stack page encoding. On failure the
        /// caller drops `self`, which unmaps whatever was mapped.
        fn map(&self) -> Result<(), ShadowStackError> {
            // `rmm` has no notion of shadow-stack pages; check its encoding is the CPU's.
            assert!(
                shstk::is_supervisor_shadow_stack_pte(page_flags().data() as u64),
                "rmm page flags do not encode a supervisor shadow-stack page"
            );
            let mut mapper = KernelMapper::lock_rw();
            for (i, virt) in self.pages().enumerate() {
                let phys = self.frame.next_by(i).base();
                // SAFETY: the slot is this kernel stack's alone and was unmapped (slots are keyed
                // by the stack's physical address); the frame is ours.
                let flush = unsafe { mapper.map_phys(virt, phys, page_flags(), 0) }
                    .ok_or(ShadowStackError::NoPageTables { addr: virt.data() })?;
                flush.flush();
            }
            Ok(())
        }

        /// The mapped pages, lowest first.
        fn pages(&self) -> impl Iterator<Item = VirtualAddress> {
            (self.base..self.top)
                .step_by(PAGE_SIZE)
                .map(VirtualAddress::new)
        }

        /// Unmaps the pages `map` mapped and flushes them from every CPU's TLB.
        fn unmap(&self) {
            let mut mapper = KernelMapper::lock_rw();
            for virt in self.pages() {
                // SAFETY: the slot is this stack's alone, so any page mapped there is ours, and
                // nothing uses this stack any more.
                if let Some((_, _, _, flush)) = unsafe { mapper.unmap_phys(virt) } {
                    flush.flush();
                }
            }
            drop(mapper);
            crate::percpu::shootdown_tlb_ipi(None);
        }
    }

    impl Drop for ShadowStack {
        fn drop(&mut self) {
            // Mirror `Kstack`'s running-stack assertion: never free the shadow stack the CPU is
            // currently using as `SSP`, which would corrupt in-flight returns. `current_ssp`
            // returns `None` while CET is off, so this does nothing on the allocated-but-off path.
            if let Some(ssp) = current_ssp() {
                assert!(
                    !self.region().contains(&ssp),
                    "freeing the shadow stack the CPU is running on (SSP {ssp:#x} in {self:?})"
                );
            }
            self.unmap();
            // SAFETY: allocated with this order in `for_kstack`, and unmapped above.
            unsafe { deallocate_p2frame(self.frame, ORDER) };
        }
    }

    impl fmt::Debug for ShadowStack {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "[shadow stack {:#x}..{:#x} at {:?}]",
                self.base, self.top, self.frame
            )
        }
    }

    /// The SSP MSRs the kernel writes.
    #[derive(Clone, Copy, Debug)]
    enum SspMsr {
        Pl0,
        Pl1,
        Pl2,
        InterruptTable,
    }

    /// Writes an SSP MSR. Only called when [`decision`](super::decision) is on, which
    /// [`init_cpu`] checks this CPU agrees with, so the MSRs exist; with CR4.CET clear the CPU
    /// does not use them yet.
    fn write_ssp_msr(msr: SspMsr, value: usize) {
        debug_assert!(value.is_multiple_of(8) && RmmA::virt_is_valid(VirtualAddress::new(value)));
        let msr = match msr {
            SspMsr::Pl0 => shstk::IA32_PL0_SSP,
            SspMsr::Pl1 => shstk::IA32_PL1_SSP,
            SspMsr::Pl2 => shstk::IA32_PL2_SSP,
            SspMsr::InterruptTable => shstk::IA32_INTERRUPT_SSP_TABLE_ADDR,
        };
        // SAFETY: CPUID reports CET_SS on this CPU, so the MSR exists; the value is a canonical,
        // 8-byte-aligned kernel address or 0, which the MSR accepts, and nothing reads it before
        // CET is turned on.
        unsafe { x86::msr::wrmsr(msr, value as u64) };
    }

    /// The per-CPU interrupt SSP table, kept in the processor control region: entry `n` is the
    /// supervisor token of IST stack `n`'s shadow stack, entry 0 is unused.
    pub type InterruptSspTable = [u64; INTERRUPT_SSP_TABLE_ENTRIES];

    /// Records `shadow`'s token as IST stack `ist`'s shadow-stack pointer in `table`. Does
    /// nothing without a shadow stack.
    ///
    /// # Panics
    ///
    /// When `ist` is not between 1 and 7.
    pub fn set_ist_ssp(table: &mut InterruptSspTable, ist: u8, shadow: Option<&ShadowStack>) {
        assert!((1..=7).contains(&ist), "IST index {ist} out of range");
        if let (Some(shadow), Some(entry)) = (shadow, table.get_mut(usize::from(ist))) {
            *entry = shadow.supervisor_token() as u64;
        }
    }

    /// Points `IA32_PL0_SSP`, the shadow-stack pointer for interrupts from user space, at the
    /// next context's kernel shadow stack (0 if it has none). Called on every context switch,
    /// like `gdt::set_tss_stack`. Does nothing unless the decision is on.
    pub fn set_entry_ssp(shadow: Option<&ShadowStack>) {
        if super::decision() {
            write_ssp_msr(SspMsr::Pl0, shadow.map_or(0, ShadowStack::supervisor_token));
        }
    }

    /// Per-CPU setup: points `IA32_INTERRUPT_SSP_TABLE_ADDR` at `table`, this CPU's table in its
    /// processor control region, and zeroes
    /// `IA32_PL0_SSP` to `IA32_PL2_SSP` (rings 1 and 2 are unused; ring 0's follows the
    /// context, see [`set_entry_ssp`]). Runs on every CPU after its PCR is installed. Does
    /// nothing unless the decision is on.
    ///
    /// # Panics
    ///
    /// When the decision is on and this CPU, unlike the bootstrap processor, lacks shadow
    /// stacks.
    pub fn init_cpu(table: &InterruptSspTable) {
        if !super::decision() {
            return;
        }
        assert!(
            super::cpu_has_shstk(),
            "CET: this CPU lacks shadow stacks, unlike the BSP"
        );
        write_ssp_msr(SspMsr::InterruptTable, core::ptr::from_ref(table) as usize);
        write_ssp_msr(SspMsr::Pl0, 0);
        write_ssp_msr(SspMsr::Pl1, 0);
        write_ssp_msr(SspMsr::Pl2, 0);
    }

    /// Set once a CPU has turned CET shadow stacks on, so [`current_ssp`] skips `RDSSPQ`
    /// entirely on boots where no CPU ever does. It stays set for the rest of the boot; whether
    /// a given CPU has them on is what [`current_ssp`] reads.
    static SHSTK_LIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

    /// Whether CET shadow stacks are turned on and in use on this boot, set once the first CPU
    /// enables them. Lets paths like `recover_and_efault` pick the shadow-stack-safe fixup only
    /// when it is needed, and stay byte-for-byte unchanged otherwise.
    pub fn live() -> bool {
        SHSTK_LIVE.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Reads the running CPU's shadow-stack pointer, or `None` while CET shadow stacks are off on
    /// it. Used by the self-test, the drop assertion and [`ShadowStack`]'s writes, which pick
    /// `WRSS` exactly when this is `Some`.
    pub fn current_ssp() -> Option<usize> {
        if !SHSTK_LIVE.load(core::sync::atomic::Ordering::Acquire) {
            return None;
        }
        // `RDSSPQ` sits in the hint-NOP encoding space: it leaves its register unchanged while
        // shadow stacks are off at CPL 0, as on an application processor that has not reached
        // its enable point after the bootstrap processor set `SHSTK_LIVE`. Starting from 0 makes
        // that case read as "off" rather than as garbage. SSP is never 0 while they are on.
        let mut ssp: usize = 0;
        // SAFETY: `RDSSPQ` only copies SSP into the register, or does nothing when shadow stacks
        // are off; it touches no memory or flags.
        unsafe {
            core::arch::asm!("rdsspq {}", inout(reg) ssp, options(nomem, nostack, preserves_flags))
        };
        (ssp != 0).then_some(ssp)
    }

    /// Writes `value` to the shadow-stack entry at `addr` with `WRSSQ`, the store that may target
    /// a shadow-stack page, through the shadow-stack window.
    ///
    /// `WRSS` faults (#PF) on any page that is not a shadow-stack page, so it cannot change
    /// ordinary memory; the assertions keep it to window slots' shadow-stack halves.
    ///
    /// # Panics
    ///
    /// When CET shadow stacks are off on this CPU (`WRSS` would raise #UD), or `addr` is not an
    /// 8-byte-aligned address in the shadow-stack part of a window slot.
    fn wrss(addr: usize, value: u64) {
        assert!(
            current_ssp().is_some(),
            "CET: WRSS to {addr:#x} while shadow stacks are off on this CPU"
        );
        assert!(
            addr.is_multiple_of(ENTRY_BYTES as usize)
                && window()
                    .stack_containing(addr as u64, SIZE as u64)
                    .is_some(),
            "CET: WRSS target {addr:#x} is not a shadow-stack entry in the window"
        );
        // SAFETY: shadow stacks are on for this CPU, and `enable_params` set
        // IA32_S_CET.WR_SHSTK_EN with them (`ferric_cet::s_cet_value`), so WRSSQ is legal at
        // CPL 0. It stores 8 bytes at `addr`, an aligned entry of a window slot's shadow-stack
        // half; the CPU refuses (#PF) the store unless that page is mapped as a supervisor shadow
        // stack, so no ordinary memory can change. It changes no register or flag.
        unsafe {
            core::arch::asm!(
                "wrssq [{addr}], {value}",
                addr = in(reg) addr,
                value = in(reg) value,
                options(nostack, preserves_flags),
            )
        };
    }

    /// Moves the return address of the innermost shadow-stack interrupt frame for interrupted
    /// ring-0 code at `cs:from` to `to`, for a handler that resumes that code at `to` instead
    /// (by editing the saved RIP its `IRETQ` uses). `IRETQ` back to ring 0 pops the frame and
    /// raises #CP unless its return address matches the saved RIP. Does nothing while CET shadow
    /// stacks are off on this CPU. Only for exceptions that do not switch to an IST stack.
    ///
    /// The frame is located from its layout (see
    /// [`ferric_cet::shstk::find_supervisor_interrupt_frame`]) in the entries between `SSP` and
    /// the top of the running shadow stack, so it does not depend on how deep the handler's own
    /// calls are, and is rewritten with `WRSS`.
    ///
    /// # Errors
    ///
    /// [`FrameError::SspOutsideWindow`] when `SSP` is not in a kernel shadow stack, and
    /// [`FrameError::NotFound`] when no frame for `cs:from` lies above it.
    pub fn retarget_kernel_return(cs: usize, from: usize, to: usize) -> Result<(), FrameError> {
        let Some(ssp) = current_ssp() else {
            return Ok(());
        };
        let (_, top) = window()
            .stack_containing(ssp as u64, SIZE as u64)
            .ok_or(FrameError::SspOutsideWindow { ssp })?;
        let not_found = FrameError::NotFound { cs, rip: from };
        // Cannot wrap: `ssp` is inside the stack, below `top`.
        let count = (top as usize).wrapping_sub(ssp) / ENTRY_BYTES as usize;
        // SAFETY: `ssp..top` is the running CPU's live shadow stack (SSP is inside it, and the
        // whole stack is mapped while it is in use), readable with ordinary loads, 8-byte aligned.
        // Only this CPU's calls change it, and only below SSP, so the entries are stable while
        // the slice is alive; it is dropped before the `WRSS` below.
        let entries = unsafe { core::slice::from_raw_parts(ssp as *const u64, count) };
        // The frame holds the 16-bit selector zero-extended; the saved CS slot's upper bits are
        // not architecturally defined.
        let cs_selector = (cs & 0xFFFF) as u16;
        let lip_index =
            shstk::find_supervisor_interrupt_frame(ssp as u64, entries, cs_selector, from as u64)
                .ok_or(not_found)?;
        // Cannot wrap: the entry lies inside `ssp..top`.
        let lip_addr = ssp.wrapping_add(lip_index.wrapping_mul(ENTRY_BYTES as usize));
        wrss(lip_addr, to as u64);
        Ok(())
    }

    /// Whether indirect branch tracking is on for the current CPU, so the combined `IA32_S_CET`
    /// value keeps IBT's `ENDBR_EN` bit set.
    fn ibt_on() -> bool {
        crate::arch::ibt::enabled_on_this_cpu()
    }

    /// The `IA32_PL0_SSP` token and the `IA32_S_CET` value to turn CET shadow stacks on for the
    /// stack `kstack`, or `None` when this boot does not enable them on this CPU
    /// ([`enabled_for_cpu`](super::enabled_for_cpu)) or the stack has no shadow stack.
    ///
    /// Computed in Rust (ordinary calls, run while CET is still off) so the enabling asm in
    /// `start.rs` need only do the MSR and `CR4` writes and `SETSSBSY`, with no call or `ret`
    /// between setting `CR4.CET` and `SETSSBSY`.
    pub fn enable_params(kstack: &crate::context::context::Kstack) -> Option<(usize, u64)> {
        if !super::enabled_for_cpu() {
            return None;
        }
        let token = kstack.shadow_stack()?.supervisor_token();
        Some((token, ferric_cet::s_cet_value(ibt_on(), true)))
    }

    /// Records that CET shadow stacks are now live and confirms them with a self-test, logging
    /// `CET: shadow stacks enabled, self-test passed` on the bootstrap processor. Call right
    /// after the enabling asm, on the stack whose shadow stack is now active.
    ///
    /// The test checks that `SSP` is non-zero and inside `shadow`'s mapped region, then runs a
    /// multi-level call/return round trip and confirms `SSP` is restored exactly. A control-flow
    /// mismatch anywhere raises #CP here, at a known point, rather than silently later. Every
    /// step only reads `SSP` and makes balanced `call`/`ret` pairs, so it cannot leave the live
    /// shadow stack misaligned. The context-switch path (`RSTORSSP`/`SAVEPREVSSP` in
    /// `switch_to_inner`) is exercised by the scheduler's first switch, which faults loudly if it
    /// is wrong.
    ///
    /// # Panics
    ///
    /// When `SSP` is zero, lies outside `shadow`, or the round trip does not restore it.
    pub fn confirm_enabled(shadow: &ShadowStack, bsp: bool) {
        SHSTK_LIVE.store(true, core::sync::atomic::Ordering::Release);

        let ssp = current_ssp().expect("CET: SSP unreadable just after enabling");
        assert!(ssp != 0, "CET: SSP is zero after SETSSBSY");
        assert!(
            shadow.region().contains(&ssp),
            "CET: SSP {ssp:#x} is outside the running stack's shadow {shadow:?}"
        );

        // A `call` pushes its return address to the shadow stack and `ret` pops and verifies it;
        // recurse a few levels so the shadow stack grows and shrinks as it does across any call
        // chain. `ssp` must be identical afterwards.
        let before = ssp;
        shstk_selftest_depth(4);
        let after = current_ssp().expect("CET: SSP unreadable after the call round trip");
        assert_eq!(
            before, after,
            "CET: SSP not restored after a call/return round trip ({before:#x} -> {after:#x})"
        );

        if bsp {
            info!("CET: shadow stacks enabled, self-test passed");
        } else {
            debug!("CET: shadow stacks enabled on this CPU, self-test passed");
        }
    }

    /// Recurses `depth` levels, each a real `call`/`ret`, for the self-test's call round trip.
    #[inline(never)]
    fn shstk_selftest_depth(depth: u32) {
        if depth > 0 {
            shstk_selftest_depth(depth - 1);
        }
        // Keep the recursion from being folded into a loop, so each level is a real call frame.
        core::hint::black_box(depth);
    }
}
