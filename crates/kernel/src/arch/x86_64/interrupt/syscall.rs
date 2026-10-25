use crate::{
    arch::{gdt, interrupt::InterruptStack},
    ptrace,
    sync::CleanLockToken,
    syscall,
    syscall::flag::{PTRACE_FLAG_IGNORE, PTRACE_STOP_POST_SYSCALL, PTRACE_STOP_PRE_SYSCALL},
};
use core::mem::offset_of;
use x86::{bits64::rflags::RFlags, msr, segmentation::SegmentSelector};

// Ferric Oxide (shadow stacks): SYSCALL does not switch SSP, so on entry the kernel loads its own
// supervisor shadow stack with SETSSBSY (SSP <- IA32_PL0_SSP, which `set_entry_ssp` points at the
// running context's kernel shadow stack, and the token is marked busy) and on return clears it
// with CLRSSBSY (verifies the busy token, clears it, SSP <- 0). Both are patched in only when CET
// shadow stacks are enabled (the "shstk" alternative); they raise #UD while CET is off, and are
// NOPs otherwise, so the default build is byte-for-byte unchanged. See arch/x86_64/shstk.rs.
#[cfg(shstk_compiled)]
macro_rules! shstk_syscall_entry {
    () => {
        $crate::arch::macros::alternative!(feature: "shstk", then: ["setssbsy"], default: [""])
    };
}
#[cfg(not(shstk_compiled))]
macro_rules! shstk_syscall_entry {
    () => {
        ""
    };
}
#[cfg(shstk_compiled)]
macro_rules! shstk_syscall_exit {
    // CLRSSBSY takes the token's address (the current SSP) as a memory operand; RAX is dead here
    // (it is reloaded by `pop_scratch!` further down), so it is free scratch. This clears the
    // busy bit left by the entry SETSSBSY (or by a new context's `install_initial`), leaving the
    // token ready for the next entry, and sets SSP to 0.
    //
    // Only for the SYSRETQ exit: SYSRETQ leaves SSP and the token alone, so the kernel must free
    // the token itself. When the saved RFLAGS has the trap flag set (byte 1, bit 0 of RFLAGS at
    // `SYSCALL_EXIT_TF_BYTE`, checked below), the return goes through the IRETQ fallback instead,
    // and SSP stays at the busy token. That is the state a hardware interrupt from ring 3 leaves
    // at its IRETQ: IRETQ to ring 3 pops no shadow frame (the CPU pushes one only for interrupts
    // from ring 0) and frees the busy supervisor token at SSP itself. Running CLRSSBSY first would
    // leave SSP at 0, and IRETQ would then try to free a token at address 0. The jump stays inside
    // the alternative, so it is position-independent when copied over the NOP slot.
    () => {
        $crate::arch::macros::alternative!(feature: "shstk", then: ["
            test BYTE PTR [rsp + 137], 1
            jnz 1f
            rdsspq rax
            clrssbsy [rax]
        1:
        "], default: [""])
    };
}
/// Offset from RSP at `enter_usermode` (the start of the `InterruptStack`) of the saved RFLAGS
/// byte holding the trap flag (RFLAGS bit 8), written as a literal in `shstk_syscall_exit!`.
#[cfg(shstk_compiled)]
const SYSCALL_EXIT_TF_BYTE: usize = 137;
#[cfg(shstk_compiled)]
const _: () = assert!(
    SYSCALL_EXIT_TF_BYTE
        == offset_of!(InterruptStack, iret) + offset_of!(super::handler::IretRegisters, rflags) + 1
);
#[cfg(not(shstk_compiled))]
macro_rules! shstk_syscall_exit {
    () => {
        ""
    };
}

pub unsafe fn init() {
    unsafe {
        // IA32_STAR[31:0] are reserved.

        // The base selector of the two consecutive segments for kernel code and the immediately
        // succeeding stack (data).
        let syscall_cs_ss_base = (gdt::GDT_KERNEL_CODE as u16) << 3;
        // The base selector of the three consecutive segments (of which two are used) for user code
        // and user data. It points to a 32-bit code segment, which must be followed by a data segment
        // (stack), and a 64-bit code segment.
        let sysret_cs_ss_base = ((gdt::GDT_USER_CODE32_UNUSED as u16) << 3) | 3;
        let star_high = u32::from(syscall_cs_ss_base) | (u32::from(sysret_cs_ss_base) << 16);

        msr::wrmsr(msr::IA32_STAR, u64::from(star_high) << 32);
        msr::wrmsr(msr::IA32_LSTAR, syscall_instruction as *const () as u64);

        // DF needs to be cleared, required by the compiler ABI. If DF were not part of FMASK,
        // userspace would be able to reverse the direction of in-kernel REP MOVS/STOS/(CMPS/SCAS), and
        // cause all sorts of memory corruption.
        //
        // IF needs to be cleared, as the kernel currently assumes interrupts are disabled except in
        // usermode and in kmain.
        //
        // TF needs to be cleared, as enabling userspace-rflags-controlled singlestep in the kernel
        // would be a bad idea.
        //
        // AC it should always be cleared when entering the kernel (and never be set except in usercopy
        // functions), if for some reason AC was set before entering userspace (AC can only be modified
        // by kernel code).
        //
        // The other flags could indeed be preserved and excluded from FMASK, but since they are not
        // used to pass data to the kernel, they might as well be masked with *marginal* security
        // benefits.
        //
        // Flags not included here are IOPL (not relevant to the kernel at all), "CPUID flag" (not used
        // at all in 64-bit mode), RF (not used yet, but DR breakpoints would remain enabled both in
        // user and kernel mode), VM8086 (not used at all), and VIF/VIP (system-level status flags?).

        let mask_critical =
            RFlags::FLAGS_DF | RFlags::FLAGS_IF | RFlags::FLAGS_TF | RFlags::FLAGS_AC;
        let mask_other = RFlags::FLAGS_CF
            | RFlags::FLAGS_PF
            | RFlags::FLAGS_AF
            | RFlags::FLAGS_ZF
            | RFlags::FLAGS_SF
            | RFlags::FLAGS_OF;
        msr::wrmsr(msr::IA32_FMASK, (mask_critical | mask_other).bits());

        let efer = msr::rdmsr(msr::IA32_EFER);
        msr::wrmsr(msr::IA32_EFER, efer | 1);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __inner_syscall_instruction(stack: *mut InterruptStack) {
    unsafe {
        let mut token = CleanLockToken::new();
        let allowed = ptrace::breakpoint_callback(PTRACE_STOP_PRE_SYSCALL, None, &mut token)
            .and_then(|_| ptrace::next_breakpoint().map(|f| !f.contains(PTRACE_FLAG_IGNORE)));

        if allowed.unwrap_or(true) {
            let scratch = &(*stack).scratch;
            // The number dispatched on. The exit record uses this copy: while the call blocks, a
            // tracer can rewrite the saved `rax` (proc's `RegsKind::Int`), and the record must
            // still name the call that ran.
            let nr = scratch.rax;

            // Ferric Oxide: the syscall tracepoints (`kernel.trace`). They sit here, not in
            // `syscall::syscall`, so that large function compiles as it does without tracing.
            // Here the arguments are read from the saved registers, and only while the event is
            // on; keeping `nr` for the exit event costs a register this function saves anyway.
            crate::trace::trace_event!(
                ferric_trace::kernel::SYSCALL_ENTER,
                nr,
                scratch.rdi,
                scratch.rsi,
                scratch.rdx,
                scratch.r10,
            );

            let ret = syscall::syscall(
                nr,
                scratch.rdi,
                scratch.rsi,
                scratch.rdx,
                scratch.r10,
                scratch.r8,
                scratch.r9,
                &mut token,
            );
            crate::trace::trace_event!(ferric_trace::kernel::SYSCALL_EXIT, nr, ret);
            (*stack).scratch.rax = ret;
        }

        ptrace::breakpoint_callback(PTRACE_STOP_POST_SYSCALL, None, &mut token);
    }
}

#[unsafe(naked)]
pub unsafe extern "C" fn syscall_instruction() {
    core::arch::naked_asm!(
    // Ferric Oxide (IBT): SYSCALL enters through IA32_LSTAR like an indirect branch.
    crate::arch::macros::ibt_endbr!(),
    // Yes, this is magic. No, you don't need to understand
    "swapgs;",                    // Swap KGSBASE with GSBASE, allowing fast TSS access.
    "mov gs:[{sp}], rsp;",        // Save userspace stack pointer
    // Ferric Oxide (KPTI): load the kernel page table, using RSP as the scratch register. This
    // clears the shadow and user PCID bits and adds the no-flush bit with PCID (see handler.rs).
    crate::arch::interrupt::handler::kpti_alternative!("
        mov rsp, cr3
        and rsp, -0x1801
        or rsp, gs:[{KPTI_CR3_NOFLUSH}]
        mov cr3, rsp
    "),
    "mov rsp, gs:[{ksp}];",       // Load kernel stack pointer
    // Ferric Oxide (shadow stacks): load this context's kernel shadow stack (NOP unless enabled).
    shstk_syscall_entry!(),
    "push QWORD PTR {ss_sel};",   // Push fake userspace SS (resembling iret frame)
    "push QWORD PTR gs:[{sp}];",  // Push userspace rsp
    "push r11;",                  // Push rflags
    "push QWORD PTR {cs_sel};",   // Push fake CS (resembling iret stack frame)
    "push rcx;",                  // Push userspace return pointer

    // Push context registers
    "push rax;",
    crate::push_scratch!(),
    crate::push_preserved!(),

    // Call inner funtion
    "mov rdi, rsp;",
    "call __inner_syscall_instruction;",

    "
    .globl enter_usermode
    enter_usermode:
    ",

    // Ferric Oxide (shadow stacks): free this context's supervisor shadow-stack token before a
    // SYSRETQ return to user space (NOP unless enabled; skipped for the single-step IRETQ
    // fallback, see the macro). SSP is at the busy supervisor token here, whether reached from
    // __inner_syscall_instruction or a new context's first `enter_usermode`; RAX is still dead
    // (reloaded by pop_scratch below).
    shstk_syscall_exit!(),

    // Pop context registers
    crate::pop_preserved!(),
    crate::pop_scratch!(),

    // Ferric Oxide (KPTI): move the return frame to the entry stack and load the shadow table.
    crate::arch::interrupt::handler::kpti_exit!("0"),

    // Restore user GSBASE by swapping GSBASE and KGSBASE.
    "swapgs;",

    // TODO: Should we unconditionally jump or avoid jumping, to hint to the branch predictor that
    // singlestep is NOT set?
    //
    // It appears Intel CPUs assume (previously unknown) forward conditional branches to not be
    // taken, and AMD appears to assume all previously unknown conditional branches will not be
    // taken.

    // Check if the Trap Flag (singlestep flag) is set. If so, sysretq will return to before the
    // instruction, whereas debuggers expect the iretq behavior of returning to after the
    // instruction.

    "test BYTE PTR [rsp + 17], 1;",
    // If set, return using IRETQ instead.
    "jnz 2f;",

    // Otherwise, continue with the fast sysretq.

    // Pop userspace return pointer
    "pop rcx;",

    // We must ensure RCX is canonical; if it is not when running sysretq, the consequences can be
    // fatal from a security perspective.
    //
    // See https://xenproject.org/2012/06/13/the-intel-sysret-privilege-escalation/.
    //
    // This is not just theoretical; ptrace allows userspace to change RCX (via RIP) of target
    // processes.
    //
    // While we could also conditionally IRETQ here, an easier method is to simply sign-extend RCX:

    // Shift away the upper 16 bits (0xBAAD_8000_DEAD_BEEF => 0x8000_DEAD_BEEF_XXXX).
    "shl rcx, 16;",
    // Shift arithmetically right by 16 bits, effectively extending the 47th sign bit to bits
    // 63:48 (0x8000_DEAD_BEEF_XXXX => 0xFFFF_8000_DEAD_BEEF).
    "sar rcx, 16;",

    "add rsp, 8;",              // Pop fake userspace CS
    "pop r11;",                 // Pop rflags
    "pop rsp;",                 // Restore userspace stack pointer
    "sysretq;",                 // Return into userspace; RCX=>RIP,R11=>RFLAGS

    // IRETQ fallback (single-step: the Trap Flag is set, so IRETQ is used to return *after* the
    // instruction, as debuggers expect, rather than SYSRETQ's return to before it).
    //
    // Ferric Oxide (shadow stacks): with CET shadow stacks on, `shstk_syscall_exit!` skipped
    // CLRSSBSY for this path, so SSP is still at this context's busy supervisor token, as after
    // an interrupt from ring 3. IRETQ to ring 3 checks no shadow frame (none is pushed for
    // interrupts from ring 3, and SYSCALL pushes none either) and frees that token, so no frame
    // needs building here. Not yet run on CET hardware.
    "
    .p2align 4
2:
    xor rcx, rcx
    xor r11, r11
    iretq
    .globl __syscall_instruction_end
__syscall_instruction_end:
    ",

    sp = const(offset_of!(gdt::ProcessorControlRegion, user_rsp_tmp)),
    ksp = const(offset_of!(gdt::ProcessorControlRegion, kernel_rsp)),
    ss_sel = const(SegmentSelector::new(gdt::GDT_USER_DATA as u16, x86::Ring::Ring3).bits()),
    cs_sel = const(SegmentSelector::new(gdt::GDT_USER_CODE as u16, x86::Ring::Ring3).bits()),
    KPTI_SCRATCH = const(super::handler::KPTI_SCRATCH),
    KPTI_ENTRY_TOP = const(super::handler::KPTI_ENTRY_TOP),
    KPTI_CR3_NOFLUSH = const(super::handler::KPTI_CR3_NOFLUSH),
    KPTI_CR3_USER = const(super::handler::KPTI_CR3_USER),
    );
}
unsafe extern "C" {
    // TODO: macro?
    pub fn enter_usermode();
    pub fn __syscall_instruction_end();
}
