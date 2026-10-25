use crate::{arch::flags::FLAG_SINGLESTEP, memory::ArchIntCtx, panic, syscall::IntRegisters};

#[derive(Default)]
#[repr(C)]
pub struct ScratchRegisters {
    pub r11: usize,
    pub r10: usize,
    pub r9: usize,
    pub r8: usize,
    pub rsi: usize,
    pub rdi: usize,
    pub rdx: usize,
    pub rcx: usize,
    pub rax: usize,
}

impl ScratchRegisters {
    pub fn dump(&self) {
        crate::println!("RAX:   {:016x}", { self.rax });
        crate::println!("RCX:   {:016x}", { self.rcx });
        crate::println!("RDX:   {:016x}", { self.rdx });
        crate::println!("RDI:   {:016x}", { self.rdi });
        crate::println!("RSI:   {:016x}", { self.rsi });
        crate::println!("R8:    {:016x}", { self.r8 });
        crate::println!("R9:    {:016x}", { self.r9 });
        crate::println!("R10:   {:016x}", { self.r10 });
        crate::println!("R11:   {:016x}", { self.r11 });
    }
}

#[derive(Default)]
#[repr(C)]
pub struct PreservedRegisters {
    pub r15: usize,
    pub r14: usize,
    pub r13: usize,
    pub r12: usize,
    pub rbp: usize,
    pub rbx: usize,
}

impl PreservedRegisters {
    pub fn dump(&self) {
        crate::println!("RBX:   {:016x}", { self.rbx });
        crate::println!("RBP:   {:016x}", { self.rbp });
        crate::println!("R12:   {:016x}", { self.r12 });
        crate::println!("R13:   {:016x}", { self.r13 });
        crate::println!("R14:   {:016x}", { self.r14 });
        crate::println!("R15:   {:016x}", { self.r15 });
    }
}

#[derive(Default)]
#[repr(C)]
pub struct IretRegisters {
    pub rip: usize,
    pub cs: usize,
    pub rflags: usize,

    // In x86 Protected Mode, i.e. 32-bit kernels, the following two registers are conditionally
    // pushed if the privilege ring changes. In x86 Long Mode however, i.e. 64-bit kernels, they
    // are unconditionally pushed, mostly due to stack alignment requirements.
    pub rsp: usize,
    pub ss: usize,
}

impl IretRegisters {
    pub fn dump(&self) {
        crate::println!("RFLAG: {:016x}", { self.rflags });
        crate::println!("CS:    {:016x}", { self.cs });
        crate::println!("RIP:   {:016x}", { self.rip });

        crate::println!("RSP:   {:016x}", { self.rsp });
        crate::println!("SS:    {:016x}", { self.ss });

        unsafe {
            let fsbase = x86::msr::rdmsr(x86::msr::IA32_FS_BASE);
            let gsbase = x86::msr::rdmsr(x86::msr::IA32_KERNEL_GSBASE);
            let kgsbase = x86::msr::rdmsr(x86::msr::IA32_GS_BASE);
            crate::println!(
                "FSBASE  {:016x}\nGSBASE  {:016x}\nKGSBASE {:016x}",
                fsbase,
                gsbase,
                kgsbase
            );
        }
    }
}

#[derive(Default)]
#[repr(C)]
pub struct InterruptStack {
    pub preserved: PreservedRegisters,
    pub scratch: ScratchRegisters,
    pub iret: IretRegisters,
}

impl InterruptStack {
    pub fn init(&mut self) {
        // Always enable interrupts!
        self.iret.rflags = x86::bits64::rflags::RFlags::FLAGS_IF.bits() as usize;
        self.iret.cs = (crate::arch::gdt::GDT_USER_CODE << 3) | 3;
        self.iret.ss = (crate::arch::gdt::GDT_USER_DATA << 3) | 3;
    }
    pub fn frame_pointer(&self) -> usize {
        self.preserved.rbp
    }
    pub fn stack_pointer(&self) -> usize {
        self.iret.rsp
    }
    pub fn set_stack_pointer(&mut self, rsp: usize) {
        self.iret.rsp = rsp;
    }
    pub fn instr_pointer(&self) -> usize {
        self.iret.rip
    }
    pub fn sig_archdep_reg(&self) -> usize {
        self.iret.rflags
    }
    pub fn set_instr_pointer(&mut self, rip: usize) {
        self.iret.rip = rip;
    }
    pub fn set_arg1(&mut self, arg_opt: Option<usize>) {
        if let Some(arg) = arg_opt {
            self.scratch.rsi = arg;
        }
    }
    pub fn dump(&self) {
        self.iret.dump();
        self.scratch.dump();
        self.preserved.dump();
    }
    pub fn trace(&self) {
        self.dump();
        unsafe {
            panic::user_stack_trace(self);
            panic::stack_trace();
        }
    }
    /// Saves all registers to a struct used by the proc:
    /// scheme to read/write registers.
    pub fn save(&self, all: &mut IntRegisters) {
        all.r15 = self.preserved.r15;
        all.r14 = self.preserved.r14;
        all.r13 = self.preserved.r13;
        all.r12 = self.preserved.r12;
        all.rbp = self.preserved.rbp;
        all.rbx = self.preserved.rbx;
        all.r11 = self.scratch.r11;
        all.r10 = self.scratch.r10;
        all.r9 = self.scratch.r9;
        all.r8 = self.scratch.r8;
        all.rsi = self.scratch.rsi;
        all.rdi = self.scratch.rdi;
        all.rdx = self.scratch.rdx;
        all.rcx = self.scratch.rcx;
        all.rax = self.scratch.rax;
        all.rip = self.iret.rip;
        all.cs = self.iret.cs;
        all.rflags = self.iret.rflags;
        all.rsp = self.iret.rsp;
        all.ss = self.iret.ss;
    }
    /// Loads all registers from a struct used by the proc:
    /// scheme to read/write registers.
    pub fn load(&mut self, all: &IntRegisters) {
        self.preserved.r15 = all.r15;
        self.preserved.r14 = all.r14;
        self.preserved.r13 = all.r13;
        self.preserved.r12 = all.r12;
        self.preserved.rbp = all.rbp;
        self.preserved.rbx = all.rbx;
        self.scratch.r11 = all.r11;
        self.scratch.r10 = all.r10;
        self.scratch.r9 = all.r9;
        self.scratch.r8 = all.r8;
        self.scratch.rsi = all.rsi;
        self.scratch.rdi = all.rdi;
        self.scratch.rdx = all.rdx;
        self.scratch.rcx = all.rcx;
        self.scratch.rax = all.rax;
        self.iret.rip = all.rip;
        self.iret.rsp = all.rsp;

        // CS and SS are immutable, at least their privilege levels.

        // OF, DF, 0, TF => D
        // SF, ZF, 0, AF => D
        // 0, PF, 1, CF => 5
        const ALLOWED_RFLAGS: usize = 0xDD5;

        self.iret.rflags &= !ALLOWED_RFLAGS;
        self.iret.rflags |= all.rflags & ALLOWED_RFLAGS;
    }
    /// Enables the "Trap Flag" in the FLAGS register, causing the CPU
    /// to send a Debug exception after the next instruction. This is
    /// used for singlestep in the proc: scheme.
    pub fn set_singlestep(&mut self, enabled: bool) {
        if enabled {
            self.iret.rflags |= FLAG_SINGLESTEP;
        } else {
            self.iret.rflags &= !FLAG_SINGLESTEP;
        }
    }
}

#[macro_export]
macro_rules! push_scratch {
    () => {
        "
        // Push scratch registers
        push rcx
        push rdx
        push rdi
        push rsi
        push r8
        push r9
        push r10
        push r11
    "
    };
}
#[macro_export]
macro_rules! pop_scratch {
    () => {
        "
        // Pop scratch registers
        pop r11
        pop r10
        pop r9
        pop r8
        pop rsi
        pop rdi
        pop rdx
        pop rcx
        pop rax
    "
    };
}

#[macro_export]
macro_rules! push_preserved {
    () => {
        "
        // Push preserved registers
        push rbx
        push rbp
        push r12
        push r13
        push r14
        push r15
    "
    };
}
#[macro_export]
macro_rules! pop_preserved {
    () => {
        "
        // Pop preserved registers
        pop r15
        pop r14
        pop r13
        pop r12
        pop rbp
        pop rbx
    "
    };
}
// Ferric Oxide: kernel page-table isolation (KPTI, see arch/x86_64/kpti.rs). All of these are
// patched in by the "kpti" alternative and skipped otherwise. They use the PCR offsets below,
// which every naked function that expands them passes as `const` operands.

/// PCR offset of the KPTI scratch slot.
pub const KPTI_SCRATCH: usize =
    core::mem::offset_of!(crate::arch::gdt::ProcessorControlRegion, kpti_scratch);
/// PCR offset of the top of the per-CPU entry stack.
pub const KPTI_ENTRY_TOP: usize =
    core::mem::offset_of!(crate::arch::gdt::ProcessorControlRegion, kpti_entry_top);
/// PCR offset of the top of the current context's kernel stack.
pub const KPTI_KERNEL_RSP: usize =
    core::mem::offset_of!(crate::arch::gdt::ProcessorControlRegion, kernel_rsp);
/// PCR offset of the bits ORed into CR3 to load a table without flushing (bit 63 with PCID).
pub const KPTI_CR3_NOFLUSH: usize =
    core::mem::offset_of!(crate::arch::gdt::ProcessorControlRegion, kpti_cr3_noflush);
/// PCR offset of the bits ORed into a kernel-table CR3 to load its shadow.
pub const KPTI_CR3_USER: usize =
    core::mem::offset_of!(crate::arch::gdt::ProcessorControlRegion, kpti_cr3_user);

// Wraps KPTI code in the "kpti" alternative, with a jump over the padding when it is off.
macro_rules! kpti_alternative {
    ($code:expr_2021) => {
        concat!(
            $crate::arch::macros::alternative!(feature: "kpti", then: [$code], default: ["jmp 69f"]),
            "\n69:\n"
        )
    };
}

// First thing on interrupt entry, before any other stack use. $err is 8 when the CPU (or the
// stub) pushed an error code, else 0. From user space: switch to the kernel table and, when the
// CPU put the frame on the entry stack, move it to the kernel stack. GS is left as it was, so the
// usual SWAPGS logic that follows still applies. From the kernel ($fix = 1): if the shadow is
// loaded (an exception between switching tables and leaving the kernel), load the kernel table.
//
// Loading the kernel table clears the shadow bit (12) and the user PCID bit (11, never set
// without PCID) and, from user space, ORs in the no-flush bit from the PCR. The kernel-mode fix
// may run with the user's GS (a fault on IRETQ), so it cannot read the PCR and lets the write
// flush the kernel PCID instead, which is always correct.
macro_rules! kpti_enter {
    ($err:literal, $fix:literal) => {
        $crate::arch::interrupt::handler::kpti_alternative!(concat!(
            "
        test QWORD PTR [rsp + 8 + ",
            $err,
            "], 3
        jz 74f
        swapgs
        mov gs:[{KPTI_SCRATCH}], rax
        mov rax, cr3
        and rax, -0x1801
        or rax, gs:[{KPTI_CR3_NOFLUSH}]
        mov cr3, rax
        lea rax, [rsp + 40 + ",
            $err,
            "]
        cmp rax, gs:[{KPTI_ENTRY_TOP}]
        jne 73f
        mov rax, rsp
        mov rsp, gs:[{KPTI_KERNEL_RSP}]
        push QWORD PTR [rax + 32 + ",
            $err,
            "]
        push QWORD PTR [rax + 24 + ",
            $err,
            "]
        push QWORD PTR [rax + 16 + ",
            $err,
            "]
        push QWORD PTR [rax + 8 + ",
            $err,
            "]
        push QWORD PTR [rax + ",
            $err,
            "]
        .if ",
            $err,
            "
        push QWORD PTR [rax]
        .endif
    73:
        mov rax, gs:[{KPTI_SCRATCH}]
        swapgs
        jmp 75f
    74:
        .if ",
            $fix,
            "
        push rax
        mov rax, cr3
        btr rax, 12
        jnc 76f
        and rax, -0x801
        mov cr3, rax
    76:
        pop rax
        .endif
    75:
    "
        ))
    };
}

// Last thing before IRETQ (or SYSRETQ), with the five-word interrupt frame at RSP. Returning to
// user space: copy the frame to the entry stack and load the shadow table, by ORing the PCR's
// `kpti_cr3_user` into CR3 (the shadow bit, plus the user PCID and no-flush bit with PCID). $gs_user
// is 1 when SWAPGS has already restored the user GS, 0 when GS is still the kernel's.
macro_rules! kpti_exit {
    ($gs_user:literal) => {
        $crate::arch::interrupt::handler::kpti_alternative!(concat!(
            "
        test QWORD PTR [rsp + 8], 3
        jz 77f
        .if ",
            $gs_user,
            "
        swapgs
        .endif
        mov gs:[{KPTI_SCRATCH}], rax
        mov rax, rsp
        mov rsp, gs:[{KPTI_ENTRY_TOP}]
        push QWORD PTR [rax + 32]
        push QWORD PTR [rax + 24]
        push QWORD PTR [rax + 16]
        push QWORD PTR [rax + 8]
        push QWORD PTR [rax]
        mov rax, cr3
        or rax, gs:[{KPTI_CR3_USER}]
        mov cr3, rax
        mov rax, gs:[{KPTI_SCRATCH}]
        .if ",
            $gs_user,
            "
        swapgs
        .endif
    77:
    "
        ))
    };
}

// Paranoid handlers (NMI, #MC, #DB) can interrupt the entry or exit code with either table
// loaded. Save CR3 in R12 (callee-saved, and already pushed) and load the kernel table; restore
// the saved one on the way out. Both run with the kernel GS, so both switch without flushing when
// PCID is on: TLB invalidations never wait for a flush on a later switch (see kpti.rs).
macro_rules! kpti_paranoid_save {
    () => {
        $crate::arch::interrupt::handler::kpti_alternative!(
            "
        mov r12, cr3
        bt r12, 12
        jnc 78f
        mov rax, r12
        and rax, -0x1801
        or rax, gs:[{KPTI_CR3_NOFLUSH}]
        mov cr3, rax
    78:
    "
        )
    };
}
macro_rules! kpti_paranoid_restore {
    () => {
        $crate::arch::interrupt::handler::kpti_alternative!(
            "
        bt r12, 12
        jnc 79f
        or r12, gs:[{KPTI_CR3_NOFLUSH}]
        mov cr3, r12
    79:
    "
        )
    };
}

macro_rules! swapgs_iff_ring3_fast {
    // TODO: Spectre V1: LFENCE?
    () => {
        "
        // Check whether the last two bits RSP+8 (code segment) are equal to zero.
        test QWORD PTR [rsp + 8], 0x3
        // Skip the SWAPGS instruction if CS & 0b11 == 0b00.
        jz 2f
        swapgs
        2:
    "
    };
}
macro_rules! swapgs_iff_ring3_fast_errorcode {
    // TODO: Spectre V1: LFENCE?
    () => {
        "
        test QWORD PTR [rsp + 16], 0x3
        jz 2f
        swapgs
        2:
    "
    };
}

macro_rules! conditional_swapgs_paranoid {
    // For regular interrupt handlers and the syscall handler, managing IA32_GS_BASE and
    // IA32_KERNEL_GS_BASE (the "GSBASE registers") is more or less trivial when using the SWAPGS
    // instruction.
    //
    // The syscall handler simply runs SWAPGS, as syscalls can only originate from usermode,
    // whereas interrupt handlers conditionally SWAPGS unless the interrupt was triggered from
    // kernel mode, in which case the "swap state" is already valid, and there is no need to
    // SWAPGS.
    //
    // Handling GSBASE correctly for paranoid interrupts however, is not as simple. NMIs can occur
    // between the check of whether an interrupt came from usermode, and the actual SWAPGS
    // instruction. #DB can also be triggered inside of a kernel interrupt handler, due to
    // breakpoints, even though setting up such breakpoints in the first place, is not yet
    // supported by the kernel.
    //
    // Luckily, the GDT always resides in the PCR (at least after init_paging, but there are no
    // interrupt handlers set up before that), allowing GSBASE to be calculated relatively cheaply.
    // Out of the two GSBASE registers, at least one must be *the* kernel GSBASE, allowing for a
    // simple conditional SWAPGS.
    //
    // (An alternative to conditionally executing SWAPGS, would be to save and restore GSBASE via
    // e.g. the stack. That would nonetheless require saving and restoring both GSBASE registers,
    // if the interrupt handler should be allowed to context switch, which the current #DB handler
    // may do.)
    //
    // TODO: Handle nested NMIs like Linux does (https://lwn.net/Articles/484932/)?.

    () => { concat!(
        // Put the GDT base pointer in RDI.
        "
        sub rsp, 16
        sgdt [rsp + 6]
        mov rdi, [rsp + 8]
        add rsp, 16
        ",
        // Calculate the PCR address by subtracting the offset of the GDT in the PCR struct.
        "sub rdi, {PCR_GDT_OFFSET};",

        // Read the current IA32_GS_BASE value into RDX.
        $crate::arch::macros::alternative!(
            feature: "fsgsbase",
            then: ["rdgsbase rdx"],
            default: ["
                mov ecx, {IA32_GS_BASE}
                rdmsr
                shl rdx, 32
                or rdx, rax
            "]
        ),

        // If they were not equal, the PCR address must instead be in IA32_KERNEL_GS_BASE,
        // requiring a SWAPGS. GSBASE needs to be swapped back, so store the same flag in RBX.

        // TODO: Spectre V1: LFENCE?
        "
        cmp rdx, rdi
        sete bl
        je 2f
        swapgs
        2:
        ",
        $crate::arch::interrupt::handler::kpti_paranoid_save!(),
    ) }
}
macro_rules! conditional_swapgs_back_paranoid {
    () => {
        concat!(
            $crate::arch::interrupt::handler::kpti_paranoid_restore!(),
            "
        test bl, bl
        jnz 2f
        swapgs
        2:
    "
        )
    };
}
macro_rules! nop {
    () => {
        "
        // Unused: {IA32_GS_BASE} {PCR_GDT_OFFSET}
        "
    };
}

#[macro_export]
macro_rules! interrupt_stack {
    // XXX: Apparently we cannot use $expr and check for bool exhaustiveness, so we will have to
    // use idents directly instead.
    ($name:ident, $save1:ident!, $save2:ident!, $rstor2:ident!, $rstor1:ident!, is_paranoid: $is_paranoid:expr_2021, kpti_fix: $kpti_fix:literal, |$stack:ident| $code:block) => {
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name() {
            unsafe extern "C" fn inner($stack: &mut $crate::arch::x86_64::interrupt::InterruptStack) {
                $code
            }
            core::arch::naked_asm!(
                // Ferric Oxide (IBT): the IDT is an indirect branch target.
                $crate::arch::macros::ibt_endbr!(),
                // Clear direction flag, required by ABI when running any Rust code in the kernel.
                "cld;",

                // Ferric Oxide (KPTI)
                $crate::arch::interrupt::handler::kpti_enter!("0", $kpti_fix),

                // Backup all userspace registers to stack
                $crate::arch::interrupt::handler::$save1!(),
                "push rax",
                $crate::push_scratch!(),
                $crate::push_preserved!(),

                $crate::arch::interrupt::handler::$save2!(),

                // Call inner function with pointer to stack
                "
                mov rdi, rsp
                call {inner}
                ",

                $crate::arch::interrupt::handler::$rstor2!(),

                // Restore all userspace registers
                $crate::pop_preserved!(),
                $crate::pop_scratch!(),

                $crate::arch::interrupt::handler::$rstor1!(),
                $crate::arch::interrupt::handler::kpti_exit!("1"),
                "iretq",

                inner = sym inner,
                IA32_GS_BASE = const(x86::msr::IA32_GS_BASE),
                KPTI_SCRATCH = const($crate::arch::interrupt::handler::KPTI_SCRATCH),
                KPTI_ENTRY_TOP = const($crate::arch::interrupt::handler::KPTI_ENTRY_TOP),
                KPTI_KERNEL_RSP = const($crate::arch::interrupt::handler::KPTI_KERNEL_RSP),
                KPTI_CR3_NOFLUSH = const($crate::arch::interrupt::handler::KPTI_CR3_NOFLUSH),
                KPTI_CR3_USER = const($crate::arch::interrupt::handler::KPTI_CR3_USER),

                PCR_GDT_OFFSET = const(core::mem::offset_of!($crate::arch::gdt::ProcessorControlRegion, gdt)),
            );
        }
    };
    ($name:ident, |$stack:ident| $code:block) => { $crate::interrupt_stack!($name, swapgs_iff_ring3_fast!, nop!, nop!, swapgs_iff_ring3_fast!, is_paranoid: false, kpti_fix: "1", |$stack| $code); };
    ($name:ident, @paranoid, |$stack:ident| $code:block) => { $crate::interrupt_stack!($name, nop!, conditional_swapgs_paranoid!, conditional_swapgs_back_paranoid!, nop!, is_paranoid: true, kpti_fix: "0", |$stack| $code); }
}

#[macro_export]
macro_rules! interrupt {
    ($name:ident, || $code:block) => {
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name() {
            unsafe extern "C" fn inner() {
                $code
            }

            core::arch::naked_asm!(
                // Ferric Oxide (IBT): the IDT is an indirect branch target.
                $crate::arch::macros::ibt_endbr!(),
                // Clear direction flag, required by ABI when running any Rust code in the kernel.
                "cld;",

                // Ferric Oxide (KPTI)
                $crate::arch::interrupt::handler::kpti_enter!("0", "1"),

                // Backup all userspace registers to stack
                $crate::arch::interrupt::handler::swapgs_iff_ring3_fast!(),
                "push rax",
                $crate::push_scratch!(),

                // Call inner function with pointer to stack
                "call {inner}",

                // Restore all userspace registers
                $crate::pop_scratch!(),

                $crate::arch::interrupt::handler::swapgs_iff_ring3_fast!(),
                $crate::arch::interrupt::handler::kpti_exit!("1"),
                "iretq",

                inner = sym inner,
                KPTI_SCRATCH = const($crate::arch::interrupt::handler::KPTI_SCRATCH),
                KPTI_ENTRY_TOP = const($crate::arch::interrupt::handler::KPTI_ENTRY_TOP),
                KPTI_KERNEL_RSP = const($crate::arch::interrupt::handler::KPTI_KERNEL_RSP),
                KPTI_CR3_NOFLUSH = const($crate::arch::interrupt::handler::KPTI_CR3_NOFLUSH),
                KPTI_CR3_USER = const($crate::arch::interrupt::handler::KPTI_CR3_USER),
            );
        }
    };
}

#[macro_export]
macro_rules! interrupt_error {
    ($name:ident, |$stack:ident, $error_code:ident| $code:block) => {
        #[unsafe(naked)]
        pub unsafe extern "C" fn $name() {
            unsafe extern "C" fn inner($stack: &mut $crate::arch::x86_64::interrupt::handler::InterruptStack, $error_code: usize) {
                $code
            }

            core::arch::naked_asm!(
                // Ferric Oxide (IBT): the IDT is an indirect branch target.
                $crate::arch::macros::ibt_endbr!(),
                // Clear direction flag, required by ABI when running any Rust code in the kernel.
                "cld;",

                // Ferric Oxide (KPTI)
                $crate::arch::interrupt::handler::kpti_enter!("8", "1"),

                $crate::arch::interrupt::handler::swapgs_iff_ring3_fast_errorcode!(),

                // Don't push RAX yet, as the error code is already stored in RAX's position.

                // Push all userspace registers
                $crate::push_scratch!(),
                $crate::push_preserved!(),

                // Now that we have a couple of usable registers, put the error code in the second
                // argument register for the inner function, and save RAX where it would normally
                // be.
                "mov rsi, [rsp + {rax_offset}];",
                "mov [rsp + {rax_offset}], rax;",

                // Call inner function with pointer to stack, and error code.
                "mov rdi, rsp;",
                "call {inner};",

                // Restore all userspace registers
                $crate::pop_preserved!(),
                $crate::pop_scratch!(),

                // The error code has already been popped, so use the regular macro.
                $crate::arch::interrupt::handler::swapgs_iff_ring3_fast!(),
                $crate::arch::interrupt::handler::kpti_exit!("1"),
                "iretq;",

                inner = sym inner,
                KPTI_SCRATCH = const($crate::arch::interrupt::handler::KPTI_SCRATCH),
                KPTI_ENTRY_TOP = const($crate::arch::interrupt::handler::KPTI_ENTRY_TOP),
                KPTI_KERNEL_RSP = const($crate::arch::interrupt::handler::KPTI_KERNEL_RSP),
                KPTI_CR3_NOFLUSH = const($crate::arch::interrupt::handler::KPTI_CR3_NOFLUSH),
                KPTI_CR3_USER = const($crate::arch::interrupt::handler::KPTI_CR3_USER),
                rax_offset = const(size_of::<$crate::arch::interrupt::handler::PreservedRegisters>() + size_of::<$crate::arch::interrupt::handler::ScratchRegisters>() - 8),
            );
        }
    };
}

impl ArchIntCtx for InterruptStack {
    fn ip(&self) -> usize {
        self.iret.rip
    }
    fn recover_and_efault(&mut self) {
        // We were inside a usercopy function (a `rep movsb`, see `arch_copy_to_user`) that
        // faulted. The caller expects a non-zero return (EFAULT).
        //
        // Ferric Oxide (shadow stacks): when CET shadow stacks are on, do NOT rewrite the saved
        // RIP to emulate the usercopy function's `ret`. Doing so would leave the RIP on the
        // kernel stack disagreeing with the copy on the shadow stack that the CPU pushed when it
        // delivered the page fault, so IRETQ would raise #CP. Instead, make the faulting
        // instruction a no-op on restart: `rep movsb` with RCX = 0 copies nothing, so IRETQ
        // returns to it, it falls through to the usercopy function's own `clac`/`ret`, and that
        // `ret` pops the normal and shadow stacks together, keeping them consistent. RAX = 1 is
        // the EFAULT indicator the function would otherwise have left zero. This works with CET
        // off too, but the default build must match upstream exactly, so the original
        // ret-emulation stays on that path.
        #[cfg(shstk_compiled)]
        if crate::arch::shstk::live() {
            self.scratch.rax = 1;
            self.scratch.rcx = 0;
            return;
        }
        // We were inside a usercopy function that failed. This is handled by setting rax to a
        // nonzero value, and emulating the ret instruction.
        self.scratch.rax = 1;
        let ret_addr = unsafe { (self.iret.rsp as *const usize).read() };
        self.iret.rsp += 8;
        self.iret.rip = ret_addr;
        self.iret.rflags &= !(1 << 18);
    }
}

// Assembly helpers used by the exported interrupt macros.
#[allow(unused_imports)]
pub(crate) use {
    conditional_swapgs_back_paranoid, conditional_swapgs_paranoid, kpti_alternative, kpti_enter,
    kpti_exit, kpti_paranoid_restore, kpti_paranoid_save, nop, swapgs_iff_ring3_fast,
    swapgs_iff_ring3_fast_errorcode,
};
