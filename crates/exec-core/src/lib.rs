//! The portable executor core for Ferric Oxide kernel async work ([issue #178] and
//! [#180]): task identity, state transitions, bounded ready queues, thread-safe
//! allocation-free wake and cancel notification, fake-clock deadlines, and the
//! sleep/readiness handshake that keeps a wake from stranding a task between its
//! empty-queue check and its sleep.
//!
//! The kernel binds this core to its scheduler in [#181] (workers, wake routing,
//! timers): this crate itself has no clock of its own (a [`Clock`] is injected, so
//! host tests run a [`FakeClock`]), no periodic polling, no stack for tasks, and no
//! kernel types. Everything here is host-testable and runs under Miri: the crate is
//! `forbid(unsafe_code)`.
//!
//! The model, from the design ([ferric-async.md]);
//!
//! - A *task* is one unit of async work with no stack of its own. Its pollable head
//!   is a `FnMut(&mut TaskCtx) -> Step` closure stored in a fixed pool slot; the
//!   pool bounds live tasks ([`Executor::spawn`] is fallible) and never panics.
//! - A task is `Ready`, `Running`, waiting for a wake (`Waiting`), waiting for a
//!   deadline (`Sleeping`), `Completed` or `Cancelled`. Task ids carry a
//!   generation, so a stale [`Waker`] handle (from before a completion, a reap
//!   or a reuse) cannot act on a new occupant.
//! - [`Executor::poll`] runs at most a configured budget of ready tasks, draining
//!   due deadlines and the wake channel between tasks. A budget does not interrupt
//!   a single unbounded poll, and a cancel that lands during one applies at the
//!   task's next step boundary.
//! - Wakes and cancellations from another thread or an interrupt context only set
//!   an atomic flag on the task's slot and push one entry into the executor's
//!   bounded wake channel: no allocation, no lock, no destructor runs on the
//!   notifying path. The owner (the worker) performs state changes and drops task
//!   bodies.
//! - The executor exposes [`Executor::next_deadline`] and [`Executor::schedulable`]:
//!   the worker has nothing to do when nothing is schedulable and there is no
//!   deadline, and never has to poll to make progress.
//!
//! Work that this core deliberately does not do (see the design): task migration,
//! work stealing, real-time metadata or reservation authority, and any blocking
//! poll. Dropping a task does not cancel external I/O: the request-scoped
//! resources are owned by the kernel's cancellation protocol ([#182]) until the
//! device or scheme acknowledges.
//!
//! [issue #178]: https://github.com/rexlunae/ferric-oxide/issues/178
//! [#180]: https://github.com/rexlunae/ferric-oxide/issues/180
//! [#181]: https://github.com/rexlunae/ferric-oxide/issues/181
//! [#182]: https://github.com/rexlunae/ferric-oxide/issues/182
//! [ferric-async.md]: https://ferric-oxide.rexlunae.dev/ferric-async.html

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "alloc")]
extern crate alloc;

mod clock;
mod task;

#[cfg(feature = "alloc")]
pub mod exec;
#[cfg(all(test, feature = "alloc"))]
mod exec_tests;

pub use clock::{Clock, FakeClock};
pub use task::{Step, TaskCtx, TaskId};

#[cfg(feature = "alloc")]
pub use exec::{Admission, CancelError, Config, Executor, JoinOutcome, PollOutcome, Stats, Waker};
