//! Signals and resumable exceptions delivered while assembly holds vector canaries.
//!
//! A sender waits for the loaded-register handshake; only the handler releases
//! that loop. A finite iteration bound reports missing delivery. Exception probes
//! execute UD2 in the same assembly interval and resume by editing the public RIP.

use std::{
    fmt, io,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

use super::{
    asm, cpu_has_avx512bw, hold_registers, pattern, Zmm, K_SLOTS, SIGNAL_PHASE, TRAP, WIDE_MASKS,
    ZMM_BYTES,
};

/// An application realtime signal, away from either runtime's reserved low end.
const REALTIME_SIGNAL: libc::c_int = 44;
static HANDLER_STACK: AtomicUsize = AtomicUsize::new(0);

/// Entry paths covered by the canary probe.
#[derive(Clone, Copy, Debug)]
pub(super) enum Delivery {
    Standard,
    Realtime,
    AlternateStack,
    Exception,
}

impl Delivery {
    /// Every entry path, each with a fresh set of canaries.
    pub(super) const ALL: [Self; 4] = [
        Self::Standard,
        Self::Realtime,
        Self::AlternateStack,
        Self::Exception,
    ];
    fn number(self) -> libc::c_int {
        match self {
            Self::Realtime => REALTIME_SIGNAL,
            Self::Exception => libc::SIGILL,
            _ => libc::SIGUSR1,
        }
    }
}

/// Why a controlled signal probe failed.
#[derive(Debug)]
pub(super) enum Failure {
    /// A signal API failed.
    Io {
        operation: &'static str,
        source: io::Error,
    },
    /// pthread_kill returned an errno.
    Send(i32),
    /// The sender panicked.
    Sender,
    /// The handler did not run during the assembly interval.
    Missing,
    /// A handler configured for the alternate stack ran elsewhere.
    WrongStack { address: usize },
    /// A vector byte changed.
    Vector {
        byte: usize,
        expected: u8,
        actual: u8,
    },
    /// An opmask changed.
    Mask {
        index: usize,
        expected: u64,
        actual: u64,
    },
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => write!(f, "{operation} failed: {source}"),
            Self::Send(errno) => write!(f, "pthread_kill failed with errno {errno}"),
            Self::Sender => f.write_str("the signal sender panicked"),
            Self::Missing => {
                f.write_str("the handler did not run while vector registers were held")
            }
            Self::WrongStack { address } => write!(
                f,
                "handler stack {address:#x} is outside the alternate stack"
            ),
            Self::Vector {
                byte,
                expected,
                actual,
            } => write!(
                f,
                "zmm{} byte {} changed from {expected:#04x} to {actual:#04x}",
                byte / ZMM_BYTES,
                byte % ZMM_BYTES
            ),
            Self::Mask {
                index,
                expected,
                actual,
            } => write!(
                f,
                "k{index} changed from {expected:#018x} to {actual:#018x}"
            ),
        }
    }
}
impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
fn io_failure(operation: &'static str) -> Failure {
    Failure::Io {
        operation,
        source: io::Error::last_os_error(),
    }
}

/// Clobbers each additional AVX-512 component and records handler execution.
///
/// # Safety
/// CPU and OS AVX-512F support must have been checked.
#[target_feature(enable = "avx512f")]
unsafe fn clobber() {
    let stack: usize;
    // SAFETY: AVX-512 was checked; only declared registers are changed.
    unsafe {
        asm!(
            "vpxord zmm0, zmm0, zmm0",
            "vpxord zmm16, zmm16, zmm16",
            "kxorw k1, k1, k1",
            "mov {stack}, rsp",
            stack = out(reg) stack,
            out("zmm0") _, out("zmm16") _, out("k1") _,
            options(nostack, nomem),
        );
    }
    HANDLER_STACK.store(stack, Ordering::Relaxed);
    SIGNAL_PHASE.store(2, Ordering::Release);
}

/// # Safety
/// Install only after checking CPU and OS AVX-512 support.
unsafe extern "C" fn handler(_: libc::c_int) {
    // SAFETY: the probe installs this handler only after checking AVX-512.
    unsafe { clobber() };
}

/// Resumes after the two-byte UD2 while preserving the public context ABI.
///
/// # Safety
/// The runtime must pass its valid mutable ucontext; AVX-512 must be enabled.
unsafe extern "C" fn exception_handler(
    _: libc::c_int,
    _: *mut libc::siginfo_t,
    raw: *mut libc::c_void,
) {
    // SAFETY: SA_SIGINFO supplies the live context for this signal invocation.
    let context = unsafe { &mut *raw.cast::<libc::ucontext_t>() };
    #[cfg(target_os = "redox")]
    {
        context.uc_mcontext.rip = context.uc_mcontext.rip.wrapping_add(2);
    }
    #[cfg(target_os = "linux")]
    {
        context.uc_mcontext.gregs[libc::REG_RIP as usize] =
            context.uc_mcontext.gregs[libc::REG_RIP as usize].wrapping_add(2);
    }
    // SAFETY: the probe checks AVX-512 before installing this handler.
    unsafe { clobber() };
}

/// Restores the prior alternate stack before its backing allocation is dropped.
fn restore_stack(old: Option<libc::stack_t>, storage: Vec<u8>) -> Result<(), Failure> {
    if let Some(old) = old {
        // SAFETY: old is the valid disposition returned by sigaltstack.
        if unsafe { libc::sigaltstack(&old, core::ptr::null_mut()) } != 0 {
            let error = io_failure("restore sigaltstack");
            // The OS still owns this stack registration. Retain its storage
            // until the test process reports the error and exits.
            core::mem::forget(storage);
            return Err(error);
        }
    }
    Ok(())
}

/// Runs register canaries under an already installed handler.
///
/// # Safety
/// AVX-512 must be enabled and the selected handler must be installed.
unsafe fn probe(delivery: Delivery) -> Result<(), Failure> {
    SIGNAL_PHASE.store(0, Ordering::Release);
    let exception = matches!(delivery, Delivery::Exception);
    // SAFETY: the current thread remains alive until its sender is joined.
    let target = unsafe { libc::pthread_self() } as usize;
    let sender = if exception {
        None
    } else {
        Some(
            thread::Builder::new()
                .spawn(move || {
                    while SIGNAL_PHASE.load(Ordering::Acquire) == 0 {
                        thread::yield_now();
                    }
                    // SAFETY: the target remains alive until this sender is joined.
                    let errno =
                        unsafe { libc::pthread_kill(target as libc::pthread_t, delivery.number()) };
                    if errno != 0 {
                        SIGNAL_PHASE.store(2, Ordering::Release);
                        return Err(Failure::Send(errno));
                    }
                    Ok(())
                })
                .map_err(|source| Failure::Io {
                    operation: "spawn sender",
                    source,
                })?,
        )
    };
    let mut src = Zmm([0; 32 * ZMM_BYTES]);
    src.0.copy_from_slice(&pattern(0x512, 32 * ZMM_BYTES));
    let wide = cpu_has_avx512bw();
    let ksrc = [if wide { 0xa551_8765_dead_5aa5 } else { 0x5aa5 }; K_SLOTS];
    let mut dst = Zmm([0; 32 * ZMM_BYTES]);
    let mut kdst = [0; K_SLOTS];
    let flags = if wide { WIDE_MASKS } else { 0 } | if exception { TRAP } else { 0 };
    // SAFETY: CPU support and the handler are checked, and the typed buffers
    // cover every register. A synchronous trap needs no waiting loop.
    let remaining = unsafe {
        hold_registers(
            &src,
            &ksrc,
            &mut dst,
            &mut kdst,
            if exception { 1 } else { 8_000_000_000 },
            if exception { None } else { Some(&SIGNAL_PHASE) },
            flags,
        )
    };
    if let Some(sender) = sender {
        sender.join().map_err(|_| Failure::Sender)??;
    }
    // A late signal must not turn an expired assembly interval into a pass.
    if (!exception && remaining == 0) || SIGNAL_PHASE.load(Ordering::Acquire) != 2 {
        return Err(Failure::Missing);
    }
    for (byte, (&expected, &actual)) in src.0.iter().zip(&dst.0).enumerate() {
        if expected != actual {
            return Err(Failure::Vector {
                byte,
                expected,
                actual,
            });
        }
    }
    for index in 0..K_SLOTS {
        if ksrc[index] != kdst[index] {
            return Err(Failure::Mask {
                index,
                expected: ksrc[index],
                actual: kdst[index],
            });
        }
    }
    Ok(())
}

/// Verifies every vector/mask canary across a real signal or exception handler.
///
/// # Errors
/// Reports signal API errors, missing delivery, sender panic or altered state.
///
/// # Safety
/// The CPU and OS must support AVX-512F and its enabled state components.
pub(super) unsafe fn test(delivery: Delivery) -> Result<(), Failure> {
    let mut storage = if matches!(delivery, Delivery::AlternateStack) {
        vec![0u8; 65536]
    } else {
        Vec::new()
    };
    let old_stack = if storage.is_empty() {
        None
    } else {
        // SAFETY: stack_t's integer/pointer fields have valid zero representations.
        let mut old: libc::stack_t = unsafe { core::mem::zeroed() };
        let stack = libc::stack_t {
            ss_sp: storage.as_mut_ptr().cast(),
            ss_flags: 0,
            ss_size: storage.len(),
        };
        // SAFETY: this allocation remains live until the registration is restored.
        if unsafe { libc::sigaltstack(&stack, &mut old) } != 0 {
            return Err(io_failure("sigaltstack"));
        }
        Some(old)
    };
    // SAFETY: sigaction's integer/pointer/mask fields have valid zero representations.
    let (mut action, mut old): (libc::sigaction, libc::sigaction) = unsafe { core::mem::zeroed() };
    action.sa_sigaction = if matches!(delivery, Delivery::Exception) {
        action.sa_flags |= libc::SA_SIGINFO;
        exception_handler as *const () as usize
    } else {
        handler as *const () as usize
    };
    if matches!(delivery, Delivery::AlternateStack) {
        action.sa_flags |= libc::SA_ONSTACK;
    }
    // SAFETY: both disposition buffers are initialized and valid.
    if unsafe { libc::sigaction(delivery.number(), &action, &mut old) } != 0 {
        let error = io_failure("sigaction");
        restore_stack(old_stack, storage)?;
        return Err(error);
    }
    // SAFETY: the selected handler is installed and AVX-512 was checked.
    let result = unsafe { probe(delivery) };
    let stack_address = HANDLER_STACK.load(Ordering::Acquire);
    let expected_stack = storage.as_ptr().addr()..storage.as_ptr().addr() + storage.len();
    // SAFETY: restore the old action after the probe and its sender have returned.
    let restored = unsafe { libc::sigaction(delivery.number(), &old, core::ptr::null_mut()) };
    let action_error = if restored != 0 {
        Some(io_failure("restore sigaction"))
    } else {
        None
    };
    restore_stack(old_stack, storage)?;
    if let Some(error) = action_error {
        return Err(error);
    }
    result?;
    if matches!(delivery, Delivery::AlternateStack) && !expected_stack.contains(&stack_address) {
        return Err(Failure::WrongStack {
            address: stack_address,
        });
    }
    Ok(())
}
