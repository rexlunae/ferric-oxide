//! Ferric Oxide: the process manager's tracepoints (`sched.proc_fork`, `sched.proc_exec`,
//! `sched.proc_exit` and `sched.proc_wait`, `ferric_trace::kernel::USER_EVENTS`).
//!
//! Only procmgr knows when a process forks, execs, exits or is reaped, so it reports these
//! events itself: it writes a `ferric_trace::abi::UserRecord` to `/scheme/kernel.trace/emit`
//! and the kernel stamps and stores it like its own records. The events are part of the
//! `sched` subsystem, so they exist only in a kernel built with `kernel.trace_sched`; the
//! first write to any other kernel fails with `EOPNOTSUPP` and procmgr stops writing. A
//! kernel without `kernel.trace` has no trace scheme, and nothing is opened.
//!
//! A write for an event that is off costs one system call and stores nothing. Tracing never
//! makes a process manager call fail: errors are logged and the records dropped.

use ferric_trace::Event;
use ferric_trace::abi::{EMIT_PATH, UserRecord};
use libredox::protocol::O_CLOEXEC;
use redox_rt::proc::FdGuard;
use syscall::flag::O_WRONLY;
use syscall::{ENOENT, EOPNOTSUPP, Error};

/// Where procmgr writes its trace records, while the kernel takes them.
pub struct ProcTrace {
    emit: Option<FdGuard>,
}

impl ProcTrace {
    /// Opens `emit` under the trace scheme's root, `root` (none when the kernel has no
    /// `kernel.trace`). A failure is logged, and the tracer then records nothing.
    pub fn open(root: Option<&FdGuard>) -> Self {
        let emit = root.and_then(
            |root| match root.openat(EMIT_PATH, O_WRONLY | O_CLOEXEC, 0) {
                Ok(emit) => Some(emit),
                // A kernel older than the user events.
                Err(Error { errno: ENOENT }) => None,
                Err(err) => {
                    log::warn!("procmgr: trace: cannot open {EMIT_PATH}: {err}");
                    None
                }
            },
        );
        Self { emit }
    }

    /// Records `event` with `args`, one argument per name in `event.args()`.
    pub fn record(&mut self, event: &Event, args: &[u64]) {
        let Some(emit) = &self.emit else {
            return;
        };
        let record = match UserRecord::new(event, args) {
            Ok(record) => record,
            Err(err) => {
                log::warn!("procmgr: trace: {}: {err}", event.name());
                return;
            }
        };
        let mut bytes = [0; UserRecord::MAX_BYTES];
        let len = record.encode(&mut bytes);
        match emit.write(bytes.get(..len).unwrap_or_default()) {
            Ok(_) => {}
            // The kernel does not compile the events in: stop asking.
            Err(Error { errno: EOPNOTSUPP }) => self.emit = None,
            Err(err) => {
                log::warn!(
                    "procmgr: trace: {}: {err}; no more process records",
                    event.name()
                );
                self.emit = None;
            }
        }
    }
}
