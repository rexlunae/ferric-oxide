//! Per-event argument filters (roadmap T3c): only records whose arguments pass are written.
//!
//! A [`Filter`] is a conjunction of up to [`MAX_CONDITIONS`] [`Condition`]s, each comparing
//! one named argument of its event with a constant:
//!
//! ```text
//! nr == 3 && a < 0x1000
//! ```
//!
//! The arguments are the event's own names from the catalog; values are decimal or `0x`
//! hexadecimal `u64`s; the operators are `==`, `!=`, `<`, `<=`, `>`, `>=` and `&` (any of the
//! value's bits set). The `kernel.trace` scheme takes `filter <event> <filter>` and
//! `filter <event> none` ([`crate::abi::Command::Filter`]).
//!
//! The kernel keeps one [`Slot`] per event. A slot is a sequence lock over atomics: the one
//! writer (the scheme, under its lock) stores a filter, and tracepoints on any CPU check
//! records against it without locking, retrying a read that overlapped a store. An empty slot
//! costs a tracepoint one relaxed load.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};

use crate::Event;

/// The most conditions one filter holds.
pub const MAX_CONDITIONS: usize = 4;

/// How a [`Condition`] compares an argument with its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Op {
    /// `==`
    Eq = 0,
    /// `!=`
    Ne = 1,
    /// `<`
    Lt = 2,
    /// `<=`
    Le = 3,
    /// `>`
    Gt = 4,
    /// `>=`
    Ge = 5,
    /// `&`: the argument has any of the value's bits set.
    AnyBits = 6,
}

impl Op {
    /// Every operator, in [`Op::raw`] order.
    pub const ALL: [Op; 7] = [Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::AnyBits];

    /// The operator as written in a filter.
    pub const fn symbol(self) -> &'static str {
        match self {
            Op::Eq => "==",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::AnyBits => "&",
        }
    }

    /// The operator written `symbol`.
    pub fn from_symbol(symbol: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.symbol() == symbol)
    }

    /// The operator as a number.
    pub const fn raw(self) -> u8 {
        self as u8
    }

    /// The operator numbered `raw`.
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Op::Eq),
            1 => Some(Op::Ne),
            2 => Some(Op::Lt),
            3 => Some(Op::Le),
            4 => Some(Op::Gt),
            5 => Some(Op::Ge),
            6 => Some(Op::AnyBits),
            _ => None,
        }
    }

    /// Whether `arg` compares to `value` this way.
    pub const fn holds(self, arg: u64, value: u64) -> bool {
        match self {
            Op::Eq => arg == value,
            Op::Ne => arg != value,
            Op::Lt => arg < value,
            Op::Le => arg <= value,
            Op::Gt => arg > value,
            Op::Ge => arg >= value,
            Op::AnyBits => arg & value != 0,
        }
    }
}

/// One comparison: argument number `arg` of the event, `op`, `value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Condition {
    /// The argument's index in the event's argument list.
    pub arg: u8,
    /// The comparison.
    pub op: Op,
    /// The constant compared with.
    pub value: u64,
}

impl Condition {
    /// Whether `args` (a record's arguments) pass. A missing argument fails.
    pub fn holds(&self, args: &[u64]) -> bool {
        args.get(usize::from(self.arg))
            .is_some_and(|&arg| self.op.holds(arg, self.value))
    }
}

/// Why a filter did not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterError {
    /// The filter has no conditions (and is not `none`).
    Empty,
    /// A condition is not three words, `<arg> <op> <value>`.
    Syntax,
    /// A condition names an argument the event does not have.
    UnknownArg,
    /// A condition's operator is not one of [`Op::ALL`].
    UnknownOp,
    /// A value is not a decimal or `0x` hexadecimal `u64`.
    Value,
    /// More than [`MAX_CONDITIONS`] conditions.
    TooMany,
}

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FilterError::Empty => "the filter has no conditions",
            FilterError::Syntax => "a condition is not `<arg> <op> <value>`",
            FilterError::UnknownArg => "the event has no argument of that name",
            FilterError::UnknownOp => "unknown operator (==, !=, <, <=, >, >=, &)",
            FilterError::Value => "a value is not a decimal or 0x hexadecimal u64",
            FilterError::TooMany => "a filter has at most 4 conditions",
        })
    }
}

impl core::error::Error for FilterError {}

/// The conditions a record of one event must all pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Filter {
    conditions: [Condition; MAX_CONDITIONS],
    len: u8,
}

/// The word `filter <event>` takes to remove a filter.
pub const NONE: &str = "none";

impl Filter {
    /// Parses `text` as a filter on `event`'s arguments: conditions joined by `&&`, or
    /// [`NONE`] for no filter (`Ok(None)`).
    ///
    /// # Errors
    ///
    /// A [`FilterError`] saying what is wrong.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_trace::{filter::Filter, kernel::SYSCALL_ENTER};
    ///
    /// let filter = Filter::parse(&SYSCALL_ENTER, "nr == 3 && a < 0x10")
    ///     .expect("parses")
    ///     .expect("not none");
    /// assert!(filter.admits(&[3, 15, 0, 0, 0]));
    /// assert!(!filter.admits(&[3, 16, 0, 0, 0]));
    /// assert!(!filter.admits(&[4, 0, 0, 0, 0]));
    /// ```
    pub fn parse(event: &Event, text: &str) -> Result<Option<Self>, FilterError> {
        let text = text.trim();
        if text == NONE {
            return Ok(None);
        }
        let mut filter = Filter {
            conditions: [Condition {
                arg: 0,
                op: Op::Eq,
                value: 0,
            }; MAX_CONDITIONS],
            len: 0,
        };
        for part in text.split("&&") {
            let condition = parse_condition(event, part)?;
            let slot = filter
                .conditions
                .get_mut(usize::from(filter.len))
                .ok_or(FilterError::TooMany)?;
            *slot = condition;
            filter.len += 1;
        }
        Ok(Some(filter))
    }

    /// The conditions.
    pub fn conditions(&self) -> &[Condition] {
        self.conditions
            .get(..usize::from(self.len))
            .unwrap_or_default()
    }

    /// Whether a record with `args` passes every condition.
    pub fn admits(&self, args: &[u64]) -> bool {
        self.conditions().iter().all(|c| c.holds(args))
    }

    /// The filter written with `event`'s argument names, as [`Filter::parse`] reads it.
    pub fn display<'a>(&'a self, event: &'a Event) -> impl fmt::Display + 'a {
        Shown {
            filter: self,
            event,
        }
    }

    /// The filter as [`ENCODED_WORDS`] words, for a [`Slot`]: the count, then a word with
    /// the argument and operator and a word with the value for each condition.
    fn encode(&self) -> [u64; ENCODED_WORDS] {
        let mut words = [0; ENCODED_WORDS];
        words[0] = u64::from(self.len);
        for (i, condition) in self.conditions().iter().enumerate() {
            words[1 + 2 * i] = u64::from(condition.arg) | (u64::from(condition.op.raw()) << 8);
            words[2 + 2 * i] = condition.value;
        }
        words
    }

    /// The filter of [`Filter::encode`]'s words, or `None` for words no filter encodes to.
    fn decode(words: &[u64; ENCODED_WORDS]) -> Option<Self> {
        let len = u8::try_from(words[0])
            .ok()
            .filter(|&len| (1..=MAX_CONDITIONS as u8).contains(&len))?;
        let mut filter = Filter {
            conditions: [Condition {
                arg: 0,
                op: Op::Eq,
                value: 0,
            }; MAX_CONDITIONS],
            len,
        };
        for (i, slot) in filter
            .conditions
            .iter_mut()
            .enumerate()
            .take(usize::from(len))
        {
            let head = words[1 + 2 * i];
            *slot = Condition {
                arg: (head & 0xff) as u8,
                op: Op::from_raw(((head >> 8) & 0xff) as u8)?,
                value: words[2 + 2 * i],
            };
        }
        Some(filter)
    }
}

/// A [`Filter`] with its event's argument names, for display.
struct Shown<'a> {
    filter: &'a Filter,
    event: &'a Event,
}

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, condition) in self.filter.conditions().iter().enumerate() {
            if i > 0 {
                f.write_str(" && ")?;
            }
            let name = self
                .event
                .args()
                .get(usize::from(condition.arg))
                .copied()
                .unwrap_or("?");
            write!(f, "{name} {} {:#x}", condition.op.symbol(), condition.value)?;
        }
        Ok(())
    }
}

/// One `<arg> <op> <value>` condition.
fn parse_condition(event: &Event, text: &str) -> Result<Condition, FilterError> {
    let mut words = text.split_ascii_whitespace();
    let (Some(name), Some(op), Some(value), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(if text.trim().is_empty() {
            FilterError::Empty
        } else {
            FilterError::Syntax
        });
    };
    let arg = event
        .args()
        .iter()
        .position(|&arg| arg == name)
        .and_then(|i| u8::try_from(i).ok())
        .ok_or(FilterError::UnknownArg)?;
    let op = Op::from_symbol(op).ok_or(FilterError::UnknownOp)?;
    Ok(Condition {
        arg,
        op,
        value: parse_value(value)?,
    })
}

/// A decimal or `0x` hexadecimal `u64`.
fn parse_value(text: &str) -> Result<u64, FilterError> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None if text.bytes().all(|b| b.is_ascii_digit()) => text.parse(),
        None => return Err(FilterError::Value),
    };
    parsed.map_err(|_| FilterError::Value)
}

/// The words of an encoded filter.
const ENCODED_WORDS: usize = 1 + 2 * MAX_CONDITIONS;

/// How many times [`Slot::load`] rereads a filter that a store overlapped before it gives
/// [`Loaded::Busy`], so [`Slot::admits`] stays bounded on the tracepoint path.
const READ_RETRIES: u32 = 8;

/// What [`Slot::load`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// No filter is set.
    Empty,
    /// The filter that is set.
    Set(Filter),
    /// A filter is set, but every reread overlapped a store replacing it. A reader that
    /// excludes the writer (as the kernel's `filters` listing does) never sees this.
    Busy,
}

/// One event's filter, shared between one writer and lock-free readers on every CPU.
///
/// The writer must be serialized by the caller (the kernel holds the scheme's lock):
/// [`Slot::store`] from two threads at once can leave a mix of both filters.
#[derive(Debug)]
pub struct Slot {
    /// Whether a filter is stored: the fast path of [`Slot::admits`].
    active: AtomicBool,
    /// Odd while a store is in progress.
    sequence: AtomicU32,
    words: [AtomicU64; ENCODED_WORDS],
}

impl Default for Slot {
    fn default() -> Self {
        Self::new()
    }
}

impl Slot {
    /// An empty slot: everything passes.
    pub const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            sequence: AtomicU32::new(0),
            words: [const { AtomicU64::new(0) }; ENCODED_WORDS],
        }
    }

    /// Stores `filter`, or empties the slot for `None`. One writer at a time.
    pub fn store(&self, filter: Option<&Filter>) {
        let Some(filter) = filter else {
            self.active.store(false, Ordering::Release);
            return;
        };
        let words = filter.encode();
        let start = self.sequence.load(Ordering::Relaxed);
        self.sequence
            .store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (slot, word) in self.words.iter().zip(words) {
            slot.store(word, Ordering::Relaxed);
        }
        self.sequence
            .store(start.wrapping_add(2), Ordering::Release);
        self.active.store(true, Ordering::Release);
    }

    /// The stored filter: [`Loaded::Set`], [`Loaded::Empty`], or [`Loaded::Busy`] when a
    /// store overlapped each of a few rereads.
    pub fn load(&self) -> Loaded {
        if !self.active.load(Ordering::Acquire) {
            return Loaded::Empty;
        }
        for _ in 0..READ_RETRIES {
            let before = self.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let words: [u64; ENCODED_WORDS] =
                core::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == before {
                // A consistent read of what `store` encoded always decodes; anything else is
                // treated as a read that did not settle.
                return Filter::decode(&words).map_or(Loaded::Busy, Loaded::Set);
            }
        }
        Loaded::Busy
    }

    /// Whether a record with `args` passes the stored filter. An empty slot, or a filter
    /// being replaced for longer than a few rereads, lets it through: a filter only ever
    /// removes records, so a record is never invented.
    #[inline]
    pub fn admits(&self, args: &[u64]) -> bool {
        if !self.active.load(Ordering::Relaxed) {
            return true;
        }
        match self.load() {
            Loaded::Set(filter) => filter.admits(args),
            Loaded::Empty | Loaded::Busy => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{SCHED_SWITCH, SYSCALL_ENTER, SYSCALL_EXIT};
    use std::string::ToString;

    fn parse(event: &Event, text: &str) -> Filter {
        Filter::parse(event, text)
            .expect("parses")
            .expect("not none")
    }

    #[test]
    fn operators() {
        for (op, yes, no) in [
            (Op::Eq, 5, 6),
            (Op::Ne, 6, 5),
            (Op::Lt, 4, 5),
            (Op::Le, 5, 6),
            (Op::Gt, 6, 5),
            (Op::Ge, 5, 4),
            (Op::AnyBits, 4, 2),
        ] {
            assert!(op.holds(yes, 5), "{op:?}");
            assert!(!op.holds(no, 5), "{op:?}");
            assert_eq!(Op::from_symbol(op.symbol()), Some(op));
            assert_eq!(Op::from_raw(op.raw()), Some(op));
        }
        assert_eq!(Op::from_raw(7), None);
        assert_eq!(Op::from_symbol("=~"), None);
    }

    #[test]
    fn parses_and_displays() {
        let filter = parse(&SYSCALL_ENTER, " nr == 3 &&a >= 0x10 && d & 1 ");
        assert_eq!(
            filter.conditions(),
            [
                Condition {
                    arg: 0,
                    op: Op::Eq,
                    value: 3
                },
                Condition {
                    arg: 1,
                    op: Op::Ge,
                    value: 16
                },
                Condition {
                    arg: 4,
                    op: Op::AnyBits,
                    value: 1
                },
            ]
        );
        let shown = filter.display(&SYSCALL_ENTER).to_string();
        assert_eq!(shown, "nr == 0x3 && a >= 0x10 && d & 0x1");
        assert_eq!(parse(&SYSCALL_ENTER, &shown), filter);
        assert_eq!(Filter::parse(&SYSCALL_EXIT, "none"), Ok(None));
    }

    #[test]
    fn refuses_bad_filters() {
        let bad = |text| Filter::parse(&SYSCALL_EXIT, text).expect_err(text);
        assert_eq!(bad(""), FilterError::Empty);
        assert_eq!(bad("nr == 1 &&"), FilterError::Empty);
        assert_eq!(bad("nr =="), FilterError::Syntax);
        assert_eq!(bad("nr == 1 2"), FilterError::Syntax);
        assert_eq!(bad("pid == 1"), FilterError::UnknownArg);
        assert_eq!(bad("nr ~ 1"), FilterError::UnknownOp);
        assert_eq!(bad("nr == -1"), FilterError::Value);
        assert_eq!(bad("nr == 0x"), FilterError::Value);
        assert_eq!(bad("nr == 18446744073709551616"), FilterError::Value);
        assert_eq!(
            bad("nr == 1 && nr == 2 && nr == 3 && nr == 4 && nr == 5"),
            FilterError::TooMany
        );
    }

    #[test]
    fn admits_only_matching_records() {
        let filter = parse(&SYSCALL_EXIT, "nr == 1 && ret != 0");
        assert!(filter.admits(&[1, 7]));
        assert!(!filter.admits(&[1, 0]));
        assert!(!filter.admits(&[2, 7]));
        // A missing argument fails its condition.
        assert!(!filter.admits(&[1]));
    }

    #[test]
    fn slots_store_load_and_clear() {
        let slot = Slot::new();
        assert!(slot.admits(&[0, 0]));
        assert_eq!(slot.load(), Loaded::Empty);
        let filter = parse(&SCHED_SWITCH, "next_pid == 9");
        slot.store(Some(&filter));
        assert_eq!(slot.load(), Loaded::Set(filter));
        let next_pid = SCHED_SWITCH
            .args()
            .iter()
            .position(|&a| a == "next_pid")
            .expect("next_pid");
        let mut args = [0u64; 8];
        args[next_pid] = 9;
        assert!(slot.admits(&args[..SCHED_SWITCH.args().len()]));
        args[next_pid] = 8;
        assert!(!slot.admits(&args[..SCHED_SWITCH.args().len()]));
        slot.store(None);
        assert!(slot.admits(&args[..SCHED_SWITCH.args().len()]));
        assert_eq!(slot.load(), Loaded::Empty);
    }

    #[test]
    fn a_store_in_progress_reads_as_busy() {
        let slot = Slot::new();
        let filter = parse(&SYSCALL_EXIT, "nr == 1");
        slot.store(Some(&filter));
        // What a store looks like to a reader between its two sequence updates.
        let settled = slot.sequence.load(Ordering::Relaxed);
        slot.sequence.store(settled + 1, Ordering::Relaxed);
        assert_eq!(slot.load(), Loaded::Busy);
        // The record passes: a filter never removes what it could not read.
        assert!(slot.admits(&[2, 0]));
        slot.sequence.store(settled + 2, Ordering::Relaxed);
        assert_eq!(slot.load(), Loaded::Set(filter));
        assert!(!slot.admits(&[2, 0]));
    }

    #[test]
    fn readers_never_see_a_mix_of_two_filters() {
        use std::sync::Arc;
        let slot = Arc::new(Slot::new());
        let a = parse(&SYSCALL_EXIT, "nr == 1 && ret == 1");
        let b = parse(&SYSCALL_EXIT, "nr == 2 && ret == 2 && nr != 3");
        slot.store(Some(&a));
        let writer = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || {
                for i in 0..20_000 {
                    slot.store(Some(if i % 2 == 0 { &b } else { &a }));
                }
            })
        };
        for _ in 0..20_000 {
            match slot.load() {
                Loaded::Set(seen) => assert!(seen == a || seen == b, "{seen:?}"),
                Loaded::Busy => {}
                Loaded::Empty => panic!("the slot always holds a filter"),
            }
        }
        writer.join().expect("writer");
    }

    #[test]
    fn decode_refuses_garbage() {
        let mut words = [0u64; ENCODED_WORDS];
        assert_eq!(Filter::decode(&words), None);
        words[0] = 5;
        assert_eq!(Filter::decode(&words), None);
        words[0] = 1;
        words[1] = 0xff << 8;
        assert_eq!(Filter::decode(&words), None);
    }
}
