//! Reply-before-park handoff for lazy borrowed file mappings.
//!
//! The requester and provider serialize these transitions with the context's
//! write lock. This state decides whether that same critical section must mark
//! the context hard-blocked/runnable; scheduler operations remain in the caller.

#![forbid(unsafe_code)]

#[derive(Debug)]
enum State<T> {
    Idle,
    Pending,
    Waiting,
    Ready(T),
}

/// One lazy-mapping request's response slot, protected by the context lock.
#[derive(Debug)]
pub(crate) struct FmapReply<T>(State<T>);

impl<T> FmapReply<T> {
    /// Creates an idle slot.
    pub(crate) const fn new() -> Self {
        Self(State::Idle)
    }

    /// Begins a request before dispatch, discarding any stale response.
    pub(crate) fn begin(&mut self) {
        self.0 = State::Pending;
    }

    /// Records the intent to block if the response has not arrived.
    ///
    /// True requires marking the context blocked under the same lock. False
    /// means a response is already ready, or there is no pending request.
    pub(crate) fn park(&mut self) -> bool {
        if matches!(self.0, State::Pending) {
            self.0 = State::Waiting;
            true
        } else {
            false
        }
    }

    /// Publishes the response and returns whether the registered waiter needs waking.
    ///
    /// Idle or duplicate responses are ignored. The caller changes the matching
    /// hard-blocked context to runnable under the same lock before waking it.
    pub(crate) fn provide(&mut self, value: T) -> bool {
        let waiting = matches!(self.0, State::Waiting);
        if waiting || matches!(self.0, State::Pending) {
            self.0 = State::Ready(value);
        }
        waiting
    }

    /// Takes a ready response exactly once and returns to idle.
    pub(crate) fn take(&mut self) -> Option<T> {
        match core::mem::replace(&mut self.0, State::Idle) {
            State::Ready(value) => Some(value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_before_park_needs_neither_block_nor_wakeup() {
        let mut reply = FmapReply::new();
        reply.begin();
        assert!(!reply.provide(42));
        assert!(!reply.park());
        assert_eq!(reply.take(), Some(42));
        assert_eq!(reply.take(), None);
    }

    #[test]
    fn park_before_reply_registers_exactly_one_wakeup() {
        let mut reply = FmapReply::new();
        reply.begin();
        assert!(reply.park());
        assert!(reply.provide(42));
        assert!(!reply.provide(99));
        assert_eq!(reply.take(), Some(42));
    }

    #[test]
    fn a_new_request_cannot_consume_an_old_response() {
        let mut reply = FmapReply::new();
        reply.begin();
        reply.provide(1);
        reply.begin();
        assert!(reply.park());
        assert!(reply.provide(2));
        assert_eq!(reply.take(), Some(2));
        assert!(!reply.provide(3));
        assert_eq!(reply.take(), None);
    }

    #[test]
    fn failed_dispatch_leaves_no_block_or_response_to_consume() {
        let mut reply = FmapReply::<u64>::new();
        reply.begin();
        assert_eq!(reply.take(), None);
        assert!(!reply.park());
        assert!(!reply.provide(1));
        assert_eq!(reply.take(), None);
    }
}
