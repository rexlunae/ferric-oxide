//! The ring's worker: a kernel context that runs a ring's asynchronous chains (`SqeFlags::ASYNC`
//! on a chain's first SQE) after the `Enter` that took them has returned.
//!
//! - [`queue`] runs in the submitter's context, inside `Enter`. It looks up every file the
//!   chain names (the caller's descriptors and the ring's slots), records the submitter's
//!   address space (weakly) and credentials, and queues the chain as a [`Job`]; room for the
//!   chain's completions was already set aside in the CQ. The first job starts the worker.
//! - The worker ([`worker_main`]) takes jobs in order. For each it installs the submitter's
//!   address space and credentials on itself, so a job's buffers and the requests it sends to
//!   user schemes are the submitter's, runs the chain by the usual break rules with the files
//!   `queue` found, removes the address space again, and posts the chain's CQEs. It holds no
//!   descriptor table: a job reaches only the files `queue` resolved, so the worker never keeps
//!   another process's descriptors, or the ring itself, open.
//! - [`close`] (the ring's close) marks the work closing and interrupts the job running, as a
//!   signal would; the worker completes the jobs still queued with `ECANCELED` without
//!   running them, and ends. A job whose submitter's address space is gone completes with
//!   `ECANCELED` too.
//!
//! The worker holds the ring, so the ring's memory stays until the worker has posted its last
//! completion. `in_flight` counts the SQEs queued or running; `Enter`'s `min_complete` waits
//! only while it is not zero.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};

use ferric_ring::{
    abi::MAX_CHAIN,
    chain::{Chain, Completions},
    entry::FileRef,
};
use spin::Mutex;
use syscall::error::{Error, Result, EBADF, ECANCELED, ENOMEM};

use super::{errno_result, release, resolve, run_with, Description, Ring};
use crate::{
    context::{
        self, memory::AddrSpaceWrapper, unblock_context, wakeup_context, ContextLock, Status,
    },
    scheme::CallerCtx,
    sync::{CleanLockToken, L1},
    syscall::process::exit_this_context,
};

/// A ring's asynchronous work, under the ring's `work` lock.
pub(super) struct Work {
    jobs: VecDeque<Job>,
    /// SQEs queued or running.
    in_flight: u32,
    worker: Worker,
    /// The ring is closed: queued jobs are cancelled and the worker ends.
    closing: bool,
}

impl Work {
    /// No work and no worker.
    pub(super) const fn new() -> Self {
        Self {
            jobs: VecDeque::new(),
            in_flight: 0,
            worker: Worker::None,
            closing: false,
        }
    }

    /// The SQEs queued or running.
    pub(super) const fn in_flight(&self) -> u32 {
        self.in_flight
    }
}

/// Where the ring's worker is.
enum Worker {
    /// Not started.
    None,
    /// Spawned and not yet running.
    Starting,
    /// Running, so [`close`] can interrupt it.
    Running(Weak<ContextLock>),
    /// Ended, after the ring closed.
    Done,
}

/// An asynchronous chain, as `Enter` took it.
struct Job {
    chain: Chain,
    /// Each entry's file, looked up by `Enter`: `None` for an entry that names none.
    files: Vec<Option<Result<Description>>>,
    /// The submitter's address space, which holds the buffers.
    space: Weak<AddrSpaceWrapper>,
    /// The submitter's credentials, which requests to user schemes carry.
    caller: CallerCtx,
}

/// Rings whose worker was spawned but has not yet found its ring, by the worker's context.
static STARTING: Mutex<alloc::collections::BTreeMap<usize, Arc<Ring>>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// A context's key in [`STARTING`].
fn key(context: &Arc<ContextLock>) -> usize {
    Arc::as_ptr(context) as usize
}

/// Looks up the files of `chain` in the caller's context, as `Enter` would to run it.
fn resolve_files(
    ring: &Ring,
    chain: &Chain,
    token: &mut CleanLockToken,
) -> Vec<Option<Result<Description>>> {
    let mut files = Vec::with_capacity(MAX_CHAIN);
    for sqe in chain.entries() {
        let file = sqe.op().ok().and_then(|op| op.file());
        files.push(file.map(|file| resolve(ring, file, token)));
    }
    files
}

/// Gives back the files a job did not use.
fn release_files(files: Vec<Option<Result<Description>>>, token: &mut CleanLockToken) {
    for description in files.into_iter().flatten().flatten() {
        release(description, token);
    }
}

/// Queues `chain` for the ring's worker, starting the worker for the first one. Room for its
/// completions is set aside already.
///
/// # Errors
///
/// The errno its SQEs complete with instead: `EBADF` when the ring is closing, `ENOMEM` when
/// the worker cannot be started.
pub(super) fn queue(
    ring: &Arc<Ring>,
    chain: Chain,
    token: &mut CleanLockToken,
) -> core::result::Result<(), i32> {
    let files = resolve_files(ring, &chain, token);
    let (space, caller) = {
        let current = Arc::clone(&*context::current());
        let context = current.read(token.token());
        (
            context
                .addr_space
                .as_ref()
                .map_or_else(Weak::new, Arc::downgrade),
            context.caller_ctx(),
        )
    };
    let job = Job {
        chain,
        files,
        space,
        caller,
    };
    let start = {
        let mut work = ring.work.lock();
        if work.closing {
            drop(work);
            release_files(job.files, token);
            return Err(EBADF);
        }
        work.jobs.push_back(job);
        work.in_flight = work.in_flight.saturating_add(chain.len());
        let start = matches!(work.worker, Worker::None);
        if start {
            work.worker = Worker::Starting;
        }
        start
    };
    if start && spawn(ring, token).is_err() {
        fail_start(ring, token);
        return Err(ENOMEM);
    }
    ring.jobs_ready.notify(token);
    Ok(())
}

/// Starts the worker of `ring`.
fn spawn(ring: &Arc<Ring>, token: &mut CleanLockToken) -> Result<()> {
    let context_lock = context::spawn(false, None, worker_main, token)?;
    // The context stays blocked until it is marked runnable below, so it finds its ring.
    STARTING.lock().insert(key(&context_lock), Arc::clone(ring));
    let cpu_id = {
        let mut context = context_lock.write(token.token());
        context.name.clear();
        let _ = context.name.try_push_str("[ring worker]");
        context.status = Status::Runnable;
        context.cpu_id
    };
    wakeup_context(&context_lock, cpu_id, &mut token.downgrade());
    Ok(())
}

/// The worker could not be started: completes every queued job with `ENOMEM` but the last,
/// whose SQEs the caller completes, and lets the next asynchronous chain try again.
fn fail_start(ring: &Ring, token: &mut CleanLockToken) {
    let mut jobs: Vec<Job> = {
        let mut work = ring.work.lock();
        work.worker = Worker::None;
        let jobs: Vec<Job> = work.jobs.drain(..).collect();
        let sqes = jobs
            .iter()
            .map(|job| job.chain.len())
            .fold(0, u32::saturating_add);
        work.in_flight = work.in_flight.saturating_sub(sqes);
        jobs
    };
    // The caller's own chain was queued last.
    if let Some(own) = jobs.pop() {
        release_files(own.files, token);
    }
    if jobs.is_empty() {
        return;
    }
    for job in jobs {
        let done = cancelled(job, ENOMEM, token);
        // `in_flight` already left these out.
        if let Some(shared) = ring.shared.get() {
            let _ = ring.post(&shared.frames, &done);
        }
    }
    ring.notify_completed(token);
}

/// Marks the ring's work closing and interrupts the job running, so the worker cancels the
/// rest and ends.
pub(super) fn close(ring: &Ring, token: &mut CleanLockToken) {
    let running = {
        let mut work = ring.work.lock();
        work.closing = true;
        match &work.worker {
            Worker::Running(context) => context.upgrade(),
            _ => None,
        }
    };
    ring.jobs_ready.notify(token);
    if let Some(context) = running {
        // A job blocked in a scheme request sees this as a signal: the request is cancelled.
        unblock_context(&context, &mut token.downgrade());
    }
}

/// The worker's entry point.
extern "C" fn worker_main() {
    // SAFETY: this is the first code of a new kernel context, which holds no locks, so the
    // whole lock order is free to it, as for `userspace_init`.
    let mut token = unsafe { CleanLockToken::new() };
    let me = Arc::clone(&*context::current());
    // The address space `context::spawn` gave the context is never used: each job installs
    // its submitter's.
    drop_space(install(None, &mut token), &mut token);
    let ring = STARTING.lock().remove(&key(&me));
    if let Some(ring) = ring {
        ring.work.lock().worker = Worker::Running(Arc::downgrade(&me));
        work(&ring, &mut token);
    }
    drop(me);
    exit_this_context(None, &mut token)
}

/// Runs the ring's jobs until it is closed and none is left.
fn work(ring: &Arc<Ring>, token: &mut CleanLockToken) {
    loop {
        let Some((job, closing)) = next_job(ring, token) else {
            return;
        };
        let sqes = job.chain.len();
        let done = if closing {
            cancelled(job, ECANCELED, token)
        } else {
            run_job(job, token)
        };
        finish(ring, sqes, &done, token);
    }
}

/// Waits for the next job; `None` once the ring is closed and none is left.
fn next_job(ring: &Ring, token: &mut CleanLockToken) -> Option<(Job, bool)> {
    loop {
        let mut work = ring.work.lock();
        if let Some(job) = work.jobs.pop_front() {
            let closing = work.closing;
            return Some((job, closing));
        }
        if work.closing {
            work.worker = Worker::Done;
            return None;
        }
        let mut wait_token = token.downgrade::<L1>();
        // A wakeup without a notification (the close's interruption) just checks again.
        let _ = ring.jobs_ready.wait(work, "ring worker", &mut wait_token);
    }
}

/// Posts a job's completions, counts its SQEs done, and wakes whoever waits for them.
fn finish(ring: &Ring, sqes: u32, done: &Completions, token: &mut CleanLockToken) {
    if let Some(shared) = ring.shared.get() {
        // Fails only when the application moved its CQ head out of range; the completions
        // are lost to a corrupt ring, as on `Enter`.
        let _ = ring.post(&shared.frames, done);
    }
    {
        let mut work = ring.work.lock();
        work.in_flight = work.in_flight.saturating_sub(sqes);
    }
    ring.notify_completed(token);
}

/// Completes every entry of `job` with `errno` without running it.
fn cancelled(job: Job, errno: i32, token: &mut CleanLockToken) -> Completions {
    let mut done = Completions::new();
    job.chain.refuse(errno_result(errno), |cqe| {
        done.push(cqe);
    });
    release_files(job.files, token);
    done
}

/// Runs `job` with its submitter's address space and credentials.
fn run_job(job: Job, token: &mut CleanLockToken) -> Completions {
    let Some(space) = job.space.upgrade() else {
        return cancelled(job, ECANCELED, token);
    };
    let Job {
        chain,
        mut files,
        caller,
        ..
    } = job;
    set_caller(caller, token);
    drop_space(install(Some(space), token), token);

    let mut done = Completions::new();
    let mut slots = files.iter_mut();
    chain.run(
        |sqe| {
            let mut file = slots.next().and_then(Option::take);
            run_with(
                sqe,
                &mut |_: FileRef, _: &mut CleanLockToken| {
                    file.take().unwrap_or(Err(Error::new(EBADF)))
                },
                token,
            )
        },
        errno_result(ECANCELED),
        |cqe| {
            done.push(cqe);
        },
    );
    release_files(files, token);

    let space = install(None, token);
    drop_space(space, token);
    done
}

/// Takes on `caller`'s credentials, which requests to user schemes carry.
fn set_caller(caller: CallerCtx, token: &mut CleanLockToken) {
    let current = Arc::clone(&*context::current());
    let mut context = current.write(token.token());
    context.euid = caller.uid;
    context.egid = caller.gid;
    context.pid = caller.pid;
}

/// Installs `space` as the worker's address space and returns the one it replaces.
fn install(
    space: Option<Arc<AddrSpaceWrapper>>,
    token: &mut CleanLockToken,
) -> Option<Arc<AddrSpaceWrapper>> {
    let current = Arc::clone(&*context::current());
    let mut context = current.write(token.token());
    let (context, token) = context.token_split();
    context.set_addr_space(space, token)
}

/// Drops the worker's reference to an address space, tearing the space down when it was the
/// last one, as a context's exit does.
fn drop_space(space: Option<Arc<AddrSpaceWrapper>>, token: &mut CleanLockToken) {
    if let Some(space) = space.and_then(Arc::into_inner) {
        space.into_drop(token);
    }
}
