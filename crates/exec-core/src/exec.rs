//! The executor: the fixed task pool, the ready queue, the thread-safe wake
//! channel and the poll loop, with the sleep/readiness handshake.
//!
//! # Wake channel
//!
//! Each task has atomic wake/cancel flags. A preallocated bitmap carries slot
//! hints, so stalled producers cannot reserve ring cells or overwrite unread
//! entries. An in-flight producer count covers publication gaps; the owner scans
//! flags if a producer is paused. Notifications allocate nothing and never lock.
//! Kernel bindings supply a latched worker notification and arm it before
//! [`Executor::prepare_sleep`]. No kernel worker is bound by this crate itself.

use alloc::boxed::Box;
use alloc::collections::{BinaryHeap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::clock::Clock;
use crate::task::{Step, TaskCtx, TaskId};

/// The wake flag of a slot's flag word.
pub(crate) use crate::task::WAKE;
/// The cancel flag of a slot's flag word.
const CANCEL: u64 = 1 << 1;
/// Where a slot's flag word keeps its generation.
const GEN_SHIFT: u32 = 2;
/// The generation mask of a slot's flag word.
const GEN_MASK: u64 = (u32::MAX as u64) << GEN_SHIFT;

/// Retire a slot at exhaustion rather than reviving an old id.
const MAX_GENERATION: u32 = u32::MAX;

/// A task slot's flag word: `generation << 2 | cancel | wake`.
fn flags(generation: u32, wake: bool, cancel: bool) -> u64 {
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
    generation: u32,
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
    fn free(generation: u32) -> Self {
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

/// The scheduler binding's latched notification of an armed worker.
///
/// Implementations used from interrupt context must be allocation-free,
/// nonblocking and must not run task destructors. The binding arms its wait
/// registration before calling [`Executor::prepare_sleep`]; a notification must
/// remain observable until the worker consumes it, even before it blocks.
pub trait Notify: Send + Sync {
    /// Signals the worker's armed wait registration.
    fn notify(&self);
}

struct PollOnly;
impl Notify for PollOnly {
    fn notify(&self) {}
}

/// Preallocated slot hints plus the producer/worker publication handshake.
struct Channel {
    pending: Box<[AtomicU64]>,
    hints: AtomicUsize,
    writers: AtomicUsize,
    armed: AtomicBool,
    notifier: Arc<dyn Notify>,
}

impl Channel {
    fn new(capacity: usize, notifier: Arc<dyn Notify>) -> Self {
        Self {
            pending: (0..capacity.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
            hints: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            armed: AtomicBool::new(false),
            notifier,
        }
    }

    fn push(&self, index: usize) {
        // Reserve the count first. A consumer may see a reservation before
        // its bit; `writers` keeps the flag-scan fallback and sleep check active.
        self.hints.fetch_add(1, Ordering::SeqCst);
        let mask = 1 << (index % 64);
        if self.pending[index / 64].fetch_or(mask, Ordering::SeqCst) & mask != 0 {
            self.hints.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn signal(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.notifier.notify();
        }
    }

    fn take(&self, index: usize) -> u64 {
        let bits = self.pending[index].swap(0, Ordering::SeqCst);
        self.hints
            .fetch_sub(bits.count_ones() as usize, Ordering::SeqCst);
        bits
    }

    fn pending(&self) -> bool {
        self.writers.load(Ordering::SeqCst) != 0 || self.hints.load(Ordering::SeqCst) != 0
    }
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
    /// never existed, or its index lies outside this pool.
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
    /// Slot notification hints drained (coalesced wakes and cancels).
    pub notifications: u64,
    /// Reserved stale-entry count (bitmap hints always inspect the current slot).
    pub stale: u64,
    /// Polls taken.
    pub polls: u64,
}

/// A thread-safe handle to one task, for waking and cancelling it from another
/// thread or an interrupt context.
///
/// The handle is `Send + Sync` and its methods allocate nothing, take no locks
/// and run no task destructors: they set the slot's flag and publish a bitmap hint.
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
        self.notify(WAKE, || {});
    }

    /// Asks the executor to cancel the task (see
    /// [`Executor::cancel`](Executor::cancel) for the semantics). Cancelling an
    /// already settled task is dropped.
    pub fn cancel(&self) {
        self.notify(CANCEL, || {});
    }

    /// The flag word's current generation (0 for a never-spawned slot).
    fn generation(&self) -> u32 {
        ((self.flags.load(Ordering::SeqCst) & GEN_MASK) >> GEN_SHIFT) as u32
    }

    // The producer count precedes the flag, covering every publication gap.
    // `after_flag` is an empty inlined closure except in deterministic tests.
    fn notify(&self, flag: u64, after_flag: impl FnOnce()) {
        self.channel.writers.fetch_add(1, Ordering::SeqCst);
        let mut now = self.flags.load(Ordering::SeqCst);
        loop {
            if (now & GEN_MASK) >> GEN_SHIFT != u64::from(self.id.generation()) || now & flag != 0 {
                self.channel.writers.fetch_sub(1, Ordering::SeqCst);
                self.channel.signal();
                return;
            }
            match self.flags.compare_exchange_weak(
                now,
                now | flag,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => now = actual,
            }
        }
        after_flag();
        self.channel.push(self.id.index());
        self.channel.writers.fetch_sub(1, Ordering::SeqCst);
        self.channel.signal();
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
    /// asks whether it may sleep). This polling-only constructor has no worker
    /// notification; use [`with_notifier`](Self::with_notifier) for blocking workers.
    ///
    /// # Panics
    /// Panics when capacity or budget is zero.
    pub fn new(clock: Arc<dyn Clock>, config: Config) -> Self {
        Self::with_notifier(clock, config, Arc::new(PollOnly))
    }

    /// Creates a core with the binding's latched worker notification.
    ///
    /// Arm the scheduler wait before [`prepare_sleep`](Self::prepare_sleep).
    ///
    /// # Panics
    /// Panics when capacity or budget is zero.
    pub fn with_notifier(clock: Arc<dyn Clock>, config: Config, notifier: Arc<dyn Notify>) -> Self {
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
            .map(|index| Slot::free(index as u32 + 1))
            .collect();
        let free = (0..capacity).map(|index| index as u16).collect();
        Self {
            config,
            clock,
            slots,
            free,
            ready: VecDeque::with_capacity(capacity),
            sleepers: BinaryHeap::with_capacity(capacity),
            channel: Arc::new(Channel::new(capacity, notifier)),
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
        // A distinct flag identity prevents even a paused old waker's CAS from
        // affecting this occupancy. Allocation happens at spawn, never at wake.
        slot.flags = Arc::new(AtomicU64::new(flags(generation, false, false)));
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
        let slot = self.slots.get(id.index())?;
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
            slot.flags.fetch_and(!WAKE, Ordering::SeqCst);
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
                    slot.flags.store(0, Ordering::SeqCst);
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

    /// Consumes bitmap hints, with a flag scan for paused producers.
    fn drain(&mut self, outcome: &mut PollOutcome) {
        if self.channel.hints.load(Ordering::SeqCst) != 0 {
            // Snapshot each word once: repeated low-index wakes cannot starve
            // the other bits, and each drain applies at most capacity hints.
            for word in 0..self.channel.pending.len() {
                let mut bits = self.channel.take(word);
                while bits != 0 {
                    let bit = bits.trailing_zeros();
                    bits &= bits - 1;
                    self.stats.notifications += 1;
                    self.apply(word * 64 + bit as usize, outcome);
                }
            }
        }
        if self.channel.writers.load(Ordering::SeqCst) != 0 {
            for index in 0..self.slots.len() {
                self.apply(index, outcome);
            }
        }
    }

    fn apply(&mut self, index: usize, outcome: &mut PollOutcome) {
        let slot = &mut self.slots[index];
        let word = slot.flags.fetch_and(GEN_MASK, Ordering::SeqCst);
        if word & GEN_MASK == 0 {
            return;
        }
        let id = TaskId::new(index as u16, slot.generation);
        if word & CANCEL != 0 {
            self.settle_cancel(id);
        } else if word & WAKE != 0 && matches!(slot.state, State::Waiting | State::Sleeping) {
            let sleeping = slot.deadline.take().is_some();
            slot.state = State::Ready;
            if sleeping {
                self.sleepers.retain(|Reverse((_, task))| *task != id);
            }
            self.ready.push_back(id);
            outcome.woken += 1;
        }
    }

    fn settle_cancel(&mut self, id: TaskId) {
        let slot = &mut self.slots[id.index()];
        if !matches!(slot.state, State::Ready | State::Waiting | State::Sleeping) {
            return;
        }
        let sleeping = slot.deadline.take().is_some();
        slot.state = State::Cancelled;
        slot.flags.store(0, Ordering::SeqCst);
        slot.body.take();
        let detached = !slot.join_interest;
        self.stats.cancelled += 1;
        if sleeping {
            self.sleepers.retain(|Reverse((_, task))| *task != id);
        }
        if detached {
            self.reap(id);
        }
    }

    /// Cancels a live task at its next step boundary.
    ///
    /// # Errors
    /// Returns [`CancelError::InvalidId`] for stale or out-of-pool ids and
    /// [`CancelError::NotLive`] when the task has already settled.
    pub fn cancel(&mut self, id: TaskId) -> Result<(), CancelError> {
        let slot = self.slots.get(id.index()).ok_or(CancelError::InvalidId)?;
        if slot.generation != id.generation() || slot.state == State::Free {
            return Err(CancelError::InvalidId);
        }
        if !matches!(slot.state, State::Ready | State::Waiting | State::Sleeping) {
            return Err(CancelError::NotLive);
        }
        self.settle_cancel(id);
        Ok(())
    }

    /// Returns a completed or cancelled task's outcome and frees its slot, or
    /// `None` while it still runs (or is queued, waiting or sleeping). The task
    /// body is always dropped at completion, whether or not it is joined.
    pub fn join(&mut self, id: TaskId) -> Option<JoinOutcome> {
        let outcome = {
            let slot = self.slots.get_mut(id.index())?;
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
        let Some(slot) = self.slots.get_mut(id.index()) else {
            return;
        };
        if slot.generation != id.generation() {
            return;
        }
        slot.join_interest = false;
        if matches!(slot.state, State::Completed | State::Cancelled) {
            self.reap(id);
        }
    }

    /// Removes all storage owned by a settled task; exhausted ids never wrap.
    fn reap(&mut self, id: TaskId) {
        let slot = &mut self.slots[id.index()];
        if slot.state == State::Cancelled {
            self.ready.retain(|task| *task != id);
        }
        debug_assert!(matches!(slot.state, State::Completed | State::Cancelled));
        slot.state = State::Free;
        slot.deadline = None;
        slot.join_interest = false;
        slot.result = None;
        slot.body = None;
        slot.flags.store(0, Ordering::SeqCst);
        if slot.generation != MAX_GENERATION {
            slot.generation += 1;
            self.free.push_back(id.index() as u16);
        }
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
    /// use [`prepare_sleep`](Self::prepare_sleep) after arming its binding wait.
    pub fn schedulable(&self) -> bool {
        if !self.ready.is_empty() {
            return true;
        }
        if self.channel.pending() {
            return true;
        }
        match self.next_deadline() {
            Some(deadline) => self.clock.now() >= deadline,
            None => false,
        }
    }

    /// Arms the core's wake latch and rechecks pending work before blocking.
    ///
    /// The scheduler binding must first arm its latched notification wait and
    /// timer for [`next_deadline`](Self::next_deadline). If this returns false,
    /// disarm that wait and poll again. If true, block using that registration:
    /// a concurrent producer signals it even between this check and blocking.
    /// [`new`](Self::new) has no notifier and supports polling only; a sleeping
    /// worker must be constructed with [`with_notifier`](Self::with_notifier).
    pub fn prepare_sleep(&self) -> bool {
        self.channel.armed.store(true, Ordering::SeqCst);
        if self.schedulable() {
            self.channel.armed.store(false, Ordering::SeqCst);
            return false;
        }
        self.channel.armed.load(Ordering::SeqCst)
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
            slot.flags.store(0, Ordering::SeqCst);
            slot.body.take();
        }
    }
}

#[cfg(test)]
mod regressions {
    extern crate std;
    use super::*;
    use crate::FakeClock;
    use std::sync::mpsc;

    fn single() -> Executor {
        Executor::new(
            Arc::new(FakeClock::new()),
            Config {
                capacity: 1,
                budget: 1,
            },
        )
    }

    struct Signal(mpsc::Sender<()>);
    impl Notify for Signal {
        fn notify(&self) {
            self.0.send(()).unwrap();
        }
    }

    #[test]
    fn publication_gap_prevents_sleep_and_pending_wake_is_scanned() {
        let mut exec = single();
        let id = exec.spawn(|_| Step::Wait).unwrap();
        exec.poll();
        let waker = exec.waker(id).unwrap();
        let (published, observed) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            waker.notify(WAKE, || {
                published.send(()).unwrap();
                resumed.recv().unwrap();
            });
        });
        observed.recv().unwrap();
        assert!(exec.schedulable());
        assert!(!exec.prepare_sleep());
        assert_eq!(
            exec.poll().ran,
            1,
            "scan reaches the task before hint publication"
        );
        resume.send(()).unwrap();
        producer.join().unwrap();
        exec.poll();
    }

    #[test]
    fn armed_worker_is_signalled_after_the_final_empty_check() {
        let (signal, received) = mpsc::channel();
        let mut exec = Executor::with_notifier(
            Arc::new(FakeClock::new()),
            Config {
                capacity: 1,
                budget: 1,
            },
            Arc::new(Signal(signal)),
        );
        let id = exec.spawn(|_| Step::Wait).unwrap();
        exec.poll();
        // The receiver models a wait registration armed before prepare_sleep.
        assert!(exec.prepare_sleep());
        exec.waker(id).unwrap().wake();
        received.recv().unwrap();
        assert_eq!(exec.poll().ran, 1);
    }

    #[test]
    fn detached_cancellations_reuse_the_slot_from_either_path() {
        for remote in [false, true] {
            let mut exec = single();
            for _ in 0..100 {
                let id = exec
                    .spawn(|_| Step::WaitDeadline { deadline: u64::MAX })
                    .unwrap();
                let waker = exec.waker(id).unwrap();
                exec.poll();
                exec.drop_interest(id);
                if remote {
                    waker.cancel();
                    exec.poll();
                } else {
                    exec.cancel(id).unwrap();
                }
                assert!(exec.sleepers.is_empty());
                assert!(exec.ready.is_empty());
                assert!(waker.is_stale());
            }
        }
        let mut exec = single();
        let id = exec.spawn(|_| Step::Wait).unwrap();
        exec.cancel(id).unwrap();
        exec.drop_interest(id);
        assert!(exec.spawn(|_| Step::Wait).is_ok());
    }

    #[test]
    fn early_wakes_never_accumulate_stale_deadlines() {
        let mut exec = single();
        let id = exec
            .spawn(|_| Step::WaitDeadline { deadline: u64::MAX })
            .unwrap();
        let waker = exec.waker(id).unwrap();
        exec.poll();
        for _ in 0..1000 {
            waker.wake();
            exec.poll();
            assert_eq!(exec.sleepers.len(), 1);
        }
        exec.cancel(id).unwrap();
        assert_eq!(exec.next_deadline(), None);
        assert!(exec.sleepers.is_empty());
        exec.join(id);
        assert_eq!(exec.next_deadline(), None);
    }

    #[test]
    fn stale_wakers_do_not_revive_after_the_old_16_bit_wrap() {
        let mut exec = single();
        let first = exec.spawn(|_| Step::Wait).unwrap();
        let stale = exec.waker(first).unwrap();
        exec.cancel(first).unwrap();
        exec.join(first);
        for _ in 0..=u16::MAX {
            let id = exec.spawn(|_| Step::Wait).unwrap();
            exec.cancel(id).unwrap();
            exec.join(id);
        }
        let current = exec.spawn(|_| Step::Done { result: 7 }).unwrap();
        assert_ne!(first, current);
        stale.cancel();
        exec.poll();
        assert_eq!(exec.join(current), Some(JoinOutcome::Done(7)));
    }

    #[test]
    fn generation_exhaustion_retires_the_slot_instead_of_wrapping() {
        let mut exec = single();
        exec.slots[0].generation = MAX_GENERATION;
        let id = exec.spawn(|_| Step::Wait).unwrap();
        let old = exec.waker(id).unwrap();
        exec.cancel(id).unwrap();
        exec.join(id);
        assert!(old.is_stale());
        assert_eq!(exec.spawn(|_| Step::Wait), Err(Admission::PoolFull));
    }

    #[test]
    fn stalled_cancel_producer_cannot_overwrite_other_notifications() {
        let mut exec = Executor::new(
            Arc::new(FakeClock::new()),
            Config {
                capacity: 2,
                budget: 2,
            },
        );
        let a = exec.spawn(|_| Step::Wait).unwrap();
        let b = exec.spawn(|_| Step::Wait).unwrap();
        exec.poll();
        let cancel = exec.waker(a).unwrap();
        let repeated = cancel.clone();
        let wake = exec.waker(b).unwrap();
        let (published, observed) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            cancel.notify(CANCEL, || {
                published.send(()).unwrap();
                resumed.recv().unwrap();
            });
        });
        observed.recv().unwrap();
        for _ in 0..100 {
            repeated.cancel();
            wake.wake();
            assert_eq!(exec.poll().ran, 1, "other tasks still make progress");
        }
        assert_eq!(exec.join(a), Some(JoinOutcome::Cancelled));
        resume.send(()).unwrap();
        producer.join().unwrap();
        exec.poll();
        assert_eq!(exec.join(b), None);
        assert!(exec.channel.pending.len() <= usize::from(exec.config.capacity).div_ceil(64));
    }

    #[test]
    fn invalid_public_ids_are_rejected_without_panics() {
        let mut exec = single();
        for index in [1, u16::MAX] {
            let id = TaskId::new(index, 1);
            assert!(exec.waker(id).is_none());
            assert_eq!(exec.cancel(id), Err(CancelError::InvalidId));
            assert_eq!(exec.join(id), None);
            exec.drop_interest(id);
        }
    }

    #[test]
    fn a_single_self_wake_does_not_become_a_permanent_wake_bit() {
        let mut exec = single();
        let mut first = true;
        exec.spawn(move |cx| {
            if core::mem::take(&mut first) {
                cx.wake();
            }
            Step::Wait
        })
        .unwrap();
        assert_eq!(exec.poll().ran, 1);
        assert_eq!(exec.poll().ran, 1);
        assert_eq!(exec.poll().ran, 0);
    }
}
