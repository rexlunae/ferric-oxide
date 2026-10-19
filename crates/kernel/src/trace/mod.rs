//! Ferric Oxide: kernel tracepoints (roadmap Phase T, `kernel.trace`).
//!
//! `trace_event!(EVENT, args...)` marks a tracepoint. The events, their ids and the record format
//! are the `ferric-trace` crate's (`crates/trace`); this module decides at build time which
//! tracepoints exist, turns them on and off, and writes their records:
//!
//! - **Build time.** Without the `trace` feature the macro expands to nothing. With it, an event
//!   is compiled in when its subsystem's feature (`trace_syscall`, `trace_sched`, `trace_scheme`) is on and its
//!   level is within the `trace_level_*` maximum; other events also expand to nothing, and their
//!   arguments are never evaluated.
//! - **Static keys.** A compiled-in tracepoint is a 5-byte NOP until its event is enabled, when it
//!   is patched to a jump to the code that records it (`arch::static_key`).
//! - **Boot time.** `FERRIC_TRACE=<selection>` in the boot environment, or the build-time default
//!   `trace.boot` in the kernel's `config.toml`, enables events before the other CPUs start.
//! - **Run time.** The `kernel.trace` scheme (`scheme::trace`) enables and disables events and
//!   drains the rings, or maps them read-only into a reader.
//!
//! - **Crash dump.** With `trace_crash`, a panic leaves its message and the newest records in
//!   memory that survives a warm reboot, for the next boot to hand out (`crash`).
//!
//! Records go to a ring per CPU (`ferric_trace::ring::Ring` in the shared layout, 64 KiB of
//! records after a header page). Tracepoints may only sit
//! where interrupts are off, as in the system call path and the scheduler: then nothing else
//! runs on the CPU while a record is written, and the CPU's ring has a single producer.

#[cfg(feature = "trace_crash")]
pub mod crash;
#[cfg(feature = "trace")]
mod imp;

#[cfg(feature = "trace")]
pub use imp::*;

/// Records an event of the `ferric_trace::kernel` catalog, with up to eight arguments that are
/// cast to `u64`, if the event is compiled in and enabled.
///
/// The arguments are only evaluated when the event is enabled. Use it only where interrupts are
/// disabled (see the module documentation).
macro_rules! trace_event {
    ($event:path $(, $arg:expr)* $(,)?) => {{
        #[cfg(feature = "trace")]
        {
            const EVENT: ::ferric_trace::Event = $event;
            if const { $crate::trace::compiled_in(&EVENT) }
                && $crate::trace::enabled::<{ EVENT.id().get() }>()
            {
                $crate::trace::emit(&EVENT, &[$(($arg) as u64),*]);
            }
        }
    }};
}
