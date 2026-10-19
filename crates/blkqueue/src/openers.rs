//! Which process opened each disk descriptor, so that a call changing the whole disk (the
//! queue policy) is refused on a descriptor that was passed to another process.
//!
//! A disk scheme only lets root open disk descriptors, but a descriptor can then be sent to
//! any process. The scheme's call handler sees the caller's process id, not its user id, so
//! the scheme records the opener's process id at open and [`Openers::check`] compares the
//! caller's against it.

use alloc::collections::BTreeMap;
use core::fmt;

/// The opener's process id of each open descriptor, by handle id.
#[derive(Debug, Default)]
pub struct Openers {
    by_handle: BTreeMap<usize, usize>,
}

/// The caller is not the process that opened the descriptor (or it is not a recorded one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotOpener;

impl fmt::Display for NotOpener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the caller did not open this disk descriptor")
    }
}

impl core::error::Error for NotOpener {}

impl Openers {
    /// An empty table.
    pub const fn new() -> Self {
        Self {
            by_handle: BTreeMap::new(),
        }
    }

    /// Records that process `pid` opened descriptor `handle`.
    pub fn opened(&mut self, handle: usize, pid: usize) {
        self.by_handle.insert(handle, pid);
    }

    /// Forgets descriptor `handle`, which is closed (its id may be reused).
    pub fn closed(&mut self, handle: usize) {
        self.by_handle.remove(&handle);
    }

    /// Whether process `pid` opened descriptor `handle`.
    ///
    /// # Errors
    ///
    /// [`NotOpener`] when another process opened it, or it was never recorded.
    pub fn check(&self, handle: usize, pid: usize) -> Result<(), NotOpener> {
        match self.by_handle.get(&handle) {
            Some(&opener) if opener == pid => Ok(()),
            _ => Err(NotOpener),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_opener_passes() {
        let mut openers = Openers::new();
        openers.opened(3, 100);
        assert_eq!(openers.check(3, 100), Ok(()));
        // The descriptor sent to process 200.
        assert_eq!(openers.check(3, 200), Err(NotOpener));
    }

    #[test]
    fn unknown_and_closed_descriptors_are_refused() {
        let mut openers = Openers::new();
        assert_eq!(openers.check(1, 100), Err(NotOpener));
        openers.opened(1, 100);
        openers.closed(1);
        assert_eq!(openers.check(1, 100), Err(NotOpener));
    }

    #[test]
    fn a_reused_handle_id_belongs_to_its_new_opener() {
        let mut openers = Openers::new();
        openers.opened(5, 100);
        openers.closed(5);
        openers.opened(5, 200);
        assert_eq!(openers.check(5, 100), Err(NotOpener));
        assert_eq!(openers.check(5, 200), Ok(()));
    }
}
