//! Ferric Oxide: reads and writes on a disk descriptor served concurrently.
//!
//! The scheme serves calls one at a time, which keeps a disk with several hardware queues
//! (nvmed with per-CPU queue pairs) from ever having two requests in flight. When the disk
//! says its clones may serve requests at the same time ([`Disk::concurrent`]), a plain read
//! or write on a disk or partition descriptor is checked and located here, at once, and
//! becomes a [`PendingIo`]: the request together with what it needs to answer itself.
//!
//! The disk can take the request with [`Disk::submit`] and serve it on another thread (nvmed
//! hands it to a worker, which writes the response itself, so the reply skips the scheme's
//! thread). Otherwise it runs as a task of its own on the scheme's thread, on a clone of the
//! disk. Every other call, and every call on other disks, is still served in turn by `tick`.
//!
//! The call's buffer is mapped into the driver until its response is written, which only the
//! [`PendingIo`] does, exactly once (dropping it unanswered answers `EIO`). A descriptor
//! closed while its requests run is held open until the last one ends ([`SharedGate`], over
//! `ferric_blkqueue::order::CloseGate`). `tick` runs a close that has become ready before it
//! serves the next request, and the request that makes it ready wakes the scheme's loop
//! ([`SharedGate::ready`]), so the close also runs on a disk that gets no further requests.
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use ferric_blkqueue::order::CloseGate;
use redox_scheme::scheme::{Op, OpRead, OpWrite};
use redox_scheme::{Response, SignalBehavior, Socket};
use syscall::{Error, EIO};

use super::{Disk, Handle, Hardware, RingDiskSchemeInner};

impl<T: Disk + Clone + 'static, Hw: Hardware> RingDiskSchemeInner<T, Hw> {
    /// Starts `op` when it is a read or write the disk can serve concurrently, or gives it
    /// back to be served in turn.
    pub(super) fn spawn_io(&mut self, op: Op) -> Result<(), Op> {
        let mut io = match op {
            Op::Read(read) => Io::Read(read),
            Op::Write(write) => Io::Write(write),
            other => return Err(other),
        };
        let (id, offset, len) = match &mut io {
            Io::Read(read) => (read.fd, read.offset, read.buf().len()),
            Io::Write(write) => (write.fd, write.offset, write.buf().len()),
        };
        let Ok(&Handle::Disk { num, pt }) = self.handles.get(id) else {
            return Err(io.into_op());
        };
        let Some(wrapper) = self.disks.get_mut(&num) else {
            return Err(io.into_op());
        };
        if !wrapper.disk.concurrent() {
            return Err(io.into_op());
        }
        let block_size = u64::from(wrapper.block_size());
        let block = match wrapper.locate(pt, offset / block_size, len) {
            Ok(block) => block,
            Err(err) => {
                respond(&self.socket, io.respond(Err(err)));
                return Ok(());
            }
        };
        self.gate.begin(id);
        let pending = PendingIo {
            io: Some(io),
            block,
            fd: id,
            socket: Arc::clone(&self.socket),
            gate: self.gate.clone(),
        };
        let Err(mut pending) = wrapper.disk.submit(pending) else {
            return Ok(());
        };
        let mut disk = wrapper.disk.clone();
        // The task is detached: it ends by itself once its response is written.
        let _task = self.executor.spawn(async move {
            let block = pending.block();
            let result = match pending.transfer() {
                Transfer::Read(buf) => disk.read(block, buf).await,
                Transfer::Write(buf) => disk.write(block, buf).await,
            };
            pending.finish(result);
        });
        Ok(())
    }
}

/// Writes `response`, logging a failure (the caller then gets no answer, as with any
/// response the scheme cannot write).
fn respond(socket: &Socket, response: Response) {
    if let Err(err) = socket.write_response(response, SignalBehavior::Restart) {
        log::error!("disk scheme: cannot answer a request: {err}");
    }
}

/// A read or write on a disk, located and checked, that answers itself: [`finish`] it with
/// the result, from any thread. Dropping it unfinished answers `EIO`, so a request is never
/// left without a response.
///
/// [`finish`]: Self::finish
pub struct PendingIo {
    /// `Some` until answered.
    io: Option<Io>,
    block: u64,
    /// The descriptor, for the close gate.
    fd: usize,
    socket: Arc<Socket>,
    gate: SharedGate,
}

// SAFETY: the call's buffer (the raw pointer in `OpRead`/`OpWrite`) is mapped into the
// driver for this request until its response is written, and nothing else refers to it: the
// scheme moved the call in here and keeps no copy. Only the thread holding the `PendingIo`
// touches the buffer, and the response that releases it is written once, by that thread. The
// socket and the gate are `Send + Sync`.
unsafe impl Send for PendingIo {}

/// The buffer of a [`PendingIo`].
pub enum Transfer<'a> {
    /// Read into this buffer.
    Read(&'a mut [u8]),
    /// Write from this buffer.
    Write(&'a [u8]),
}

impl PendingIo {
    /// The disk block the transfer starts at (partition offset applied).
    pub fn block(&self) -> u64 {
        self.block
    }

    /// The transfer's buffer: a whole number of blocks.
    pub fn transfer(&mut self) -> Transfer<'_> {
        match &mut self.io {
            Some(Io::Read(read)) => Transfer::Read(read.buf()),
            Some(Io::Write(write)) => Transfer::Write(write.buf()),
            // Only `answer` empties it, and nothing uses the request after that.
            None => Transfer::Write(&[]),
        }
    }

    /// Answers the request with `result` (bytes transferred, or the error).
    pub fn finish(mut self, result: syscall::Result<usize>) {
        self.answer(result);
    }

    fn answer(&mut self, result: syscall::Result<usize>) {
        let Some(io) = self.io.take() else {
            return;
        };
        // The transfer is over: a close waiting for it may run from now on, and the
        // response below only needs the request's tag.
        self.gate.end(self.fd);
        respond(&self.socket, io.respond(result));
    }
}

impl Drop for PendingIo {
    fn drop(&mut self) {
        self.answer(Err(Error::new(EIO)));
    }
}

/// A call that [`RingDiskSchemeInner::spawn_io`] may serve concurrently.
enum Io {
    Read(OpRead),
    Write(OpWrite),
}

impl Io {
    /// The call, to be served in turn after all.
    fn into_op(self) -> Op {
        match self {
            Self::Read(read) => Op::Read(read),
            Self::Write(write) => Op::Write(write),
        }
    }

    /// The response that answers the call with `result`.
    fn respond(self, result: syscall::Result<usize>) -> Response {
        match self {
            Self::Read(read) => Response::new(result, read),
            Self::Write(write) => Response::new(result, write),
        }
    }
}

/// Starts counting a request on descriptor `fd` in `gate` at once, and returns the task that
/// runs `work`, hands its result to `answer` (which writes the response), and then counts the
/// request done, waking the scheme's loop if that makes a waiting close ready.
///
/// The tests below build requests with this; `spawn_io` counts what it starts in [`PendingIo`]
/// instead, because that one may finish on another thread.
#[cfg(test)]
fn track<W: Future>(
    gate: &SharedGate,
    fd: usize,
    work: W,
    answer: impl FnOnce(W::Output),
) -> impl Future<Output = ()> {
    // Counted before the task first runs, so that a close the scheme reads next already sees
    // the request in flight.
    gate.begin(fd);
    let gate = gate.clone();
    async move {
        answer(work.await);
        gate.end(fd);
    }
}

/// The scheme's [`CloseGate`], shared with its concurrent requests, which wakes the scheme's
/// loop when a close it held back becomes ready.
#[derive(Clone, Default)]
pub struct SharedGate(Arc<Mutex<GateState>>);

#[derive(Default)]
struct GateState {
    gate: CloseGate,
    /// The scheme loop waiting in [`SharedGate::ready`], if any.
    waker: Option<Waker>,
}

impl SharedGate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        // Every update is one call on the gate, so its counts stay consistent even if a
        // thread panicked while holding the lock.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn begin(&self, fd: usize) {
        self.lock().gate.begin(fd);
    }

    /// Counts a request done; wakes the scheme loop if a close became ready.
    pub(super) fn end(&self, fd: usize) {
        let waker = {
            let mut state = self.lock();
            state.gate.end(fd);
            if state.gate.has_ready() {
                state.waker.take()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Asks to close `fd`: `true` to close it now, `false` when the close waits.
    pub(super) fn close(&self, fd: usize) -> bool {
        self.lock().gate.close(fd)
    }

    pub(super) fn take_ready(&self) -> Vec<usize> {
        self.lock().gate.take_ready()
    }

    /// Completes once a close that waited for its descriptor's requests may run, so that the
    /// scheme's loop calls `tick` again although no new request has arrived. A close that
    /// became ready before this is polled (just after `tick` returned) completes it at once.
    pub fn ready(&self) -> CloseReady {
        CloseReady(self.clone())
    }
}

/// The future [`SharedGate::ready`] returns.
pub struct CloseReady(SharedGate);

impl Future for CloseReady {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.0.lock();
        if state.gate.has_ready() {
            Poll::Ready(())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use futures::channel::oneshot;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;

    use super::*;

    /// What happened, in order: the disk's contents as the requests and closes saw them.
    type Log = Rc<RefCell<Vec<String>>>;

    /// A write of `byte` on `fd` through `track`, as `spawn_io` runs one: the disk takes the
    /// byte when `go` fires, then the response is written.
    fn write(
        gate: &SharedGate,
        fd: usize,
        byte: u8,
        disk: &Rc<RefCell<Vec<u8>>>,
        log: &Log,
    ) -> (oneshot::Sender<()>, impl Future<Output = ()>) {
        let (go, wait) = oneshot::channel::<()>();
        let (disk, log) = (Rc::clone(disk), Rc::clone(log));
        let work = async move {
            let _ = wait.await;
            disk.borrow_mut().push(byte);
            byte
        };
        let answer = move |byte| log.borrow_mut().push(format!("answer {byte}"));
        (go, track(gate, fd, work, answer))
    }

    /// The scheme loop's close handling, as `tick` and nvmed's loop do it: close what is
    /// ready, else wait for [`SharedGate::ready`].
    async fn close_loop(gate: SharedGate, disk: Rc<RefCell<Vec<u8>>>, log: Log) {
        loop {
            if let Some(fd) = gate.take_ready().first() {
                log.borrow_mut()
                    .push(format!("close {fd} sees {:?}", disk.borrow()));
                return;
            }
            gate.ready().await;
        }
    }

    #[test]
    fn a_close_waits_for_the_writes_and_runs_on_an_idle_disk() {
        let mut pool = LocalPool::new();
        let gate = SharedGate::default();
        let disk = Rc::new(RefCell::new(Vec::new()));
        let log: Log = Rc::default();
        let (go1, w1) = write(&gate, 3, 1, &disk, &log);
        let (go2, w2) = write(&gate, 3, 2, &disk, &log);
        pool.spawner().spawn_local(w1).expect("spawn");
        pool.spawner().spawn_local(w2).expect("spawn");
        pool.run_until_stalled();

        // The client closes while both writes run: the close is held back, and the loop
        // goes idle with no request left to wake it.
        assert!(!gate.close(3));
        pool.spawner()
            .spawn_local(close_loop(gate.clone(), Rc::clone(&disk), Rc::clone(&log)))
            .expect("spawn");
        pool.run_until_stalled();
        assert!(log.borrow().is_empty());

        go1.send(()).expect("first write waits");
        pool.run_until_stalled();
        assert_eq!(*log.borrow(), ["answer 1"], "one write still runs");

        go2.send(()).expect("second write waits");
        pool.run_until_stalled();
        assert_eq!(
            *log.borrow(),
            ["answer 1", "answer 2", "close 3 sees [1, 2]"],
            "the last write's end wakes the loop, and the close sees both writes landed"
        );
    }

    #[test]
    fn a_close_ready_before_the_loop_waits_is_not_missed() {
        // The last request ends just after `tick` returned, before the loop polls `ready`.
        let mut pool = LocalPool::new();
        let gate = SharedGate::default();
        let disk = Rc::new(RefCell::new(Vec::new()));
        let log: Log = Rc::default();
        let (go, w) = write(&gate, 5, 9, &disk, &log);
        pool.spawner().spawn_local(w).expect("spawn");
        pool.run_until_stalled();
        assert!(!gate.close(5));
        go.send(()).expect("write waits");
        pool.run_until_stalled();
        pool.spawner()
            .spawn_local(close_loop(gate.clone(), Rc::clone(&disk), Rc::clone(&log)))
            .expect("spawn");
        pool.run_until_stalled();
        assert_eq!(*log.borrow(), ["answer 9", "close 5 sees [9]"]);
    }

    #[test]
    fn other_descriptors_and_idle_closes_do_not_wait() {
        let mut pool = LocalPool::new();
        let gate = SharedGate::default();
        let disk = Rc::new(RefCell::new(Vec::new()));
        let log: Log = Rc::default();
        let (go, w) = write(&gate, 1, 4, &disk, &log);
        pool.spawner().spawn_local(w).expect("spawn");
        pool.run_until_stalled();
        // Descriptor 2 has nothing in flight: it closes at once, and nothing becomes ready.
        assert!(gate.close(2));
        go.send(()).expect("write waits");
        pool.run_until_stalled();
        assert!(gate.take_ready().is_empty());
        assert_eq!(*log.borrow(), ["answer 4"]);
    }
}
