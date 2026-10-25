//! FIFO queues that release their lock atomically with entering a wait condition.

use alloc::collections::VecDeque;
use syscall::{EAGAIN, EINTR};

use crate::{
    sync::{CleanLockToken, LockToken, Mutex, WaitCondition, L1, L2},
    syscall::{
        error::{Error, Result, EINVAL},
        usercopy::UserSliceWo,
    },
};

use super::records::{copy_records, QueueRecord};

/// A FIFO with an associated wait condition for blocked readers.
#[derive(Debug)]
pub struct WaitQueue<T> {
    inner: Mutex<L2, VecDeque<T>>,
    /// Notifies readers when records arrive or an external event interrupts them.
    pub condition: WaitCondition,
}

impl<T> WaitQueue<T> {
    /// Creates an empty queue without allocating.
    pub const fn new() -> WaitQueue<T> {
        WaitQueue {
            inner: Mutex::new(VecDeque::new()),
            condition: WaitCondition::new(),
        }
    }
    /// Tests for queued records while holding the queue lock.
    pub fn is_currently_empty(&self, token: &mut CleanLockToken) -> bool {
        self.inner.lock(token.token()).is_empty()
    }
    /// Copies whole records in FIFO order, optionally waiting for a producer.
    ///
    /// Empty buffers return immediately. A failed user copy leaves all records
    /// queued, even if an earlier part of the copy reached user memory.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` if a nonempty buffer cannot hold a record, `EAGAIN` if a
    /// nonblocking read finds no records, `EINTR` if waiting is interrupted, or
    /// the user-copy error if the destination is inaccessible.
    pub fn receive_into_user(
        &self,
        buf: UserSliceWo,
        block: bool,
        reason: &'static str,
        token: &mut CleanLockToken,
    ) -> Result<usize>
    where
        T: QueueRecord,
    {
        if buf.is_empty() {
            return Ok(0);
        }
        if size_of::<T>() == 0 || buf.len() < size_of::<T>() {
            return Err(Error::new(EINVAL));
        }
        loop {
            let inner = self.inner.lock(token.token());
            let (mut inner, mut token) = inner.into_split();

            if inner.is_empty() {
                if block {
                    // SAFETY: Uses wait_inner because this inner is L2. It's guaranteed there's no other
                    // lock held at this point because clean token is provided from caller.
                    if !self.condition.wait_inner(inner, reason, &mut token) {
                        return Err(Error::new(EINTR));
                    }
                    continue;
                } else {
                    // TODO: EWOULDBLOCK?
                    return Err(Error::new(EAGAIN));
                }
            }

            return copy_records(&mut inner, buf.len(), |offset, bytes| {
                buf.advance(offset)
                    .ok_or(Error::new(EINVAL))?
                    .copy_exactly(bytes)
            });
        }
    }

    /// Appends a record and wakes readers; returns the resulting queue length.
    pub fn send(&self, value: T, token: &mut CleanLockToken) -> usize {
        self.send_locked(value, token.token().downgrade())
    }

    /// Appends and notifies while the caller holds locks below the queue level.
    pub fn send_locked(&self, value: T, mut token: LockToken<'_, L1>) -> usize {
        let len = {
            let mut inner = self.inner.lock(token.token());
            inner.push_back(value);
            inner.len()
        };
        self.condition.notify_locked(token);
        len
    }
}
