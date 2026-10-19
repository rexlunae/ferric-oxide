//! Ferric Oxide: reads and writes on a disk descriptor served as concurrent tasks.
//!
//! The scheme serves calls one at a time, which keeps a disk with several hardware queues
//! (nvmed with per-CPU queue pairs) from ever having two requests in flight. When the disk
//! says its clones may serve requests at the same time ([`Disk::concurrent`]), a plain read
//! or write on a disk or partition descriptor is checked and located here, at once, and then
//! runs as a task of its own on a clone of the disk, which writes its response itself. Every
//! other call, and every call on other disks, is still served in turn by `tick`.
//!
//! The call's buffer is mapped into the driver until its response is written, which only the
//! task does. A descriptor closed while its requests run is held open until the last one ends
//! (`ferric_blkqueue::order::CloseGate`); `tick` runs the close before it serves the next
//! request.

use redox_scheme::scheme::{Op, OpRead, OpWrite};
use redox_scheme::{Response, SignalBehavior};

use super::{Disk, Handle, Hardware, RingDiskSchemeInner};

impl<T: Disk + Clone + 'static, Hw: Hardware> RingDiskSchemeInner<T, Hw> {
    /// Starts `op` as a task when it is a read or write the disk can serve concurrently, or
    /// gives it back to be served in turn.
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
        let Some(wrapper) = self.disks.get(&num) else {
            return Err(io.into_op());
        };
        if !wrapper.disk.concurrent() {
            return Err(io.into_op());
        }
        let block_size = u64::from(wrapper.block_size());
        let block = match wrapper.locate(pt, offset / block_size, len) {
            Ok(block) => block,
            Err(err) => {
                self.respond(io.respond(Err(err)));
                return Ok(());
            }
        };
        let mut disk = wrapper.disk.clone();
        let socket = std::rc::Rc::clone(&self.socket);
        let gate = std::rc::Rc::clone(&self.gate);
        gate.borrow_mut().begin(id);
        // The task is detached: it ends by itself once its response is written.
        let _task = self.executor.spawn(async move {
            let response = match io {
                Io::Read(mut read) => {
                    let result = disk.read(block, read.buf()).await;
                    Response::new(result, read)
                }
                Io::Write(write) => {
                    let result = disk.write(block, write.buf()).await;
                    Response::new(result, write)
                }
            };
            if let Err(err) = socket.write_response(response, SignalBehavior::Restart) {
                log::error!("disk scheme: cannot answer a concurrent request: {err}");
            }
            gate.borrow_mut().end(id);
        });
        Ok(())
    }

    fn respond(&self, response: Response) {
        if let Err(err) = self
            .socket
            .write_response(response, SignalBehavior::Restart)
        {
            log::error!("disk scheme: cannot answer a request: {err}");
        }
    }
}

/// A call that [`RingDiskSchemeInner::spawn_io`] may run as a task.
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
