use syscall::Exception;
use x86::irq::PageFaultError;

use crate::{
    arch::x86_shared::interrupt,
    context::signal::excp_handler,
    memory::{GenericPfFlags, VirtualAddress},
    ptrace,
    sync::CleanLockToken,
    syscall::flag::*,
};

crate::interrupt_stack!(divide_by_zero, |stack| {
    crate::println!("Divide by zero");
    stack.trace();
    excp_handler(Exception {
        kind: 0,
        ..Default::default()
    });
});

crate::interrupt_stack!(debug, @paranoid, |stack| {
    let mut handled = false;

    // Disable singlestep before there is a breakpoint, since the breakpoint
    // handler might end up setting it again but unless it does we want the
    // default to be false.
    #[cfg(target_arch = "x86")]
    let had_singlestep = stack.iret.eflags & (1 << 8) == 1 << 8;
    #[cfg(target_arch = "x86_64")]
    let had_singlestep = stack.iret.rflags & (1 << 8) == 1 << 8;
    stack.set_singlestep(false);

    let mut token = unsafe { CleanLockToken::new() };
    if ptrace::breakpoint_callback(PTRACE_STOP_SINGLESTEP, None, &mut token).is_some() {
        handled = true;
    } else {
        // There was no breakpoint, restore original value
        stack.set_singlestep(had_singlestep);
    }

    if !handled {
        crate::println!("Debug trap");
        stack.dump();
        excp_handler(Exception {
            kind: 1,
            ..Default::default()
        });
    }
});

crate::interrupt_stack!(non_maskable, @paranoid, |stack| {
    #[cfg(target_arch = "x86_64")]
    unsafe { crate::profiling::nmi_handler(stack) };

    #[cfg(not(all(target_arch = "x86_64", feature = "profiling")))]
    {
        // TODO: This will likely deadlock
        crate::println!("Non-maskable interrupt");
        // Ferric Oxide: the interrupted instruction at its link address (as kernel.sym has it)
        // and the frames above it, so an NMI locates a CPU stuck in the kernel.
        crate::println!(
            "NMI: interrupted at {:#x}",
            stack.instr_pointer().wrapping_sub(crate::kernel_image_slide())
        );
        stack.trace();
    }
});

crate::interrupt_stack!(breakpoint, |stack| {
    // Ferric Oxide: an int3 a tracepoint site holds while it is patched live resumes after the
    // site (arch/x86_64/static_key.rs).
    #[cfg(all(feature = "trace", target_arch = "x86_64"))]
    if let Some(resume) = crate::arch::static_key::breakpoint(stack.iret.rip, stack.iret.cs) {
        stack.iret.rip = resume;
        return;
    }

    // The processor lets EIP/RIP point to the instruction *after* int3, so
    // unhandled breakpoint interrupt don't go in an infinite loop. But we
    // throw SIGTRAP anyway, so that's not a problem.
    //
    // We have the following code to prevent
    // - RIP from going out of sync with instructions
    // - The user having to do 2 syscalls to replace the instruction at RIP
    // - Having more compatibility glue for GDB than necessary
    //
    // Let's just follow Linux convention and let RIP be RIP-1, point to the
    // int3 instruction. After all, it's the sanest thing to do.
    #[cfg(target_arch = "x86")]
    {
        stack.iret.eip -= 1;
    }
    #[cfg(target_arch = "x86_64")]
    {
        let after_int3 = stack.iret.rip;
        stack.iret.rip -= 1;
        // Ferric Oxide (shadow stacks): a ring-3 int3 pushes no shadow-stack frame, so only a
        // ring-0 int3 (which IDT entry 3 also accepts) needs the frame's return address moved
        // with the saved RIP, or IRETQ raises #CP. No-op while CET shadow stacks are off.
        if stack.iret.cs & 3 == 0
            && let Err(err) = crate::arch::shstk::retarget_kernel_return(
                stack.iret.cs,
                after_int3,
                stack.iret.rip,
            )
        {
            panic!("CET: cannot rewind a kernel int3: {err}");
        }
    }

    let mut token = unsafe { CleanLockToken::new() };
    if ptrace::breakpoint_callback(PTRACE_STOP_BREAKPOINT, None, &mut token).is_none() {
        crate::println!("Breakpoint trap");
        stack.dump();
        excp_handler(Exception {
            kind: 3,
            ..Default::default()
        });
    }
});

crate::interrupt_stack!(overflow, |stack| {
    crate::println!("Overflow trap");
    stack.trace();
    excp_handler(Exception {
        kind: 4,
        ..Default::default()
    });
});

crate::interrupt_stack!(bound_range, |stack| {
    crate::println!("Bound range exceeded fault");
    stack.trace();
    excp_handler(Exception {
        kind: 5,
        ..Default::default()
    });
});

crate::interrupt_stack!(invalid_opcode, |stack| {
    crate::println!("Invalid opcode fault");
    stack.trace();
    excp_handler(Exception {
        kind: 6,
        ..Default::default()
    });
});

crate::interrupt_stack!(device_not_available, |stack| {
    crate::println!("Device not available fault");
    stack.trace();
    excp_handler(Exception {
        kind: 7,
        ..Default::default()
    });
});

crate::interrupt_error!(double_fault, |stack, _code| {
    crate::println!("Double fault");
    stack.trace();
    unsafe {
        loop {
            interrupt::disable();
            interrupt::halt();
        }
    }
});

crate::interrupt_error!(invalid_tss, |stack, code| {
    crate::println!("Invalid TSS fault");
    stack.trace();
    excp_handler(Exception {
        kind: 10,
        code,
        ..Default::default()
    });
});

crate::interrupt_error!(segment_not_present, |stack, code| {
    crate::println!("Segment not present fault");
    stack.trace();
    excp_handler(Exception {
        kind: 11,
        code,
        ..Default::default()
    });
});

crate::interrupt_error!(stack_segment, |stack, code| {
    crate::println!("Stack segment fault");
    stack.trace();
    excp_handler(Exception {
        kind: 12,
        code,
        ..Default::default()
    });
});

crate::interrupt_error!(protection, |stack, code| {
    let was_failing_msr;

    // TODO: maybe consider just forcing userspace to handle this, but our exception handling is
    // currently limited
    #[cfg(target_arch = "x86_64")]
    {
        was_failing_msr = stack.iret.rip
            == crate::arch::misc::__the_wrapped_rdmsr_instr as *const () as usize
            || stack.iret.rip == crate::arch::misc::__the_wrapped_wrmsr_instr as *const () as usize;

        if was_failing_msr {
            // 0F 30 (WRMSR) or 0F 32 (RDMSR)
            let resume = stack.iret.rip + 2;
            // Ferric Oxide (shadow stacks): the ring-0 #GP pushed a shadow-stack frame whose
            // return address is the faulting instruction; IRETQ raises #CP unless it is moved
            // past it along with the saved RIP. No-op while CET shadow stacks are off.
            if let Err(err) =
                crate::arch::shstk::retarget_kernel_return(stack.iret.cs, stack.iret.rip, resume)
            {
                panic!("CET: cannot resume after a failed MSR access: {err}");
            }
            stack.iret.rip = resume;
            stack.scratch.rsi = usize::MAX;
        }
    };

    #[cfg(target_arch = "x86")]
    {
        was_failing_msr = false;
    }

    if !was_failing_msr {
        crate::println!("Protection fault code={:#0x}", code);
        stack.trace();
        excp_handler(Exception {
            kind: 13,
            code,
            ..Default::default()
        });
    }
});

crate::interrupt_error!(page, |stack, code| {
    let cr2 = VirtualAddress::new(unsafe { x86::controlregs::cr2() });
    #[cfg(all(target_arch = "x86_64", shstk_compiled))]
    if crate::arch::shstk::recover_pool_probe(stack, code, cr2.data()) {
        return;
    }
    let arch_flags = PageFaultError::from_bits_truncate(code as u32);
    let mut generic_flags = GenericPfFlags::empty();

    generic_flags.set(
        GenericPfFlags::PRESENT,
        arch_flags.contains(PageFaultError::P),
    );
    generic_flags.set(
        GenericPfFlags::INVOLVED_WRITE,
        arch_flags.contains(PageFaultError::WR),
    );
    generic_flags.set(
        GenericPfFlags::USER_NOT_SUPERVISOR,
        arch_flags.contains(PageFaultError::US),
    );
    generic_flags.set(
        GenericPfFlags::INVL,
        arch_flags.contains(PageFaultError::RSVD),
    );
    generic_flags.set(
        GenericPfFlags::INSTR_NOT_DATA,
        arch_flags.contains(PageFaultError::ID),
    );

    #[cfg(target_arch = "x86")]
    if crate::memory::page_fault_handler(&mut stack.inner, generic_flags, cr2).is_err() {
        crate::println!("Page fault: {:>08X} {:#?}", cr2.data(), arch_flags);
        stack.trace();
        excp_handler(Exception {
            kind: 14,
            code,
            address: cr2.data(),
        });
    }

    #[cfg(target_arch = "x86_64")]
    if crate::memory::page_fault_handler(stack, generic_flags, cr2).is_err() {
        crate::println!("Page fault: {:>016X} {:#?}", cr2.data(), arch_flags);
        stack.trace();
        excp_handler(Exception {
            kind: 14,
            code,
            address: cr2.data(),
        });
    }
});

crate::interrupt_stack!(fpu_fault, |stack| {
    crate::println!("FPU floating point fault");
    stack.trace();
    excp_handler(Exception {
        kind: 16,
        ..Default::default()
    });
});

crate::interrupt_error!(alignment_check, |stack, code| {
    crate::println!("Alignment check fault");
    stack.trace();
    excp_handler(Exception {
        kind: 17,
        code,
        ..Default::default()
    });
});

crate::interrupt_stack!(machine_check, @paranoid, |stack| {
    crate::println!("Machine check fault");
    stack.trace();
    unsafe {
        loop {
            interrupt::disable();
            interrupt::halt();
        }
    }
});

crate::interrupt_stack!(simd, |stack| {
    crate::println!("SIMD floating point fault");
    let mut mxcsr = 0_usize;
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) core::ptr::addr_of_mut!(mxcsr)) };
    crate::println!("MXCSR {:#0x}", mxcsr);
    stack.trace();
    excp_handler(Exception {
        kind: 19,
        ..Default::default()
    });
});

crate::interrupt_stack!(virtualization, |stack| {
    crate::println!("Virtualization fault");
    stack.trace();
    unsafe {
        loop {
            interrupt::disable();
            interrupt::halt();
        }
    }
});

crate::interrupt_error!(security, |stack, _code| {
    crate::println!("Security exception");
    stack.trace();
    unsafe {
        loop {
            interrupt::disable();
            interrupt::halt();
        }
    }
});

// Ferric Oxide (CET, see arch/x86_64/ibt.rs and shstk.rs): with kernel indirect branch tracking
// on, an indirect branch that misses `endbr64` lands here; with kernel shadow stacks on, so does a
// `ret`/`iretq` whose return address differs from the shadow-stack copy, or a bad SSP token.
// User-mode CET is never enabled, so a #CP from user space is unexpected; it is handled like other
// faults there.
#[cfg(target_arch = "x86_64")]
crate::interrupt_error!(control_protection, |stack, code| {
    let fault = ferric_cet::ControlProtectionError::from_error_code(code as u64);
    if stack.iret.cs & 3 == 3 {
        crate::println!("Control protection fault in user space: {}", fault);
        stack.trace();
        excp_handler(Exception {
            kind: 21,
            code,
            ..Default::default()
        });
        return;
    }
    crate::println!("Control protection fault code={:#x}", code);
    stack.trace();
    // For an IBT fault RIP is the branch target that lacks `endbr64`; for a shadow-stack fault it
    // is the `ret`/`iretq` or token instruction. The trace above shows how execution got there.
    panic!(
        "CET: control protection fault (#CP) in the kernel at {:#x}: {}",
        stack.iret.rip, fault
    );
});
