//! Linked SQEs: how a consumer groups the SQ into chains and runs them.
//!
//! A *chain* is a run of SQEs in which every entry but the last has
//! [`SqeFlags::LINK`](crate::SqeFlags::LINK); an SQE without it is a chain of one. The
//! entries of a chain may name files of different schemes, and they run in order without a
//! return to the submitter in between.
//!
//! The rules, which [`SqReader`] and [`Chain::run`] implement:
//!
//! - A chain is taken from the SQ only once it is complete: if the SQ ends in the middle of a
//!   chain, the entries stay until a later `Enter` finds the rest ([`Next::Incomplete`]).
//! - A chain has at most [`MAX_CHAIN`] entries. The entries of a longer chain, or of one that
//!   fills the whole SQ without ending (it can never complete), are refused: they complete
//!   with `EINVAL`, up to and including the entry that ends the chain ([`Next::Refused`]).
//! - The consumer takes a chain only when the CQ has room for all of its completions, so no
//!   completion is ever dropped. [`SqReader::peek`] shows the chain; [`SqReader::consume`]
//!   takes it.
//! - An entry *breaks* its chain when it fails, or when a transfer ([`Opcode::transfers`])
//!   moves fewer bytes than asked. The entries after it are not run and complete with
//!   `ECANCELED`. Every SQE gets exactly one CQE.

use crate::abi::MAX_CHAIN;
use crate::entry::{Cqe, Opcode, Sqe};
use crate::queue::{Consumer, Memory, RingError};

/// Up to [`MAX_CHAIN`] SQEs, copied out of the SQ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    entries: [Sqe; MAX_CHAIN],
    len: usize,
}

impl Chain {
    /// No entries.
    const fn empty() -> Self {
        Self {
            entries: [Sqe {
                opcode: 0,
                flags: crate::SqeFlags::empty(),
                ioprio: 0,
                fd: 0,
                offset: 0,
                addr: 0,
                len: 0,
                op_flags: 0,
                user_data: 0,
                reserved: [0; 3],
            }; MAX_CHAIN],
            len: 0,
        }
    }

    /// The entries.
    pub fn entries(&self) -> &[Sqe] {
        self.entries.get(..self.len).unwrap_or(&[])
    }

    /// How many entries there are, so how many CQEs the chain needs.
    pub const fn len(&self) -> u32 {
        // At most MAX_CHAIN.
        self.len as u32
    }

    /// Whether there are no entries.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the last entry links on: the chain goes on past these entries.
    fn open(&self) -> bool {
        self.entries().last().is_some_and(Sqe::is_linked)
    }

    /// Adds `sqe`; the caller checks there is room.
    fn push(&mut self, sqe: Sqe) {
        if let Some(slot) = self.entries.get_mut(self.len) {
            *slot = sqe;
            self.len += 1;
        }
    }

    /// Runs the chain: `exec` runs each entry and returns its CQE result (minus the errno on
    /// failure), until an entry breaks the chain; each entry after that gets `cancelled`
    /// (minus `ECANCELED`) without running. `complete` receives each entry's CQE, in order.
    pub fn run(
        &self,
        mut exec: impl FnMut(&Sqe) -> i64,
        cancelled: i64,
        mut complete: impl FnMut(Cqe),
    ) {
        let mut broken = false;
        for sqe in self.entries() {
            let result = if broken { cancelled } else { exec(sqe) };
            broken = broken || breaks(sqe, result);
            complete(Cqe::new(sqe.user_data, result));
        }
    }

    /// Completes every entry with `result` (minus `EINVAL` for a refused chain).
    pub fn refuse(&self, result: i64, mut complete: impl FnMut(Cqe)) {
        for sqe in self.entries() {
            complete(Cqe::new(sqe.user_data, result));
        }
    }
}

/// Whether `sqe`, completed with `result`, breaks its chain: it failed, or it is a transfer
/// that moved fewer than `len` bytes.
pub fn breaks(sqe: &Sqe, result: i64) -> bool {
    let transfers = Opcode::from_raw(sqe.opcode).is_some_and(Opcode::transfers);
    result < 0 || (transfers && result < i64::from(sqe.len))
}

/// What the SQ holds next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// No entries.
    Empty,
    /// The start of a chain whose end the submitter has not published yet.
    Incomplete,
    /// A complete chain to run.
    Chain(Chain),
    /// Entries of an overlong or never-ending chain, to complete with `EINVAL`.
    Refused(Chain),
}

/// The SQ's consumer, grouping entries into chains.
#[derive(Debug)]
pub struct SqReader {
    sq: Consumer<Sqe>,
    /// Whether the entries taken last were refused and their chain goes on: the next entries
    /// up to its end are refused too.
    refusing: bool,
}

impl SqReader {
    /// The reader of an empty SQ.
    pub const fn new(sq: Consumer<Sqe>) -> Self {
        Self {
            sq,
            refusing: false,
        }
    }

    /// The SQ's consumer.
    pub const fn consumer(&self) -> &Consumer<Sqe> {
        &self.sq
    }

    /// The next chain, copied out of the SQ; the entries stay until [`SqReader::consume`].
    ///
    /// # Errors
    ///
    /// [`RingError`] when the SQ cannot be used.
    pub fn peek<M: Memory + ?Sized>(&self, mem: &M) -> Result<Next, RingError> {
        let pending = self.sq.pending(mem)?;
        let mut chain = Chain::empty();
        for n in 0..pending.min(MAX_CHAIN as u32) {
            let Some(sqe) = self.sq.peek(mem, n)? else {
                break;
            };
            chain.push(sqe);
            if !sqe.is_linked() {
                break;
            }
        }
        if chain.is_empty() {
            return Ok(Next::Empty);
        }
        let entries = self.sq.queue().entries;
        Ok(if self.refusing {
            // The rest of a refused chain, as far as it has been published.
            Next::Refused(chain)
        } else if !chain.open() {
            Next::Chain(chain)
        } else if chain.len() == MAX_CHAIN as u32 || chain.len() == entries {
            // Too long to run, or filling the SQ so that its end can never be written.
            Next::Refused(chain)
        } else {
            Next::Incomplete
        })
    }

    /// Takes `next`, as [`SqReader::peek`] returned it, out of the SQ.
    ///
    /// # Errors
    ///
    /// [`RingError`] when the SQ cannot be used.
    pub fn consume<M: Memory + ?Sized>(&mut self, mem: &M, next: &Next) -> Result<(), RingError> {
        match next {
            Next::Empty | Next::Incomplete => Ok(()),
            Next::Chain(chain) => self.sq.release(mem, chain.len()),
            Next::Refused(chain) => {
                self.sq.release(mem, chain.len())?;
                self.refusing = chain.open();
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::AtomicU64;
    use std::vec::Vec;

    use super::*;
    use crate::abi::Layout;
    use crate::queue::Producer;

    struct Ring {
        mem: Vec<AtomicU64>,
        tx: Producer<Sqe>,
        rx: SqReader,
    }

    impl Ring {
        fn new(sq_entries: u32) -> Self {
            let layout = Layout::new(sq_entries, 0).expect("valid");
            Self {
                mem: (0..layout.words()).map(|_| AtomicU64::new(0)).collect(),
                tx: Producer::new(layout.sq()),
                rx: SqReader::new(Consumer::new(layout.sq())),
            }
        }

        fn submit(&mut self, sqes: &[Sqe]) {
            for sqe in sqes {
                self.tx.push(&self.mem[..], sqe).expect("room");
            }
        }

        fn next(&mut self) -> Next {
            let next = self.rx.peek(&self.mem[..]).expect("valid");
            self.rx.consume(&self.mem[..], &next).expect("valid");
            next
        }
    }

    fn tags(chain: &Chain) -> Vec<u64> {
        chain.entries().iter().map(|s| s.user_data).collect()
    }

    #[test]
    fn unlinked_entries_are_chains_of_one() {
        let mut ring = Ring::new(8);
        ring.submit(&[Sqe::nop(1), Sqe::nop(2)]);
        let Next::Chain(first) = ring.next() else {
            panic!("a chain");
        };
        assert_eq!(tags(&first), [1]);
        let Next::Chain(second) = ring.next() else {
            panic!("a chain");
        };
        assert_eq!(tags(&second), [2]);
        assert_eq!(ring.next(), Next::Empty);
    }

    #[test]
    fn a_chain_waits_for_its_end() {
        let mut ring = Ring::new(8);
        ring.submit(&[Sqe::nop(1).linked(), Sqe::nop(2).linked()]);
        assert_eq!(ring.next(), Next::Incomplete);
        // Nothing was taken: the entries are still there.
        assert_eq!(ring.rx.consumer().pending(&ring.mem[..]), Ok(2));
        ring.submit(&[Sqe::nop(3), Sqe::nop(4)]);
        let Next::Chain(chain) = ring.next() else {
            panic!("a chain");
        };
        assert_eq!(tags(&chain), [1, 2, 3]);
        let Next::Chain(chain) = ring.next() else {
            panic!("a chain");
        };
        assert_eq!(tags(&chain), [4]);
    }

    #[test]
    fn a_chain_filling_the_sq_is_refused() {
        let mut ring = Ring::new(4);
        ring.submit(&[Sqe::nop(1).linked(); 4]);
        let Next::Refused(chain) = ring.next() else {
            panic!("refused");
        };
        assert_eq!(chain.len(), 4);
        // Its end, when it comes, is refused too, and the reader then recovers.
        ring.submit(&[Sqe::nop(5).linked(), Sqe::nop(6), Sqe::nop(7)]);
        let Next::Refused(rest) = ring.next() else {
            panic!("refused");
        };
        assert_eq!(tags(&rest), [5, 6]);
        let Next::Chain(chain) = ring.next() else {
            panic!("a chain");
        };
        assert_eq!(tags(&chain), [7]);
    }

    #[test]
    fn an_overlong_chain_is_refused() {
        let mut ring = Ring::new(64);
        let long: Vec<Sqe> = (0..40).map(|i| Sqe::nop(i).linked()).collect();
        ring.submit(&long);
        ring.submit(&[Sqe::nop(40), Sqe::nop(41)]);
        let Next::Refused(first) = ring.next() else {
            panic!("refused");
        };
        assert_eq!(first.len(), MAX_CHAIN as u32);
        let Next::Refused(rest) = ring.next() else {
            panic!("refused");
        };
        assert_eq!(rest.len(), 9);
        assert_eq!(rest.entries().last().map(|s| s.user_data), Some(40));
        let Next::Chain(chain) = ring.next() else {
            panic!("a chain");
        };
        assert_eq!(tags(&chain), [41]);
    }

    #[test]
    fn a_failure_cancels_the_rest_of_the_chain() {
        let mut ring = Ring::new(8);
        ring.submit(&[
            Sqe::nop(1).linked(),
            Sqe::read(3, 0, 0x1000, 16, 2).linked(),
            Sqe::nop(3).linked(),
            Sqe::nop(4),
        ]);
        let Next::Chain(chain) = ring.next() else {
            panic!("a chain");
        };
        let mut ran = Vec::new();
        let mut cqes = Vec::new();
        chain.run(
            |sqe| {
                ran.push(sqe.user_data);
                if sqe.user_data == 2 { -9 } else { 0 }
            },
            -125,
            |cqe| cqes.push((cqe.user_data, cqe.result)),
        );
        assert_eq!(ran, [1, 2]);
        assert_eq!(cqes, [(1, 0), (2, -9), (3, -125), (4, -125)]);
    }

    #[test]
    fn a_short_transfer_breaks_the_chain() {
        let read = Sqe::read(3, 0, 0x1000, 16, 1);
        assert!(breaks(&read, 15));
        assert!(!breaks(&read, 16));
        assert!(breaks(&read, -5));
        // Operations that move no bytes break only on failure.
        assert!(!breaks(&Sqe::fsync(3, 1), 0));
        assert!(!breaks(&Sqe::nop(1), 0));
    }

    #[test]
    fn refused_chains_complete_every_entry() {
        let mut ring = Ring::new(4);
        ring.submit(&[Sqe::nop(1).linked(); 4]);
        let Next::Refused(chain) = ring.next() else {
            panic!("refused");
        };
        let mut cqes = Vec::new();
        chain.refuse(-22, |cqe| cqes.push(cqe.result));
        assert_eq!(cqes, [-22; 4]);
    }
}
