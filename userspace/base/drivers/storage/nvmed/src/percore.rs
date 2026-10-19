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
//! The disk scheme stays on the main thread. Its reads and writes become [`Job`]s, which a
//! [`Dispatcher`] hands to the workers in turn (the scheme cannot see the caller's CPU, so in
//! this brokered mode requests are spread round-robin). A worker runs its jobs one at a time,
//! through its own bounce buffer and queue pair, and reports each finished job on a pipe that
//! the main thread's executor waits on; the job's future then completes. A job's buffer
//! belongs to the scheme's future until the job is done, so a future dropped early waits for
//! the worker first.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::task::{Context, Poll, Waker};

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

/// A finished job's result, written by the worker.
struct Slot {
    result: Mutex<Option<syscall::Result<usize>>>,
    done: AtomicBool,
}

/// One read or write for a worker.
struct Job {
    id: u64,
    namespace: NvmeNamespace,
    lba: u64,
    op: Op,
    slot: Arc<Slot>,
}

// SAFETY: the pointers in `op` stay valid until the job is done: the scheme future that made
// the job does not return or drop before `slot.done` is set (see `JobFuture`'s `Drop`), and
// the worker is the only one touching the buffer meanwhile.
unsafe impl Send for Job {}

/// The main thread's handle on one worker.
struct Worker {
    jobs: mpsc::Sender<Job>,
    /// The write end of the pipe the worker's executor waits on.
    wake: File,
    cpu: CpuId,
}

/// Hands jobs to the workers and completes their futures; lives on the main thread.
pub struct Dispatcher {
    workers: Vec<Worker>,
    next_worker: Cell<usize>,
    next_id: Cell<u64>,
    /// The read end of the pipe the workers report finished jobs on (non-blocking).
    done: File,
    waiting: RefCell<HashMap<u64, Waker>>,
}

impl Dispatcher {
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
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        let slot = Arc::new(Slot {
            result: Mutex::new(None),
            done: AtomicBool::new(false),
        });
        let index = self.next_worker.get();
        self.next_worker.set((index + 1) % self.workers.len());
        let worker = &self.workers[index];
        let job = Job {
            id,
            namespace,
            lba,
            op,
            slot: Arc::clone(&slot),
        };
        if worker.jobs.send(job).is_err() {
            log::error!("nvmed: the worker on CPU {} is gone", worker.cpu.get());
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
        JobFuture {
            dispatcher: Rc::clone(self),
            id,
            slot,
        }
        .await
    }

    /// Wakes the futures of the jobs the workers finished.
    fn drain(&self) {
        let mut ids = [0_u8; 8 * 32];
        loop {
            match (&self.done).read(&mut ids) {
                Ok(0) => return,
                Ok(len) => {
                    for id in ids[..len - len % 8].chunks_exact(8) {
                        let id = u64::from_ne_bytes(id.try_into().expect("8-byte chunk"));
                        if let Some(waker) = self.waiting.borrow_mut().remove(&id) {
                            waker.wake();
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    log::error!("nvmed: cannot read finished jobs: {err}");
                    return;
                }
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
        if self.slot.done.load(Ordering::Acquire) || self.worker_gone() {
            let result = self.slot.result.lock().map(|mut r| r.take());
            return Poll::Ready(match result {
                Ok(Some(result)) => result,
                _ => Err(syscall::Error::new(syscall::EIO)),
            });
        }
        self.dispatcher
            .waiting
            .borrow_mut()
            .insert(self.id, cx.waker().clone());
        Poll::Pending
    }
}

impl JobFuture {
    /// Whether the worker dropped the job without finishing it (its thread panicked): it no
    /// longer touches the buffer, and no result is coming.
    fn worker_gone(&self) -> bool {
        Arc::strong_count(&self.slot) == 1
    }
}

impl Drop for JobFuture {
    fn drop(&mut self) {
        self.dispatcher.waiting.borrow_mut().remove(&self.id);
        // The worker may still be using the buffer: it is the caller's again only once the job
        // is done.
        while !self.slot.done.load(Ordering::Acquire) && !self.worker_gone() {
            std::thread::yield_now();
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
        "nvmed: {} per-CPU I/O queue pairs (controller grants {granted} pairs), workers on CPUs {on:?}",
        workers.len()
    );
    Some(Rc::new(Dispatcher {
        workers,
        next_worker: Cell::new(0),
        next_id: Cell::new(0),
        done,
        waiting: RefCell::new(HashMap::new()),
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
    let nvme = Arc::clone(nvme);
    std::thread::Builder::new()
        .name(format!("nvmed-q{qid}"))
        .spawn(move || serve(nvme, qid, iv, cpu, irq, rx, wake_rx, done_tx))?;
    Ok(Worker { jobs, wake, cpu })
}

/// A worker thread: pins itself to `cpu`, then runs the jobs it is sent on queue pair `qid`.
#[allow(clippy::too_many_arguments)]
fn serve(
    nvme: Arc<Nvme>,
    qid: u16,
    iv: u16,
    cpu: CpuId,
    irq: File,
    jobs: mpsc::Receiver<Job>,
    wake_rx: File,
    mut done_tx: File,
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
    executor.block_on(async {
        loop {
            // Drain the wake-ups before looking for jobs, so one sent after the look still
            // has its byte waiting.
            let mut bytes = [0_u8; 64];
            while matches!((&wake_rx).read(&mut bytes), Ok(n) if n > 0) {}
            loop {
                match jobs.try_recv() {
                    Ok(job) => {
                        let id = job.id;
                        run(&nvme, job).await;
                        if done_tx.write_all(&id.to_ne_bytes()).is_err() {
                            log::error!("nvmed: queue pair {qid} worker cannot report job {id}");
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let _ = wake.as_mut().next().await;
        }
    });
}

/// Runs one job on this thread's queue pair and stores its result.
async fn run(nvme: &Nvme, job: Job) {
    let Job {
        namespace,
        lba,
        op,
        slot,
        ..
    } = job;
    let result = match op {
        Op::Read { buf, len } => {
            // SAFETY: see `Job`'s `Send`: the buffer is this job's until `done` is set.
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
    if let Ok(mut stored) = slot.result.lock() {
        *stored = Some(result);
    }
    slot.done.store(true, Ordering::Release);
}
