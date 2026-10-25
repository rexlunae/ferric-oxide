//! The executor: the fixed task pool, the ready queue, the thread-safe wake
//! channel and the poll loop, with the sleep/readiness handshake.
//!
//! # Wake channel
//!
//! Cross-thread wakes and cancels never touch the executor's queues: they set an
//! atomic flag on the task's slot and push one entry into a bounded ring whose
//! capacity is twice the pool size. A flag transitions 0→1 at most once per
//! drain, and the drain clears it, so at most one wake entry and one cancel entry
//! per slot are in flight and a claim can never pass the consumer by more than
//! the capacity. Entries carry the slot's generation, so a stale waker's entry
//! is dropped against a reused slot. The drain may wait for a claim whose
//! producer is between its claim and its store (a producer is an interrupt path
//! on another CPU); if it cannot make progress it falls back to scanning every
//! slot's flags, so a request is never lost. A producer that never finishes
//! (a wedged CPU) keeps the scan in charge of that slot until it returns; the
//! wake is re-applied, never dropped.

use alloc::boxed::Box;
use alloc::collections::{BinaryHeap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::clock::Clock;
use crate::task::{Step, TaskCtx, TaskId};

/// The wake flag of a slot's flag word.
pub(crate) const WAKE: u64 = 1 << 0;
/// The cancel flag of a slot's flag word.
const CANCEL: u64 = 1 << 1;
/// A channel entry that cancels its task.
const KIND_CANCEL: u64 = 1 << 63;
/// Where a slot's flag word keeps its generation.
const GEN_SHIFT: u32 = 2;
/// The generation mask of a slot's flag word.
const GEN_MASK: u64 = (u16::MAX as u64) << GEN_SHIFT;

/// The generation a slot returns to after wrapping; 0 stays reserved.
const MAX_GENERATION: u16 = u16::MAX;

/// A task slot's flag word: `generation << 2 | cancel | wake`.
fn flags(generation: u16, wake: bool, cancel: bool) -> u64 {
    (u64::from(generation) << GEN_SHIFT) | u64::from(wake) | (u64::from(cancel) << 1)
}

/// A task's pollable head: how a task asks the executor for its next step.
type TaskBody = Box<dyn FnMut(&mut TaskCtx) -> Step>;

/// A task's state, owned by the executor thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Queued, will run in a later batch.
    Ready,
    /// A poll is running its body right now.
    Running,
    /// Waiting for a wake (or cancel); the worker may sleep.
    Waiting,
    /// Waiting for its deadline; the scheduler arms the earliest one.
    Sleeping,
    /// Finished; the result waits for a join or the release of join interest.
    Completed,
    /// Cancelled; it will never run again.
    Cancelled,
    /// No task.
    Free,
}

/// One pool slot.
struct Slot {
    /// The slot's generation (also the upper bits of the flag word).
    generation: u16,
    /// The task's state.
    state: State,
    /// The wake and cancel flags, shared with every [`Waker`] of this task.
    flags: Arc<AtomicU64>,
    /// The task's current sleep deadline, when `state` is `Sleeping`.
    deadline: Option<u64>,
    /// Whether a joiner may still come for this task; cleared by `drop_interest`.
    join_interest: bool,
    /// The completed result, when `state` is `Completed`.
    result: Option<u64>,
    /// The task body; absent while running (taken out) or finished.
    body: Option<TaskBody>,
}

impl Slot {
    fn free(generation: u16) -> Self {
        Self {
            generation,
            state: State::Free,
            // A never-spawned slot has no flag word clients; generation 0 in the
            // word makes any stray waker fail its check.
            flags: Arc::new(AtomicU64::new(0)),
            deadline: None,
            join_interest: false,
            result: None,
            body: None,
        }
    }
}

/// One claimed ring position.
struct Entry {
    /// `KIND_CANCEL` set, or a wake.
    kind: u64,
    /// The generation the entry was pushed for.
    generation: u16,
    /// The pool slot.
    index: u16,
}

/// The ring cell of the wake channel.
struct Cell {
    /// `0` empty, `1` claimed but the entry not yet stored, `2` ready.
    state: AtomicU8,
    /// The entry, packed as the waker wrote it (kind, generation, index).
    entry: AtomicU64,
}

/// The thread-safe notification channel of an executor.
struct Channel {
    /// Cells, `2 × capacity` (two entries per slot, in flight at most once each).
    cells: Box<[Cell]>,
    /// The mask for the capacity (a power of two).
    mask: usize,
    /// The next position a waker claims (unbounded; the mask wraps it).
    head: AtomicUsize,
    /// The next position the executor reads.
    tail: AtomicUsize,
}

impl Channel {
    fn new(capacity: usize) -> Self {
        let len = (capacity * 2).next_power_of_two();
        let cells = (0..len)
            .map(|_| Cell {
                state: AtomicU8::new(0),
                entry: AtomicU64::new(0),
            })
            .collect();
        Self {
            cells,
            mask: len - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Claims a position and writes `entry`; the entry becomes visible in order.
    ///
    /// Position claims can overtake stores, so the reader waits for a claimed
    /// cell to turn ready rather than reading straight through. The capacity is
    /// twice the pool size and a slot's flags bound its in-flight entries, so a
    /// claim cannot pass the reader by more than the capacity: this never blocks
    /// a caller for a position.
    fn push(&self, kind: u64, generation: u16, index: u16) {
        let entry = kind | (u64::from(generation) << 16) | u64::from(index);
        let position = self.head.fetch_add(1, Ordering::AcqRel);
        let cell = &self.cells[position & self.mask];
        cell.state.store(1, Ordering::Release);
        cell.entry.store(entry, Ordering::Release);
        cell.state.store(2, Ordering::Release);
    }

    /// The entry at the next unread position, waiting briefly for its writer.
    ///
    /// Returns [`Pop::Empty`] when there is nothing to read, and
    /// [`Pop::Saturated`] when the next writer has not finished within
    /// `spin_bound` attempts (the caller then scans the pool's flags instead).
    fn pop(&self, spin_bound: u32) -> Pop {
        if self.tail.load(Ordering::Acquire) == self.head.load(Ordering::Acquire) {
            return Pop::Empty;
        }
        let cell = &self.cells[self.tail.load(Ordering::Acquire) & self.mask];
        let mut spins = 0;
        while cell.state.load(Ordering::Acquire) != 2 {
            spins += 1;
            if spins > spin_bound {
                return Pop::Saturated;
            }
            core::hint::spin_loop();
        }
        let word = cell.entry.load(Ordering::Acquire);
        cell.state.store(0, Ordering::Release);
        self.tail.fetch_add(1, Ordering::AcqRel);
        Pop::Entry(Entry {
            kind: word & KIND_CANCEL,
            generation: ((word >> 16) & 0xffff) as u16,
            index: (word & 0xffff) as u16,
        })
    }
}

/// What [`Channel::pop`] found.
enum Pop {
    /// Nothing unread.
    Empty,
    /// A claim has been held too long; scan the flags instead. The request is
    /// not lost: its flag is still set, and the scan applies it.
    Saturated,
    /// A whole entry.
    Entry(Entry),
}

/// The executor's bounded configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Task slots; also the bound on live tasks and queued work.
    pub capacity: u16,
    /// The maximum number of tasks one [`poll`](Executor::poll) may run. A batch
    /// budget never interrupts a single poll.
    pub budget: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capacity: 1024,
            budget: 64,
        }
    }
}

/// Why [`Executor::spawn`] refused a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Every slot is live; nothing was spawned and nothing was changed.
    PoolFull,
}

/// Why [`Executor::cancel`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelError {
    /// The id's generation does not match its slot: it was reaped and reused, or
    /// never existed.
    InvalidId,
    /// The task already completed or was cancelled.
    NotLive,
}

/// The settled outcome of a task, from [`Executor::join`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// The task finished with this result.
    Done(u64),
    /// The task was cancelled before it finished.
    Cancelled,
}

/// What one [`Executor::poll`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollOutcome {
    /// Tasks whose body ran.
    pub ran: u64,
    /// Ready tasks filtered because a cancel had settled them.
    pub cancelled: u64,
    /// Tasks woken out of a wait by the channel.
    pub woken: u64,
    /// Sleeping tasks whose deadline came due.
    pub due: u64,
    /// Still queued after the batch.
    pub ready_after: u64,
}

/// The executor's cheap counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Tasks ever spawned.
    pub spawned: u64,
    /// Tasks that completed.
    pub completed: u64,
    /// Tasks cancelled.
    pub cancelled: u64,
    /// Channel entries drained (wakes and cancels).
    pub notifications: u64,
    /// Entries dropped for a stale generation.
    pub stale: u64,
    /// Polls taken.
    pub polls: u64,
}

/// A thread-safe handle to one task, for waking and cancelling it from another
/// thread or an interrupt context.
///
/// The handle is `Send + Sync` and its methods allocate nothing, take no locks
/// and run no destructors: they set the slot's flag and push one channel entry.
/// The executor's owner applies the outcome. A handle for a reaped or reused
/// slot is stale and its calls are dropped ([`is_stale`](Waker::is_stale)).
#[derive(Clone)]
pub struct Waker {
    channel: Arc<Channel>,
    flags: Arc<AtomicU64>,
    id: TaskId,
}

impl Waker {
    /// The task this handle names.
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Whether the slot still holds the generation this handle was made for.
    pub fn is_stale(&self) -> bool {
        self.generation() != self.id.generation()
    }

    /// Asks the executor to run the task again, coalescing duplicate wakes. A
    /// wake for a completed or reaped task is dropped.
    pub fn wake(&self) {
        self.notify(WAKE, 0);
    }

    /// Asks the executor to cancel the task (see
    /// [`Executor::cancel`](Executor::cancel) for the semantics). Cancelling an
    /// already settled task is dropped.
    pub fn cancel(&self) {
        self.notify(CANCEL, KIND_CANCEL);
    }

    /// The flag word's current generation (0 for a never-spawned slot).
    fn generation(&self) -> u16 {
        ((self.flags.load(Ordering::Acquire) & GEN_MASK) >> GEN_SHIFT) as u16
    }

    /// Sets `flag` (0→1) and pushes the matching channel entry; the flag
    /// transition bounds the ring, so the push cannot fail.
    fn notify(&self, flag: u64, kind: u64) {
        let mut now = self.flags.load(Ordering::Acquire);
        loop {
            if (now & GEN_MASK) >> GEN_SHIFT != u64::from(self.id.generation()) {
                return; // The slot was reaped and reused, or never held this task.
            }
            if now & flag != 0 {
                return; // Already requested; the request is either pending or spent.
            }
            match self.flags.compare_exchange_weak(
                now,
                now | flag,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => now = actual,
            }
        }
        self.channel
            .push(kind, self.id.generation(), self.id.index() as u16);
    }
}

/// The executor. Owned by its worker (initially one worker, later pinned per-CPU
/// workers); only the [`Waker`] handles are thread-safe.
pub struct Executor {
    config: Config,
    clock: Arc<dyn Clock>,
    slots: Vec<Slot>,
    /// Free slots, by index.
    free: VecDeque<u16>,
    /// Ready tasks, FIFO.
    ready: VecDeque<TaskId>,
    /// Sleeping tasks, earliest deadline first.
    sleepers: BinaryHeap<Reverse<(u64, TaskId)>>,
    /// The cross-thread notification channel.
    channel: Arc<Channel>,
    stats: Stats,
}

impl Executor {
    /// An executor with `config`, reading time from `clock` (nothing here polls
    /// the clock; it is only consulted when the worker enters the executor or
    /// asks whether it may sleep).
    pub fn new(clock: Arc<dyn Clock>, config: Config) -> Self {
        assert!(
            config.capacity > 0,
            "an executor needs at least one task slot"
        );
        assert!(
            config.budget > 0,
            "an executor needs a poll budget of at least one"
        );
        let capacity = usize::from(config.capacity);
        let slots = (0..capacity)
            .map(|index| Slot::free(index as u16 + 1))
            .collect();
        let free = (0..capacity).map(|index| index as u16).collect();
        Self {
            config,
            clock,
            slots,
            free,
            ready: VecDeque::with_capacity(capacity),
            sleepers: BinaryHeap::new(),
            channel: Arc::new(Channel::new(capacity)),
            stats: Stats::default(),
        }
    }

    /// The configuration this executor was built with.
    pub fn config(&self) -> Config {
        self.config
    }

    /// Spawns `body` as a task, returning its id, or [`Admission::PoolFull`]
    /// when every slot is live: nothing is spawned, nothing is changed, and no
    /// queue ever panics.
    pub fn spawn(
        &mut self,
        body: impl FnMut(&mut TaskCtx) -> Step + 'static,
    ) -> Result<TaskId, Admission> {
        let index = self.free.pop_front().ok_or(Admission::PoolFull)?;
        let slot = &mut self.slots[usize::from(index)];
        debug_assert_eq!(slot.state, State::Free);
        let generation = slot.generation;
        // Clear both flags for the new occupant before the id can escape.
        slot.flags
            .store(flags(generation, false, false), Ordering::Release);
        slot.state = State::Ready;
        slot.join_interest = true;
        slot.deadline = None;
        slot.result = None;
        slot.body = Some(Box::new(body));
        let id = TaskId::new(index, generation);
        self.ready.push_back(id);
        self.stats.spawned += 1;
        Ok(id)
    }

    /// A [`Waker`] for `id`, or `None` when the id is stale or settled.
    pub fn waker(&self, id: TaskId) -> Option<Waker> {
        let slot = &self.slots[id.index()];
        if slot.generation != id.generation()
            || !matches!(
                slot.state,
                State::Ready | State::Running | State::Waiting | State::Sleeping
            )
        {
            return None;
        }
        Some(Waker {
            channel: Arc::clone(&self.channel),
            flags: Arc::clone(&slot.flags),
            id,
        })
    }

    /// Runs up to `config.budget` ready tasks: first the due sleepers and the
    /// channel, then the batch, draining the channel between tasks. A single
    /// task's poll is never interrupted; a task that never returns keeps the
    /// worker.
    pub fn poll(&mut self) -> PollOutcome {
        let mut outcome = PollOutcome {
            ran: 0,
            cancelled: 0,
            woken: 0,
            due: 0,
            ready_after: 0,
        };
        self.stats.polls += 1;
        let now = self.clock.now();
        self.wake_due(now, &mut outcome);
        self.drain(&mut outcome);
        for _ in 0..self.config.budget {
            let Some(id) = self.ready.pop_front() else {
                break;
            };
            self.drain(&mut outcome);
            let slot = &mut self.slots[id.index()];
            if slot.generation != id.generation() {
                continue; // A stale queue entry; impossible, but never act on it.
            }
            if slot.state == State::Cancelled {
                outcome.cancelled += 1;
                continue;
            }
            let mut body = slot.body.take().expect("a ready task has a body");
            slot.state = State::Running;
            let step = {
                let mut ctx = TaskCtx { wake: &slot.flags };
                body(&mut ctx)
            };
            match step {
                Step::Done { result } => {
                    // The body is dropped here, exactly once; the slot and the
                    // result stay for a join (or the release of join interest).
                    drop(body);
                    let slot = &mut self.slots[id.index()];
                    slot.state = State::Completed;
                    slot.result = Some(result);
                    slot.flags
                        .store(flags(slot.generation, false, false), Ordering::Release);
                    self.stats.completed += 1;
                    if !slot.join_interest {
                        self.reap(id);
                    }
                }
                Step::Wait | Step::WaitDeadline { .. } | Step::Yield => {
                    let slot = &mut self.slots[id.index()];
                    let woken_early = slot.flags.load(Ordering::Acquire) & WAKE != 0;
                    let settled = match step {
                        Step::Wait => {
                            if woken_early {
                                // The sleep handshake: a wake from during the poll
                                // (or the task's own `ctx.wake`) means "run again",
                                // not "sleep now".
                                State::Ready
                            } else {
                                State::Waiting
                            }
                        }
                        Step::WaitDeadline { deadline } => {
                            if woken_early || deadline <= now {
                                // A wake beats the sleep; a deadline already past
                                // is never discarded or delayed.
                                State::Ready
                            } else {
                                slot.deadline = Some(deadline);
                                self.sleepers.push(Reverse((deadline, id)));
                                State::Sleeping
                            }
                        }
                        Step::Yield => State::Ready,
                        Step::Done { .. } => unreachable!(),
                    };
                    slot.state = settled;
                    match settled {
                        State::Cancelled => {
                            drop(body);
                            self.stats.cancelled += 1;
                        }
                        State::Ready => {
                            slot.body = Some(body);
                            self.ready.push_back(id);
                        }
                        State::Waiting | State::Sleeping => {
                            slot.body = Some(body);
                        }
                        _ => unreachable!(),
                    }
                }
            }
            // The step-boundary drain: a wake or cancel that landed while the
            // body ran settles now, at the task's step boundary: a wake turns
            // the just-returned `Wait` into a re-enqueue, a cancel settles it.
            self.drain(&mut outcome);
            outcome.ran += 1;
        }
        outcome.ready_after = self.ready.len() as u64;
        outcome
    }

    /// Moves the sleepers whose deadline is due or past into the ready queue.
    fn wake_due(&mut self, now: u64, outcome: &mut PollOutcome) {
        while let Some(&Reverse((deadline, id))) = self.sleepers.peek() {
            if deadline > now {
                break;
            }
            self.sleepers.pop();
            let slot = &mut self.slots[id.index()];
            if slot.generation != id.generation() {
                continue;
            }
            if slot.state != State::Sleeping || slot.deadline != Some(deadline) {
                continue; // Woken, cancelled or re-slept since; its own entry rules.
            }
            slot.deadline = None;
            slot.state = State::Ready;
            self.ready.push_back(id);
            outcome.due += 1;
        }
    }

    /// Consumes the channel: settles cancels, wakes waiting and sleeping tasks.
    fn drain(&mut self, outcome: &mut PollOutcome) {
        let mut saturated = false;
        loop {
            match self.channel.pop(10_000) {
                Pop::Empty => break,
                Pop::Saturated => {
                    saturated = true;
                    break;
                }
                Pop::Entry(entry) => {
                    self.stats.notifications += 1;
                    self.apply(&entry, outcome);
                }
            }
        }
        if saturated {
            // A producer is wedged between its claim and its store. Never lose
            // its request: apply every set flag. The flags are left set (the
            // ring still holds their entries), so a later drain settles them
            // and the slot cannot receive a duplicate request in between.
            for index in 0..self.slots.len() {
                let word = self.slots[index].flags.load(Ordering::Acquire);
                let id = TaskId::new(index as u16, ((word & GEN_MASK) >> GEN_SHIFT) as u16);
                if word & WAKE != 0 {
                    let slot = &mut self.slots[index];
                    if slot.generation == id.generation()
                        && (slot.state == State::Waiting || slot.state == State::Sleeping)
                    {
                        slot.state = State::Ready;
                        slot.deadline = None;
                        self.ready.push_back(id);
                        outcome.woken += 1;
                    }
                }
                if word & CANCEL != 0 {
                    self.apply(
                        &Entry {
                            kind: KIND_CANCEL,
                            generation: id.generation(),
                            index: index as u16,
                        },
                        outcome,
                    );
                }
            }
        }
    }

    /// One channel entry: cancel settles the task, wake re-queues a waiting one.
    fn apply(&mut self, entry: &Entry, outcome: &mut PollOutcome) {
        let slot = &mut self.slots[entry.index as usize];
        if slot.generation != entry.generation {
            // The slot was reaped and reused since; the entry is spent.
            self.stats.stale += 1;
            return;
        }
        if entry.kind & KIND_CANCEL != 0 {
            slot.flags.fetch_and(!CANCEL, Ordering::AcqRel);
            match slot.state {
                State::Ready | State::Waiting => {
                    // A queued task's entry is filtered when it comes up.
                    slot.state = State::Cancelled;
                    slot.body.take();
                    self.stats.cancelled += 1;
                }
                State::Sleeping => {
                    slot.state = State::Cancelled;
                    slot.deadline = None;
                    slot.body.take();
                    self.stats.cancelled += 1;
                }
                State::Running => unreachable!(
                    "a drain never runs while a body does; the step-boundary drain settles cancels"
                ),
                State::Completed | State::Cancelled => {
                    // A cancel of a settled task is dropped.
                }
                State::Free => unreachable!("a live entry names a live slot"),
            }
        } else {
            slot.flags.fetch_and(!WAKE, Ordering::AcqRel);
            if slot.state == State::Waiting || slot.state == State::Sleeping {
                slot.state = State::Ready;
                slot.deadline = None;
                self.ready
                    .push_back(TaskId::new(entry.index, entry.generation));
                outcome.woken += 1;
            }
        }
    }

    /// Requests the cancellation of `id` from the owner thread: queued, waiting
    /// and sleeping tasks are settled now (they never run again); a running task
    /// finishes its current poll and the cancel applies at its step boundary. A
    /// completed task's cancel is dropped, as is a stale id's.
    pub fn cancel(&mut self, id: TaskId) -> Result<(), CancelError> {
        let slot = &mut self.slots[id.index()];
        if slot.generation != id.generation() {
            return Err(CancelError::InvalidId);
        }
        match slot.state {
            State::Ready | State::Waiting | State::Sleeping => {
                slot.deadline = None;
                slot.state = State::Cancelled;
                slot.body.take();
                self.stats.cancelled += 1;
                Ok(())
            }
            // A running poll cannot be interrupted from inside the core; the
            // cross-thread path (a `Waker::cancel`) is drained at the step
            // boundary, and the owner thread cannot call `cancel` mid-poll.
            State::Running => Err(CancelError::NotLive),
            State::Completed | State::Cancelled => Err(CancelError::NotLive),
            State::Free => Err(CancelError::InvalidId),
        }
    }

    /// Returns a completed or cancelled task's outcome and frees its slot, or
    /// `None` while it still runs (or is queued, waiting or sleeping). The task
    /// body is always dropped at completion, whether or not it is joined.
    pub fn join(&mut self, id: TaskId) -> Option<JoinOutcome> {
        let outcome = {
            let slot = &mut self.slots[id.index()];
            if slot.generation != id.generation() {
                None
            } else {
                match slot.state {
                    State::Completed => Some(JoinOutcome::Done(
                        slot.result.take().expect("a completed task has a result"),
                    )),
                    State::Cancelled => Some(JoinOutcome::Cancelled),
                    _ => None,
                }
            }
        };
        if outcome.is_some() {
            self.reap(id);
        }
        outcome
    }

    /// Releases the join interest in `id`: the slot is freed as soon as the task
    /// settles, instead of waiting for a `join`. Stale ids are ignored.
    pub fn drop_interest(&mut self, id: TaskId) {
        let slot = &mut self.slots[id.index()];
        if slot.generation != id.generation() {
            return;
        }
        slot.join_interest = false;
        if slot.state == State::Completed {
            self.reap(id);
        }
    }

    /// Frees the slot of a settled task: clears every bit, returns the slot to
    /// the free list and bumps the generation for the next occupant.
    fn reap(&mut self, id: TaskId) {
        let slot = &mut self.slots[id.index()];
        debug_assert!(matches!(slot.state, State::Completed | State::Cancelled));
        slot.generation = if slot.generation == MAX_GENERATION {
            1
        } else {
            slot.generation + 1
        };
        slot.state = State::Free;
        slot.deadline = None;
        slot.join_interest = false;
        slot.result = None;
        slot.body = None;
        slot.flags.store(0, Ordering::Release);
        self.free.push_back(id.index() as u16);
    }

    /// The earliest deadline a sleeping task waits for, for the kernel's shared
    /// rearm protocol; `None` when no task waits on time. The executor never
    /// reads the clock on its own, so nothing here polls time.
    pub fn next_deadline(&self) -> Option<u64> {
        self.sleepers.peek().map(|Reverse((deadline, _))| *deadline)
    }

    /// Whether the worker has work: a ready task, a notification pending in the
    /// channel, or a sleeping task that might already be due. When this is false
    /// and [`next_deadline`](Executor::next_deadline) is `None`, the worker may
    /// sleep until a wake or a rearm.
    pub fn schedulable(&self) -> bool {
        if !self.ready.is_empty() {
            return true;
        }
        if self.channel.head.load(Ordering::Acquire) != self.channel.tail.load(Ordering::Acquire) {
            return true;
        }
        match self.next_deadline() {
            Some(deadline) => self.clock.now() >= deadline,
            None => false,
        }
    }

    /// The counters since construction.
    pub fn stats(&self) -> Stats {
        self.stats
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Every live task body is dropped exactly once, here; a worker teardown
        // drops whatever ran on it. The kernel's cancellation protocol must have
        // settled external request ownership before the executor goes.
        for slot in &mut self.slots {
            slot.body.take();
        }
    }
}
