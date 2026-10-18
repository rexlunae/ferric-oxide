//! The parts of tracing that exist with the `trace` feature: what is compiled in, turning events
//! on and off, the per-CPU rings and writing records.

use alloc::boxed::Box;
use core::{fmt, sync::atomic::AtomicU64};

use ferric_trace::{
    kernel::CATALOG, ring::Ring, select, Event, EventSet, Level, SelectError, Subsystem,
};
use spin::Once;

pub use crate::arch::static_key::{interrupts_enabled, is_on, key as enabled, Mode};
use crate::{
    arch::static_key,
    cpu_set::{LogicalCpuId, MAX_CPU_COUNT},
};

#[cfg(not(target_arch = "x86_64"))]
compile_error!("kernel tracing (the `trace` feature) is only implemented on x86_64");

// The profiler takes one CPU out of the system (profiling.rs) and points all of its interrupt
// vectors at the wakeup handler, so it would never acknowledge the IPI live patching waits for.
#[cfg(feature = "profiling")]
compile_error!("the `trace` and `profiling` features cannot be combined yet");

/// The most verbose level compiled in, from the `trace_level_*` features.
pub const MAX_LEVEL: Level = if cfg!(feature = "trace_level_trace") {
    Level::Trace
} else if cfg!(feature = "trace_level_debug") {
    Level::Debug
} else if cfg!(feature = "trace_level_info") {
    Level::Info
} else if cfg!(feature = "trace_level_warn") {
    Level::Warn
} else {
    Level::Error
};

/// Whether `subsystem`'s tracepoints are compiled in.
const fn subsystem_compiled(subsystem: Subsystem) -> bool {
    (matches!(subsystem, Subsystem::Syscall) && cfg!(feature = "trace_syscall"))
        || (matches!(subsystem, Subsystem::Sched) && cfg!(feature = "trace_sched"))
}

/// Whether `event`'s tracepoints are compiled in: its subsystem is, and its level is within
/// [`MAX_LEVEL`].
pub const fn compiled_in(event: &Event) -> bool {
    subsystem_compiled(event.subsystem()) && event.level().within(MAX_LEVEL)
}

/// The events compiled into this kernel.
pub fn compiled() -> EventSet {
    let mut set = EventSet::new();
    for event in CATALOG.events().iter().filter(|event| compiled_in(event)) {
        set.insert(event.id());
    }
    set
}

/// Why a selection could not be turned on or off.
#[derive(Clone, Copy, Debug)]
pub enum ControlError<'a> {
    /// The selection names something that is not an event or subsystem.
    Select(SelectError<'a>),
    /// The selection names events, but none of them is compiled into this kernel.
    NotCompiled,
}

impl fmt::Display for ControlError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::Select(err) => fmt::Display::fmt(err, f),
            ControlError::NotCompiled => f.write_str("no selected event is compiled in"),
        }
    }
}

impl<'a> From<SelectError<'a>> for ControlError<'a> {
    fn from(err: SelectError<'a>) -> Self {
        ControlError::Select(err)
    }
}

/// Turns the compiled-in events that `spec` selects on or off, and returns how many changed.
///
/// [`Mode::Live`] waits for every other CPU to take an interrupt: call it from a system call,
/// not from an interrupt handler.
///
/// # Errors
///
/// [`ControlError::Select`] when `spec` does not parse, and [`ControlError::NotCompiled`] when it
/// selects events but none that this kernel has.
pub fn control(spec: &str, on: bool, mode: Mode) -> Result<usize, ControlError<'_>> {
    let selected = select(&CATALOG, spec)?;
    let events = selected.intersection(&compiled());
    if events.is_empty() && !selected.is_empty() {
        return Err(ControlError::NotCompiled);
    }
    if on && mode == Mode::Live {
        init_rings();
    }
    Ok(static_key::set(&events, on, mode))
}

/// The value of `key` in the boot environment (`KEY=VALUE` lines).
fn env_value<'a>(env: &'a [u8], key: &str) -> Option<&'a str> {
    env.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(key.as_bytes())?.strip_prefix(b"="))
        .and_then(|value| core::str::from_utf8(value).ok())
        .map(str::trim)
}

/// Turns on the events the build-time default (`trace.boot` in the kernel's `config.toml`) and
/// the boot environment's `FERRIC_TRACE` select. Runs on the BSP before the other CPUs start.
pub fn init_boot(env: &[u8]) {
    let sources = [
        ("trace.boot", option_env!("FERRIC_TRACE_BOOT").unwrap_or("")),
        ("FERRIC_TRACE", env_value(env, "FERRIC_TRACE").unwrap_or("")),
    ];
    for (source, spec) in sources {
        if spec.is_empty() {
            continue;
        }
        match control(spec, true, Mode::Boot) {
            Ok(_) => {
                for event in CATALOG.events().iter().filter(|event| is_on(event.id())) {
                    info!("trace: {} on from boot ({source})", event.name());
                }
            }
            Err(err) => {
                warn!("trace: {source}={spec:?} ignored: {err}");
            }
        }
    }
}

/// A CPU's ring.
pub type CpuRing = Ring<Box<[AtomicU64]>>;

/// The size of each CPU's ring in words: 64 KiB.
const RING_WORDS: usize = 8192;

/// Each CPU's ring, made by [`init_rings`].
static RINGS: [Once<CpuRing>; MAX_CPU_COUNT as usize] =
    [const { Once::new() }; MAX_CPU_COUNT as usize];

/// Makes a ring for every CPU that is up and has none. Called from `kmain`, once the CPUs are
/// counted, and before an event is turned on at run time.
pub fn init_rings() {
    for ring in RINGS.iter().take(crate::cpu_count() as usize) {
        ring.call_once(|| {
            Ring::with_capacity(RING_WORDS).expect("RING_WORDS is a power of two above a record")
        });
    }
}

/// CPU `cpu`'s ring, if it has one.
pub fn ring(cpu: LogicalCpuId) -> Option<&'static CpuRing> {
    RINGS.get(cpu.get() as usize)?.get()
}

/// The timestamp of a record: the TSC.
fn timestamp() -> u64 {
    // SAFETY: RDTSC reads a counter; it has no side effects and every x86_64 CPU has it.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Writes a record of `event` with `args` to this CPU's ring. The slow path of `trace_event!`.
#[cold]
#[inline(never)]
pub fn emit(event: &Event, args: &[u64]) {
    // Before `kmain` makes the rings there is nowhere to put the record.
    let Some(ring) = ring(crate::cpu_id()) else {
        return;
    };
    if interrupts_enabled() {
        // An interrupt could switch contexts mid-record and leave the ring with two producers.
        ring.count_lost();
        return;
    }
    // A full ring counts the record as lost itself.
    let _ = ring.push(event.id(), timestamp(), args);
}
