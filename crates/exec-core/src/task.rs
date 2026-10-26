//! Task identity, the poll step and the per-task context.
//!
//! A [`TaskId`] is the pair of a pool slot and a generation: reusing a slot bumps
//! the generation, so a stale id (a `Waker` held past completion,
//! a late response, a cancelled task's old handle) fails against the new occupant
//! instead of acting on it.

use core::fmt;
use core::sync::atomic::AtomicU64;

/// The per-task wake bit, also consumed by the allocation-backed executor.
pub(crate) const WAKE: u64 = 1;

/// A task's identity: its pool slot and the slot's generation.
///
/// The generation makes ids unique across reuses: the pool reuses a slot only
/// with a new generation, and every handle and queue entry carries the full id,
/// so a stale handle can never act on a newer task in the same slot. Generations
/// are 32-bit; exhausted slots retire instead of wrapping. Ids belong to one
/// executor and must not be used with another executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId {
    /// The pool slot.
    index: u16,
    /// The slot's generation.
    generation: u32,
}

impl TaskId {
    /// The id of slot `index` at `generation`.
    pub const fn new(index: u16, generation: u32) -> Self {
        Self { index, generation }
    }

    /// The pool slot.
    pub const fn index(self) -> usize {
        self.index as usize
    }

    /// The slot's generation.
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "task {}@{generation}",
            self.index,
            generation = self.generation
        )
    }
}

/// What a task's pollable head asks the executor to do next.
///
/// The step is the whole interface between a task and the executor: the task
/// never touches queues, clocks or state itself, so the executor can account for
/// every transition. A single poll runs until it returns one of these; the
/// executor's batch budget applies between tasks, never inside one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The task finished with `result`, which a later
    /// `Executor::join` returns.
    Done {
        /// The task's result.
        result: u64,
    },
    /// The task has nothing to do and must be woken (or cancelled). The executor
    /// re-checks the wake flag once more before sleeping (the sleep/readiness
    /// handshake), so a wake that arrived during the poll is never lost.
    Wait,
    /// The task has nothing to do until the clock reaches `deadline`
    /// (nanoseconds, the [`Clock`](crate::Clock) scale). The executor may only
    /// run the task again once the deadline is due, by wake, or by cancel.
    WaitDeadline {
        /// The earliest instant the task may run again.
        deadline: u64,
    },
    /// The task asks to run again later without a wake (a voluntary yield). The
    /// executor re-enqueues it at the tail of the ready queue, bounded by the
    /// batch budget.
    Yield,
}

/// What a running task may observe about itself.
///
/// The executor creates one context per poll and passes it to the task body.
///
/// Cancellation is *not* observable from inside a poll: the executor processes
/// cancel requests between tasks, never inside one, so a poll that never returns
/// runs to completion and a cancel issued while a task runs takes effect at the
/// task's next step boundary (documented in the crate and the design).
pub struct TaskCtx<'a> {
    /// The task's slot's wake flag, so [`wake`](TaskCtx::wake) is visible to the
    /// executor's sleep handshake. Constructed by the executor only.
    pub(crate) wake: &'a AtomicU64,
}

impl TaskCtx<'_> {
    /// Asks the executor to run this task again after this poll returns, as if a
    /// waker had fired: the task's wake flag is set, so the sleep/readiness
    /// handshake turns a following `Wait` into a re-enqueue instead of a sleep.
    ///
    /// This is the deterministic, single-threaded form of self-wake; a task can
    /// also hold its own `Waker` and wake it from any thread.
    pub fn wake(&mut self) {
        self.wake
            .fetch_or(WAKE, core::sync::atomic::Ordering::Release);
    }
}
