//! Ferric Oxide: how the page-fault handler ends a fault, given what caused it and whether
//! the page tables could be corrected.
//!
//! A fault on a user address raised by user mode, or by a kernel copy to or from user memory
//! (usercopy), first tries to map the page (demand paging, copy on write). When that fails,
//! the fault is either refused (the access is not allowed or the address is not mapped) or out
//! of memory (no physical frame, or a page reference count would overflow). Both end the same
//! way, so running out of memory fails the faulting operation rather than the kernel:
//!
//! - a usercopy fault makes the copy return `EFAULT` to the system call;
//! - a user-mode fault is unhandled, and the architecture's fault path delivers it to the
//!   faulting thread as an exception (a signal through `redox-rt`, `SIGSEGV` on x86_64), or ends the thread if it
//!   has no exception handler.
//!
//! The kernel exempts no process: a service that runs out of memory fails as it would on any
//! other fatal fault. [`Cause`] keeps out-of-memory apart so it can be logged and traced.
//!
//! The module depends only on `core` so `crates/kernel/tests` checks it on the host.

/// What raised a page fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    /// The faulting address is in the user half of the address space.
    pub(crate) address_is_user: bool,
    /// The CPU was in user mode.
    pub(crate) from_user: bool,
    /// The faulting instruction is in the kernel's usercopy functions.
    pub(crate) usercopy: bool,
    /// The access was an instruction fetch.
    pub(crate) instr_fetch: bool,
}

/// The result of trying to correct the page tables for a fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Correction {
    /// The page was mapped; the access can be retried.
    Mapped,
    /// The access is not allowed or the address is not mapped.
    Refused,
    /// No physical frame was available, or a page reference count would overflow.
    OutOfMemory,
}

/// Why a fault could not be resolved by mapping the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// The access is not allowed, the address is not mapped, or no correction applies.
    NotMapped,
    /// The page could not be mapped for lack of memory.
    OutOfMemory,
}

/// How a page fault ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The page tables were corrected; the access is retried.
    Mapped,
    /// A kernel copy to or from user memory faulted; the copy returns `EFAULT`.
    CopyFailed(Cause),
    /// The fault is not handled here: an exception for a user thread, a panic in the kernel.
    Unhandled(Cause),
}

/// Decides how `fault` ends. `correct` tries to correct the page tables and is only called
/// for faults on user addresses raised by user mode or by a usercopy.
pub(crate) fn resolve(fault: Fault, correct: impl FnOnce() -> Correction) -> Outcome {
    let cause = if fault.address_is_user && (fault.from_user || fault.usercopy) {
        match correct() {
            Correction::Mapped => return Outcome::Mapped,
            Correction::Refused => Cause::NotMapped,
            Correction::OutOfMemory => Cause::OutOfMemory,
        }
    } else {
        Cause::NotMapped
    };

    if fault.address_is_user && !fault.from_user && !fault.instr_fetch && fault.usercopy {
        Outcome::CopyFailed(cause)
    } else {
        Outcome::Unhandled(cause)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Fault = Fault {
        address_is_user: true,
        from_user: true,
        usercopy: false,
        instr_fetch: false,
    };
    const USERCOPY: Fault = Fault {
        address_is_user: true,
        from_user: false,
        usercopy: true,
        instr_fetch: false,
    };

    fn not_called() -> Correction {
        panic!("the page tables must not be corrected for this fault")
    }

    #[test]
    fn mapped_faults_retry() {
        for fault in [USER, USERCOPY] {
            assert_eq!(resolve(fault, || Correction::Mapped), Outcome::Mapped);
        }
    }

    #[test]
    fn user_fault_out_of_memory_is_unhandled_not_a_panic() {
        assert_eq!(
            resolve(USER, || Correction::OutOfMemory),
            Outcome::Unhandled(Cause::OutOfMemory)
        );
    }

    #[test]
    fn usercopy_out_of_memory_fails_the_copy() {
        assert_eq!(
            resolve(USERCOPY, || Correction::OutOfMemory),
            Outcome::CopyFailed(Cause::OutOfMemory)
        );
    }

    #[test]
    fn refused_faults_keep_their_existing_outcome() {
        assert_eq!(
            resolve(USER, || Correction::Refused),
            Outcome::Unhandled(Cause::NotMapped)
        );
        assert_eq!(
            resolve(USERCOPY, || Correction::Refused),
            Outcome::CopyFailed(Cause::NotMapped)
        );
    }

    #[test]
    fn kernel_faults_outside_usercopy_are_not_corrected() {
        let kernel_on_user_address = Fault {
            usercopy: false,
            ..USERCOPY
        };
        let user_on_kernel_address = Fault {
            address_is_user: false,
            ..USER
        };
        let copy_on_kernel_address = Fault {
            address_is_user: false,
            ..USERCOPY
        };
        for fault in [kernel_on_user_address, user_on_kernel_address] {
            assert_eq!(
                resolve(fault, not_called),
                Outcome::Unhandled(Cause::NotMapped)
            );
        }
        assert_eq!(
            resolve(copy_on_kernel_address, not_called),
            Outcome::Unhandled(Cause::NotMapped)
        );
    }

    #[test]
    fn usercopy_instruction_fetch_is_unhandled() {
        let fetch = Fault {
            instr_fetch: true,
            ..USERCOPY
        };
        assert_eq!(
            resolve(fetch, || Correction::OutOfMemory),
            Outcome::Unhandled(Cause::OutOfMemory)
        );
    }
}
