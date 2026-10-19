//! The submission/completion ring format every Ferric Oxide scheme shares, and the interface of
//! the kernel's `kernel.ring` scheme (roadmap Phase 2.3, design in `io/rings-design.md` in the
//! project files).
//!
//! A ring is shared memory with two queues: the submitter writes [`Sqe`]s into the submission
//! queue (SQ), the other side runs them and writes one [`Cqe`] per SQE into the completion
//! queue (CQ). SQE opcodes are the kernel's scheme request numbers, so a ring request and a
//! scheme request name the same operation, and SQEs can be linked into chains that cross
//! schemes and run without a return to the submitter.
//!
//! - [`abi`] has the `kernel.ring` scheme's name, number and calls, the [`NotifyMode`]s, and
//!   the memory [`Layout`] both sides compute from the entry counts.
//! - [`entry`] has the [`Sqe`] and [`Cqe`] formats and the checks that turn an SQE into an
//!   [`Op`].
//! - [`queue`] has the single-producer, single-consumer [`Producer`] and [`Consumer`] over
//!   [`Memory`] that the other side may write at any time: every access is atomic, each side
//!   keeps its own index privately, and a corrupt peer index is an error, never undefined
//!   behaviour. This crate has no `unsafe` code.
//! - [`chain`] groups the SQ into chains ([`SqReader`]) and runs them by the break rules.
//! - `files` (with the `alloc` feature) is the ring's file table, the slots that fixed-file
//!   SQEs name.
//! - [`notify`] is the need-wakeup protocol that lets both sides skip system calls while the
//!   other is awake, without losing wakeups.
//!
//! # Examples
//!
//! The submitter's side and the consumer's side of one ring, here in the same process:
//!
//! ```
//! use core::sync::atomic::AtomicU64;
//! use ferric_ring::{chain::Next, Consumer, Cqe, Layout, Producer, Sqe, SqReader};
//!
//! let layout = Layout::new(8, 0).expect("powers of two");
//! let mem: Vec<AtomicU64> = (0..layout.words()).map(|_| AtomicU64::new(0)).collect();
//! let mem = &mem[..];
//!
//! // The submitter links a read to a write: the write runs only if the read filled the buffer.
//! let mut sq = Producer::<Sqe>::new(layout.sq());
//! sq.push(mem, &Sqe::read(3, 0, 0x1000, 512, 1).linked()).expect("room");
//! sq.push(mem, &Sqe::write(4, 0, 0x1000, 512, 2)).expect("room");
//!
//! // The consumer takes the chain, runs it and posts the completions.
//! let mut reader = SqReader::new(Consumer::new(layout.sq()));
//! let mut cq = Producer::<Cqe>::new(layout.cq());
//! let next = reader.peek(mem).expect("valid");
//! reader.consume(mem, &next).expect("valid");
//! let Next::Chain(chain) = next else { panic!("a complete chain") };
//! chain.run(|sqe| i64::from(sqe.len), -125, |cqe| cq.push(mem, &cqe).expect("room"));
//!
//! // The submitter reads one completion per SQE.
//! let mut completions = Consumer::<Cqe>::new(layout.cq());
//! assert_eq!(completions.pop(mem).expect("valid"), Some(Cqe::new(1, 512)));
//! assert_eq!(completions.pop(mem).expect("valid"), Some(Cqe::new(2, 512)));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(any(feature = "alloc", test))]
extern crate alloc;

pub mod abi;
pub mod chain;
pub mod entry;
#[cfg(any(feature = "alloc", test))]
pub mod files;
pub mod notify;
pub mod queue;

pub use abi::{Layout, NotifyMode};
pub use chain::SqReader;
pub use entry::{Cqe, Entry, Op, Sqe, SqeFlags};
pub use queue::{Consumer, Memory, Producer, Reserving};
