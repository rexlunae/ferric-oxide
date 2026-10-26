//! Ferric Oxide: per-CPU I/O queue pairs and the threads that serve them (roadmap Phase 2.4,
//! PR 4; features.toml `drivers.nvme_percore`).
//!
//! At start, after the admin queue and the first I/O queue pair exist, [`start`]:
//!
//! 1. asks the controller for one more I/O queue pair per online CPU (`irq:cpus`) and takes as
//!    many as it grants and the MSI-X table has entries for;
//! 2. gives each pair an MSI-X entry with [`Msix::alloc_spread`], in two batches, and checks
//!    each vector's CPU against `irq:cpus` ([`ferric_irq::verify_spread`]);
//! 3. starts one worker thread per pair, pinned to its vector's CPU, with its own executor
//!    waiting on its own `irq:` handle.
//!
//! The disk scheme stays on the main thread. Under the default
//! [`QueuePolicy::Adaptive`](ferric_blkqueue::QueuePolicy), a read or write that arrives while
//! no other is in flight runs on the main thread's own queue pair, as with one I/O queue: a
//! hand-off costs two cross-CPU wake-ups and gains nothing without overlap. The others become
//! [`Job`]s, which a [`Dispatcher`] hands to the workers in turn (the scheme cannot see the
//! caller's CPU, so in this brokered mode requests are spread round-robin). The disk scheme's
//! set-policy call (`ferric-blkqueue`'s `abi`) can pin either path, which the same-boot
//! benchmark uses to compare them. A worker runs its jobs one at a time,
//! through its own bounce buffer and queue pair, and reports each finished job on a pipe that
//! the main thread's executor waits on; the job's future then completes. A job's buffer
//! belongs to the scheme's future until the job is settled, so a future dropped early waits
//! for the worker first.
//!
//! Under [`QueuePolicy::Workers`] the disk scheme serves reads and writes concurrently and
//! hands each one over whole, as a [`PendingIo`], through [`Dispatcher::submit`]. The worker
//! then runs it and writes the scheme's response itself, so the reply does not go back
//! through the main thread (no pipe report, no main-thread wake-up).
//!
//! A worker holds each job through a [`Claim`], which settles the job's [`JobSlot`] once:
//! done, or abandoned if the worker drops the job unfinished. When a worker thread ends
//! (returns or unwinds), its [`WorkerEnd`] marks it dead, drops the jobs still queued for it
//! (abandoning them), and reports [`WORKER_GONE`] on the pipe, which wakes every waiting
//! future; an abandoned job's future returns `EIO`. Later requests skip dead workers, and run
//! on the main thread's pair once none is left.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use driver_block::{PendingIo, Transfer};
use ferric_blkqueue::ids::{self, IdStream, WORKER_GONE};
use ferric_blkqueue::slot::{Claim, JobSlot, Outcome};
use ferric_blkqueue::{QueuePolicy, Route};
use ferric_irq::{CpuEntry, CpuId};
use pcid_interface::irq_helpers::{InterruptVector, Msix};
use pcid_interface::percpu;

use crate::nvme::executor::NvmeExecutor;
use crate::nvme::{Nvme, NvmeNamespace};

/// The first queue id the workers get; pair 1 is the main thread's.
const FIRST_WORKER_QUEUE: u16 = 2;
/// The most worker queue pairs, whatever the controller and the CPU count allow.
const MAX_WORKERS: usize = 64;

/// What a job does, with the scheme's buffer for a copy through the bounce buffer.
#[derive(Clone, Copy)]
pub enum Op {
    /// Read into `len` bytes at `buf`.
    Read { buf: *mut u8, len: usize },
    /// Write `len` bytes from `buf`.
    Write { buf: *const u8, len: usize },
    /// DMA `sectors` blocks to physical address `addr`.
    ReadDma { addr: u64, sectors: u64 },
    /// DMA `sectors` blocks from physical address `addr`.
    WriteDma { addr: u64, sectors: u64 },
}

/// A job's result slot.
type Slot = JobSlot<syscall::Result<usize>>;

/// One read or write for a worker.
struct Job {
    id: u64,
    namespace: NvmeNamespace,
    lba: u64,
    op: Op,
    claim: Claim<syscall::Result<usize>>,
}

// SAFETY: the pointers in `op` stay valid until the job is settled: the scheme future that
// made the job does not return or drop before its slot is settled, done or abandoned (see
// `JobFuture`'s `Drop`), the claim settles it only once the worker is done with the buffer,
// and the worker is the only one touching the buffer meanwhile.
unsafe impl Send for Job {}

/// The main thread's handle on one worker.
struct Worker {
    jobs: mpsc::Sender<Work>,
    /// The write end of the pipe the worker's executor waits on.
    wake: File,
    cpu: CpuId,
    /// How many times the worker's watchdog has ticked: still 0, or not rising, while a job
    /// waits means the worker thread is not running at all.
    ticks: Arc<AtomicU64>,
    /// Cleared by the worker's [`WorkerEnd`] as its thread ends, or here when a job cannot be
    /// sent to it.
    alive: Arc<AtomicBool>,
}

impl Worker {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

/// When a job was handed out, the index of its worker, and its result slot.
type Started = (Instant, usize, Arc<Slot>);

/// Hands jobs to the workers and completes their futures; lives on the main thread.
pub struct Dispatcher {
    workers: Vec<Worker>,
    next_worker: Cell<usize>,
    next_id: Cell<u64>,
    /// The read end of the pipe the workers report finished jobs on (non-blocking).
    done: File,
    /// The ids read from `done` so far, with any part of one that a read split.
    reports: RefCell<IdStream>,
    waiting: RefCell<HashMap<u64, Waker>>,
    /// When each unfinished job was handed out, to which worker, and its result slot, for the
    /// watchdog.
    started: RefCell<HashMap<u64, Started>>,
    /// How requests are placed.
    policy: Cell<QueuePolicy>,
    /// Requests in flight on any queue pair, the main thread's included.
    in_flight: Rc<Cell<usize>>,
    /// Held by the request running on the main thread's queue pair: its thread context (and
    /// bounce buffer) serves one request at a time, and its lock is reentrant, so concurrent
    /// requests on the scheme's thread must queue here instead.
    main_queue: futures::lock::Mutex<()>,
}

/// A request running inline on the main thread's queue pair: counted in flight, and holding
/// the main queue, until it is dropped.
pub struct InlineGuard<'a> {
    _counted: InFlightGuard,
    _main: futures::lock::MutexGuard<'a, ()>,
}

/// One request counted in [`Dispatcher`]'s in-flight total until it is dropped.
pub struct InFlightGuard(Rc<Cell<usize>>);

impl InFlightGuard {
    fn new(count: &Rc<Cell<usize>>) -> Self {
        count.set(count.get() + 1);
        Self(Rc::clone(count))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl Dispatcher {
    /// Sets how requests are placed from now on.
    pub fn set_policy(&self, policy: QueuePolicy) {
        log::info!("nvmed: queue policy {policy:?}");
        self.policy.set(policy);
    }

    /// Whether any request is in flight on any queue pair: the main thread's inline one, or
    /// a job a worker is running. The Adaptive overlap option (`drivers.nvme_overlap`) uses
    /// this to decide whether a request that arrives now may be handed to a worker.
    #[cfg(feature = "overlap")]
    pub fn busy(&self) -> bool {
        self.in_flight.get() > 0
    }

    /// How requests are placed now.
    pub fn policy(&self) -> QueuePolicy {
        self.policy.get()
    }

    /// Claims a new request for the main thread's queue pair when the policy routes it there,
    /// waiting for the request already on it (only `MainQueue` sends one while another runs):
    /// the caller runs it inline while holding the guard. `None` means it goes to a worker
    /// through [`run`](Self::run).
    pub async fn claim_inline(&self) -> Option<InlineGuard<'_>> {
        let route = match self.policy.get().route(self.in_flight.get()) {
            Route::Worker if !self.workers.iter().any(Worker::is_alive) => Route::Main,
            route => route,
        };
        match route {
            Route::Main => {
                // Counted before waiting, so that requests arriving meanwhile see this one.
                let counted = InFlightGuard::new(&self.in_flight);
                let main = self.main_queue.lock().await;
                Some(InlineGuard {
                    _counted: counted,
                    _main: main,
                })
            }
            Route::Worker => None,
        }
    }

    /// The index of the next live worker in turn, or `None` when every worker has ended.
    fn next_live_worker(&self) -> Option<usize> {
        let count = self.workers.len();
        let start = self.next_worker.get();
        let index = (0..count)
            .map(|step| (start + step) % count)
            .find(|&index| self.workers[index].is_alive())?;
        self.next_worker.set((index + 1) % count);
        Some(index)
    }

    /// Runs `op` on the next worker and returns its result.
    ///
    /// # Safety
    ///
    /// For [`Op::Read`] and [`Op::Write`], the buffer must stay valid, and not be used by
    /// anything else, until the returned future completes or is dropped.
    pub async unsafe fn run(
        self: &Rc<Self>,
        namespace: NvmeNamespace,
        lba: u64,
        op: Op,
    ) -> syscall::Result<usize> {
        let _counted = InFlightGuard::new(&self.in_flight);
        let id = self.next_id.get();
        self.next_id.set(ids::next_id(id));
        let Some(index) = self.next_live_worker() else {
            log::error!("nvmed: every worker has ended");
            return Err(syscall::Error::new(syscall::EIO));
        };
        let worker = &self.workers[index];
        let (slot, claim) = JobSlot::new();
        let job = Job {
            id,
            namespace,
            lba,
            op,
            claim,
        };
        if worker.jobs.send(Work::Job(job)).is_err() {
            log::error!("nvmed: the worker on CPU {} is gone", worker.cpu.get());
            worker.alive.store(false, Ordering::Release);
            // The job came back undelivered and is dropped: nothing touches the buffer.
            return Err(syscall::Error::new(syscall::EIO));
        }
        if let Err(err) = (&worker.wake).write_all(&[1]) {
            // The job is queued and the worker may still run it, so its future must be awaited
            // for the buffer's sake; the worker finds it with the next job's wake-up.
            log::error!(
                "nvmed: cannot wake the worker on CPU {}: {err}",
                worker.cpu.get()
            );
        }
        self.started
            .borrow_mut()
            .insert(id, (Instant::now(), index, Arc::clone(&slot)));
        let result = JobFuture {
            dispatcher: Rc::clone(self),
            id,
            slot,
        }
        .await;
        self.started.borrow_mut().remove(&id);
        result
    }

    /// Hands `io` to the next live worker, which runs it on its queue pair and writes the
    /// scheme's response itself.
    ///
    /// # Errors
    ///
    /// Gives `io` back, untouched, when every worker has ended; the scheme then serves it on
    /// its own thread.
    pub fn submit(&self, namespace: NvmeNamespace, io: PendingIo) -> Result<(), PendingIo> {
        let mut work = Work::Answer { namespace, io };
        while let Some(index) = self.next_live_worker() {
            let worker = &self.workers[index];
            match worker.jobs.send(work) {
                Ok(()) => {
                    if let Err(err) = (&worker.wake).write_all(&[1]) {
                        // Queued all the same: the next wake-up finds it, and the worker's
                        // end answers it if none comes.
                        log::error!(
                            "nvmed: cannot wake the worker on CPU {}: {err}",
                            worker.cpu.get()
                        );
                    }
                    return Ok(());
                }
                Err(mpsc::SendError(back)) => {
                    log::error!("nvmed: the worker on CPU {} is gone", worker.cpu.get());
                    worker.alive.store(false, Ordering::Release);
                    work = back;
                }
            }
        }
        match work {
            Work::Answer { io, .. } => Err(io),
            // Only answers are sent from here.
            Work::Job(_) => Ok(()),
        }
    }

    /// Wakes the futures of the jobs the workers settled, and returns how many it woke.
    fn drain(&self) -> usize {
        let mut bytes = [0_u8; 8 * 32];
        let mut woken = 0;
        loop {
            match (&self.done).read(&mut bytes) {
                Ok(0) => return woken,
                Ok(len) => self
                    .reports
                    .borrow_mut()
                    .feed(&bytes[..len], |id| woken += self.report(id)),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return woken,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    log::error!("nvmed: cannot read finished jobs: {err}");
                    return woken;
                }
            }
        }
    }

    /// Wakes the future waiting for job `id`, or every waiting future when a worker reports
    /// that it ended (its unfinished jobs are abandoned); returns how many it woke.
    fn report(&self, id: u64) -> usize {
        let mut waiting = self.waiting.borrow_mut();
        if id == WORKER_GONE {
            let all: Vec<Waker> = waiting.drain().map(|(_, waker)| waker).collect();
            drop(waiting);
            let woken = all.len();
            all.into_iter().for_each(Waker::wake);
            return woken;
        }
        match waiting.remove(&id) {
            Some(waker) => {
                drop(waiting);
                waker.wake();
                1
            }
            None => 0,
        }
    }

    /// Ferric Oxide watchdog: once a second, completes the jobs whose report the pipe's event
    /// did not deliver, and says so (a few times), so a lost wake-up shows in the log as a
    /// stall instead of a hang.
    pub async fn watch(self: Rc<Self>, executor: Rc<NvmeExecutor>) {
        let Some(mut ticks) = Ticks::new(&executor) else {
            return;
        };
        let mut told = 0;
        // Jobs the workers had finished at the previous tick that the pump had not completed:
        // the pipe's event may simply not have been handled yet when a tick comes, so a job
        // counts as unreported only once it is still waiting a whole tick later.
        let mut finished_before: HashSet<u64> = HashSet::new();
        loop {
            ticks.next().await;
            let finished: HashSet<u64> = self
                .started
                .borrow()
                .iter()
                .filter(|(_, (_, _, slot))| slot.is_done())
                .map(|(&id, _)| id)
                .collect();
            let unreported = finished.intersection(&finished_before).count();
            finished_before = finished;
            if unreported != 0 {
                self.drain();
                finished_before.clear();
                if told < STALL_REPORTS {
                    told += 1;
                    log::warn!(
                        "nvmed: watchdog: {unreported} job(s) finished a second ago, but the completion pipe's event did not wake the main thread"
                    );
                }
            }
            if told >= STALL_REPORTS {
                continue;
            }
            let stalled = self
                .started
                .borrow()
                .iter()
                .filter(|(_, (_, _, slot))| !slot.is_settled())
                .map(|(&id, (since, index, _))| (id, since.elapsed(), *index))
                .find(|&(_, waited, _)| waited >= Duration::from_secs(1));
            if let Some((id, waited, index)) = stalled {
                told += 1;
                let worker = &self.workers[index];
                log::warn!(
                    "nvmed: watchdog: job {id} has waited {} ms for the worker on CPU {}, whose watchdog has ticked {} time(s)",
                    waited.as_millis(),
                    worker.cpu.get(),
                    worker.ticks.load(Ordering::Relaxed)
                );
            }
        }
    }

    /// Completes job futures as the workers finish them; runs for the driver's lifetime on the
    /// main thread's executor.
    pub async fn pump(self: Rc<Self>, executor: Rc<NvmeExecutor>) {
        let mut events = Box::pin(
            executor
                .register_external_event(self.done.as_raw_fd() as usize, event::EventFlags::READ),
        );
        loop {
            self.drain();
            let _ = events.as_mut().next().await;
        }
    }
}

/// Waits for a job's result.
struct JobFuture {
    dispatcher: Rc<Dispatcher>,
    id: u64,
    slot: Arc<Slot>,
}

impl Future for JobFuture {
    type Output = syscall::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.slot.take() {
            Outcome::Done(Some(result)) => Poll::Ready(result),
            // Polled again after its result was taken, or the worker ended without running it.
            Outcome::Done(None) | Outcome::Abandoned => {
                Poll::Ready(Err(syscall::Error::new(syscall::EIO)))
            }
            Outcome::Pending => {
                // The worker settles the slot before it reports the job (or its own end) on
                // the pipe, and the pipe is read on this thread, so the waker is in place
                // before that report is handled.
                self.dispatcher
                    .waiting
                    .borrow_mut()
                    .insert(self.id, cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for JobFuture {
    fn drop(&mut self) {
        self.dispatcher.waiting.borrow_mut().remove(&self.id);
        // The worker may still be using the buffer: it is the caller's again only once the job
        // is settled, which a worker that ends does for every job it holds.
        while !self.slot.is_settled() {
            std::thread::yield_now();
        }
    }
}

/// How many times each watchdog reports a stall.
const STALL_REPORTS: u32 = 5;

/// A once-a-second tick from `/scheme/time`, as an event on an executor.
struct Ticks {
    time: File,
    events: Pin<Box<executor::ExternalEventHandle<crate::nvme::executor::NvmeHw>>>,
}

impl Ticks {
    /// A tick source on `executor`, or `None` (logged) when the clock cannot be opened.
    fn new(executor: &NvmeExecutor) -> Option<Self> {
        let path = format!("/scheme/time/{}", libredox::flag::CLOCK_MONOTONIC);
        let time = match File::open(&path) {
            Ok(time) => time,
            Err(err) => {
                log::warn!("nvmed: no watchdog: cannot open {path}: {err}");
                return None;
            }
        };
        let events = Box::pin(
            executor.register_external_event(time.as_raw_fd() as usize, event::EventFlags::READ),
        );
        Some(Self { time, events })
    }

    /// Waits for the next tick, a second from now.
    async fn next(&mut self) {
        let mut now = syscall::TimeSpec::default();
        let armed = (&self.time).read(&mut now).is_ok() && {
            now.tv_sec += 1;
            (&self.time).write(&now).is_ok()
        };
        if armed {
            let _ = self.events.as_mut().next().await;
        } else {
            log::warn!("nvmed: watchdog clock cannot be armed");
            std::future::pending::<()>().await;
        }
    }
}

/// The job a worker is running and when it started, for its watchdog.
type InFlight = Rc<Cell<Option<(u64, Instant)>>>;

/// Ferric Oxide watchdog for a worker: once a second, reports (a few times) a job that has
/// waited a second or more, and whether its completion queue holds an entry nothing took
/// (a completion interrupt that did not wake the worker) or not (the controller has not
/// answered). The tick also wakes the executor, which then polls the queue.
async fn watch_worker(
    nvme: Arc<Nvme>,
    qid: u16,
    in_flight: InFlight,
    alive: Arc<AtomicU64>,
    executor: Rc<NvmeExecutor>,
) {
    let Some(mut ticks) = Ticks::new(&executor) else {
        return;
    };
    let mut told = 0;
    loop {
        ticks.next().await;
        alive.fetch_add(1, Ordering::Relaxed);
        let Some((id, since)) = in_flight.get() else {
            continue;
        };
        let waited = since.elapsed();
        if waited >= Duration::from_secs(1) && told < STALL_REPORTS {
            told += 1;
            let entry = if nvme.completion_waiting(qid) {
                "holds an entry the worker was not woken for"
            } else {
                "is empty"
            };
            // A request the worker answers itself has no job id (see `Work::Answer`).
            let what = if id == WORKER_GONE {
                "a direct request".to_owned()
            } else {
                format!("job {id}")
            };
            log::warn!(
                "nvmed: watchdog: queue pair {qid} {what} has waited {} ms; its completion queue {entry}",
                waited.as_millis()
            );
        }
    }
}

/// A pipe: the read end, non-blocking when `nonblock_read`, and the write end.
fn pipe(nonblock_read: bool) -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 just made these descriptors, and nothing else owns them.
    let (read, write) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
    if nonblock_read {
        // SAFETY: `read` is an open descriptor.
        let flags = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0
            || unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

/// How many worker queue pairs to make: one per CPU, no more than the controller grants
/// beyond the main thread's pair, the MSI-X table has free entries, or [`MAX_WORKERS`].
fn worker_count(cpus: usize, granted_pairs: u16, free_entries: u16) -> usize {
    cpus.min(usize::from(granted_pairs.saturating_sub(1)))
        .min(usize::from(free_entries))
        .min(MAX_WORKERS)
}

/// What the controller granted for per-CPU queue pairs, which [`request`] asks for before the
/// first I/O queue exists and [`start`] then uses.
pub struct Grant {
    /// The CPUs, from `irq:cpus`.
    cpus: Vec<CpuEntry>,
    /// I/O queue pairs the controller grants, the main thread's pair included.
    granted: u16,
}

/// Asks the controller for one I/O queue pair per CPU beyond the main thread's, or returns
/// `None` (and logs why) to keep every request on the main thread's pair.
///
/// It must run before [`Nvme::init_with_queues`] creates I/O queue pair 1: the NVMe Base
/// Specification allows Set Features, Number of Queues (feature 07h) only during
/// initialization, before any I/O queue is created, and a controller may fail it afterwards
/// (QEMU answers Command Sequence Error).
pub fn request(nvme: &Arc<Nvme>, msix: &Msix, executor: &Rc<NvmeExecutor>) -> Option<Grant> {
    let Some(cpus) = msix.cpus().map(<[CpuEntry]>::to_vec) else {
        log::info!("nvmed: no irq:cpus (kernel.irq_percpu off): one I/O queue pair");
        return None;
    };
    let wanted = u16::try_from(cpus.len().min(MAX_WORKERS) + 1).unwrap_or(u16::MAX);
    match executor.block_on(nvme.request_io_queues(wanted)) {
        Ok(granted) => Some(Grant { cpus, granted }),
        Err(err) => {
            log::warn!("nvmed: one I/O queue pair: the controller refused more ({err})");
            None
        }
    }
}

/// Sets up the per-CPU queue pairs that `grant` allows and their workers, or returns `None`
/// (and logs why) to keep every request on the main thread's queue pair.
pub fn start(
    nvme: &Arc<Nvme>,
    msix: &mut Msix,
    executor: &Rc<NvmeExecutor>,
    grant: Grant,
) -> Option<Rc<Dispatcher>> {
    let Grant { cpus, granted } = grant;
    let free = msix.table_size().saturating_sub(1);
    let count = worker_count(cpus.len(), granted, free);
    if count == 0 {
        log::info!(
            "nvmed: one I/O queue pair (controller grants {granted}, {free} free MSI-X entries)"
        );
        return None;
    }
    let vectors = spread_vectors(msix, &cpus, count)?;
    let (done, done_tx) = match pipe(true) {
        Ok(pipe) => pipe,
        Err(err) => {
            log::error!("nvmed: cannot make the completion pipe: {err}");
            return None;
        }
    };
    let mut workers = Vec::with_capacity(count);
    for (i, vector) in vectors.into_iter().enumerate() {
        let qid = FIRST_WORKER_QUEUE + i as u16;
        match spawn_worker(nvme, executor, qid, vector, &done_tx) {
            Ok(worker) => workers.push(worker),
            Err(err) => {
                log::error!("nvmed: no worker for I/O queue pair {qid}: {err}");
                break;
            }
        }
    }
    if workers.is_empty() {
        return None;
    }
    let on: Vec<u32> = workers.iter().map(|w| w.cpu.get()).collect();
    log::info!(
        "nvmed: {} I/O queue pairs (controller grants {granted}): pair 1 on the main thread, {} per-CPU, workers on CPUs {on:?}",
        workers.len() + 1,
        workers.len()
    );
    Some(Rc::new(Dispatcher {
        workers,
        next_worker: Cell::new(0),
        policy: Cell::new(QueuePolicy::default()),
        in_flight: Rc::new(Cell::new(0)),
        main_queue: futures::lock::Mutex::new(()),
        next_id: Cell::new(0),
        done,
        reports: RefCell::new(IdStream::new()),
        waiting: RefCell::new(HashMap::new()),
        started: RefCell::new(HashMap::new()),
    }))
}

/// Takes `count` MSI-X entries with [`Msix::alloc_spread`] in two batches and checks where
/// they went (the on-target test of the allocator); `None` when that fails.
fn spread_vectors(
    msix: &mut Msix,
    cpus: &[CpuEntry],
    count: usize,
) -> Option<Vec<InterruptVector>> {
    let first_entry = 1;
    let first = count.div_ceil(2);
    let mut vectors = Vec::with_capacity(count);
    for batch in [first, count - first] {
        if batch == 0 {
            continue;
        }
        match msix.alloc_spread(batch as u16, None) {
            Ok(more) => vectors.extend(more),
            Err(err) => {
                log::error!(
                    "nvmed: one I/O queue pair: no MSI-X vectors: {}",
                    report(&err)
                );
                return None;
            }
        }
    }
    let placed: Vec<CpuId> = vectors.iter().map(InterruptVector::cpu).collect();
    match ferric_irq::verify_spread(cpus, None, first_entry, &placed) {
        Ok(()) => {
            let on: Vec<u32> = placed.iter().map(|cpu| cpu.get()).collect();
            log::info!(
                "nvmed: MSI-X entries {first_entry}..={} spread over CPUs {on:?} in batches of {first} and {}, as irq:cpus and ferric_irq::spread give",
                first_entry + count - 1,
                count - first
            );
        }
        Err(err) => log::error!("nvmed: MSI-X spread check failed: {err}"),
    }
    Some(vectors)
}

/// `err` and its sources, for a log line.
fn report(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// Creates queue pair `qid` on `vector` and starts the thread that serves it.
fn spawn_worker(
    nvme: &Arc<Nvme>,
    executor: &Rc<NvmeExecutor>,
    qid: u16,
    vector: InterruptVector,
    done_tx: &File,
) -> io::Result<Worker> {
    let cpu = vector.cpu();
    let iv = vector.vector();
    let irq = vector.irq_handle().try_clone()?;
    let done_tx = done_tx.try_clone()?;
    let (wake_rx, wake) = pipe(true)?;
    executor
        .block_on(nvme.add_queue_pair(qid, vector))
        .map_err(|err| io::Error::from_raw_os_error(err.errno))?;
    let (jobs, rx) = mpsc::channel();
    let ticks = Arc::new(AtomicU64::new(0));
    let alive = Arc::new(AtomicBool::new(true));
    let nvme = Arc::clone(nvme);
    let end = WorkerEnd {
        qid,
        jobs: Some(rx),
        done_tx,
        alive: Arc::clone(&alive),
    };
    std::thread::Builder::new()
        .name(format!("nvmed-q{qid}"))
        .spawn({
            let ticks = Arc::clone(&ticks);
            move || serve(nvme, qid, iv, cpu, irq, end, wake_rx, ticks)
        })?;
    Ok(Worker {
        jobs,
        wake,
        cpu,
        ticks,
        alive,
    })
}

/// What a worker finds in its queue.
enum Next {
    /// A job to run.
    Job(Work),
    /// Nothing queued: wait for a wake-up.
    Idle,
    /// The dispatcher is gone: end the thread.
    Closed,
}

/// A worker thread's job queue and report pipe, which say the worker has ended when they drop
/// (as the thread returns or unwinds): the worker is marked dead, the jobs still queued are
/// dropped, which abandons them, and [`WORKER_GONE`] wakes every waiting future to see how its
/// job settled.
struct WorkerEnd {
    qid: u16,
    /// `Some` until the drop.
    jobs: Option<mpsc::Receiver<Work>>,
    done_tx: File,
    alive: Arc<AtomicBool>,
}

impl WorkerEnd {
    /// The next queued job, if any, or whether the dispatcher is gone.
    fn next_job(&self) -> Next {
        match self.jobs.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(job)) => Next::Job(job),
            Some(Err(mpsc::TryRecvError::Empty)) => Next::Idle,
            Some(Err(mpsc::TryRecvError::Disconnected)) | None => Next::Closed,
        }
    }

    /// Reports job `id` as settled.
    fn report(&mut self, id: u64) {
        if self.done_tx.write_all(&id.to_ne_bytes()).is_err() {
            log::error!(
                "nvmed: queue pair {} worker cannot report job {id}",
                self.qid
            );
        }
    }
}

impl Drop for WorkerEnd {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        // Dropping the receiver drops the queued jobs, and with them their claims, so every
        // job this worker held is settled before the report below.
        self.jobs = None;
        if std::thread::panicking() {
            log::error!("nvmed: queue pair {} worker panicked", self.qid);
        }
        self.report(WORKER_GONE);
    }
}

/// A worker thread: pins itself to `cpu`, then runs the jobs it is sent on queue pair `qid`.
#[allow(clippy::too_many_arguments)]
fn serve(
    nvme: Arc<Nvme>,
    qid: u16,
    iv: u16,
    cpu: CpuId,
    irq: File,
    mut end: WorkerEnd,
    wake_rx: File,
    ticks: Arc<AtomicU64>,
) {
    if let Err(err) = percpu::pin_current_thread(cpu) {
        log::warn!(
            "nvmed: queue pair {qid} worker not pinned to CPU {}: {}",
            cpu.get(),
            report(&err)
        );
    }
    nvme.enter_queue_pair(iv);
    let executor = crate::nvme::executor::init(Arc::clone(&nvme), iv, false, irq);
    let mut wake = Box::pin(
        executor.register_external_event(wake_rx.as_raw_fd() as usize, event::EventFlags::READ),
    );
    let in_flight: InFlight = Rc::new(Cell::new(None));
    executor.spawn(watch_worker(
        Arc::clone(&nvme),
        qid,
        Rc::clone(&in_flight),
        ticks,
        Rc::clone(&executor),
    ));
    executor.block_on(async {
        loop {
            // Drain the wake-ups before looking for jobs, so one sent after the look still
            // has its byte waiting.
            let mut bytes = [0_u8; 64];
            while matches!((&wake_rx).read(&mut bytes), Ok(n) if n > 0) {}
            loop {
                match end.next_job() {
                    Next::Job(Work::Job(job)) => {
                        let id = job.id;
                        in_flight.set(Some((id, Instant::now())));
                        run(&nvme, job).await;
                        in_flight.set(None);
                        end.report(id);
                    }
                    Next::Job(Work::Answer { namespace, io }) => {
                        in_flight.set(Some((WORKER_GONE, Instant::now())));
                        answer(&nvme, namespace, io).await;
                        in_flight.set(None);
                    }
                    Next::Idle => break,
                    Next::Closed => return,
                }
            }
            let _ = wake.as_mut().next().await;
        }
    });
}

/// What a worker is sent.
enum Work {
    /// A job whose result goes back to the main thread's future.
    Job(Job),
    /// A scheme request the worker answers itself.
    Answer {
        namespace: NvmeNamespace,
        io: PendingIo,
    },
}

/// Runs `io` on this thread's queue pair and writes the scheme's response.
async fn answer(nvme: &Nvme, namespace: NvmeNamespace, mut io: PendingIo) {
    let lba = io.block();
    let result = match io.transfer() {
        Transfer::Read(buf) => nvme.namespace_read(&namespace, lba, buf).await,
        Transfer::Write(buf) => nvme.namespace_write(&namespace, lba, buf).await,
    };
    io.finish(result);
}

/// Runs one job on this thread's queue pair and stores its result.
async fn run(nvme: &Nvme, job: Job) {
    let Job {
        namespace,
        lba,
        op,
        claim,
        ..
    } = job;
    let result = match op {
        Op::Read { buf, len } => {
            // SAFETY: see `Job`'s `Send`: the buffer is this job's until the claim settles.
            let buf = unsafe { std::slice::from_raw_parts_mut(buf, len) };
            nvme.namespace_read(&namespace, lba, buf).await
        }
        Op::Write { buf, len } => {
            // SAFETY: as above.
            let buf = unsafe { std::slice::from_raw_parts(buf, len) };
            nvme.namespace_write(&namespace, lba, buf).await
        }
        Op::ReadDma { addr, sectors } => nvme
            .namespace_read_zerocopy(&namespace, lba, addr, sectors)
            .await
            .map(|()| 0),
        Op::WriteDma { addr, sectors } => nvme
            .namespace_write_zerocopy(&namespace, lba, addr, sectors)
            .await
            .map(|()| 0),
    };
    claim.complete(result);
}
