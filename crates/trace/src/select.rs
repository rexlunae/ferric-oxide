//! Event sets and the selection syntax used by the boot environment and the trace scheme.
//!
//! A selection is a list of items separated by commas or whitespace. Each item is `all`, a
//! subsystem name (`syscall`: all of its events) or an event name (`sched.switch`). An empty
//! selection selects nothing.

use core::fmt;

use crate::{Catalog, EventId, MAX_EVENTS, Subsystem};

const SET_WORDS: usize = MAX_EVENTS / 64;

/// A set of event ids.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EventSet([u64; SET_WORDS]);

impl EventSet {
    /// The empty set.
    pub const fn new() -> Self {
        Self([0; SET_WORDS])
    }

    /// Adds `id`. Ids beyond [`MAX_EVENTS`] cannot be in a catalog and are ignored.
    pub fn insert(&mut self, id: EventId) {
        if let Some(word) = self.0.get_mut(id.index() / 64) {
            *word |= 1 << (id.index() % 64);
        }
    }

    /// Removes `id`.
    pub fn remove(&mut self, id: EventId) {
        if let Some(word) = self.0.get_mut(id.index() / 64) {
            *word &= !(1 << (id.index() % 64));
        }
    }

    /// Whether `id` is in the set.
    pub fn contains(&self, id: EventId) -> bool {
        self.0
            .get(id.index() / 64)
            .is_some_and(|word| word & (1 << (id.index() % 64)) != 0)
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&word| word == 0)
    }

    /// The ids in the set, in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = EventId> + '_ {
        (0..MAX_EVENTS)
            .map(|i| EventId::new(i as u16))
            .filter(|&id| self.contains(id))
    }

    /// The union of `self` and `other`.
    pub fn union(mut self, other: &EventSet) -> EventSet {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
        self
    }

    /// The events in both `self` and `other`.
    pub fn intersection(mut self, other: &EventSet) -> EventSet {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a &= b;
        }
        self
    }

    /// The number of events in the set.
    pub fn len(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }
}

/// A selection item that is neither `all`, a subsystem nor an event of the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectError<'a> {
    item: &'a str,
}

impl<'a> SelectError<'a> {
    /// The unknown item.
    pub fn item(&self) -> &'a str {
        self.item
    }
}

impl fmt::Display for SelectError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown trace event or subsystem {:?} (expected all, a subsystem or an event name)",
            self.item
        )
    }
}

impl core::error::Error for SelectError<'_> {}

/// The events of `catalog` that `spec` selects.
///
/// # Errors
///
/// [`SelectError`] naming the first item that matches nothing.
///
/// # Examples
///
/// ```
/// use ferric_trace::{kernel, select};
///
/// let set = select(&kernel::CATALOG, "syscall, sched.switch").expect("known names");
/// assert!(set.contains(kernel::SYSCALL_EXIT.id()));
/// assert!(select(&kernel::CATALOG, "sched.nothing").is_err());
/// ```
pub fn select<'a>(catalog: &Catalog, spec: &'a str) -> Result<EventSet, SelectError<'a>> {
    let mut set = EventSet::new();
    for item in spec
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|item| !item.is_empty())
    {
        let subsystem = Subsystem::from_name(item);
        let mut matched = false;
        for event in catalog.events() {
            if item == "all" || Some(event.subsystem()) == subsystem || event.name() == item {
                set.insert(event.id());
                matched = true;
            }
        }
        // A known subsystem with no events in this catalog is not an error.
        if !matched && item != "all" && subsystem.is_none() {
            return Err(SelectError { item });
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{CATALOG, SCHED_SWITCH, SYSCALL_ENTER, SYSCALL_EXIT};
    use std::vec::Vec;

    #[test]
    fn set_operations() {
        let mut set = EventSet::new();
        assert!(set.is_empty());
        set.insert(EventId::new(0));
        set.insert(EventId::new(200));
        set.insert(EventId::new(u16::MAX));
        assert!(set.contains(EventId::new(200)));
        assert!(!set.contains(EventId::new(u16::MAX)));
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [EventId::new(0), EventId::new(200)]
        );
        set.remove(EventId::new(0));
        let mut other = EventSet::new();
        other.insert(EventId::new(64));
        let union = set.union(&other);
        assert_eq!(
            union.iter().collect::<Vec<_>>(),
            [EventId::new(64), EventId::new(200)]
        );
        assert_eq!(union.len(), 2);
        assert_eq!(
            union.intersection(&other).iter().collect::<Vec<_>>(),
            [EventId::new(64)]
        );
        assert!(set.intersection(&other).is_empty());
    }

    #[test]
    fn selections() {
        let ids = |spec| {
            select(&CATALOG, spec)
                .expect("valid")
                .iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(""), []);
        assert_eq!(ids(" , "), []);
        assert_eq!(ids("syscall"), [SYSCALL_ENTER.id(), SYSCALL_EXIT.id()]);
        assert_eq!(ids("sched.switch"), [SCHED_SWITCH.id()]);
        assert_eq!(ids("all").len(), CATALOG.len());
        assert_eq!(
            ids("syscall.exit sched\tsyscall.exit"),
            [
                SYSCALL_EXIT.id(),
                SCHED_SWITCH.id(),
                crate::kernel::SCHED_FUTEX_WAIT.id(),
                crate::kernel::SCHED_FUTEX_RESUME.id(),
                crate::kernel::SCHED_FUTEX_WAKE.id(),
                crate::kernel::SCHED_CONTEXT_CREATE.id(),
                crate::kernel::SCHED_CONTEXT_START.id(),
                crate::kernel::SCHED_CONTEXT_EXIT.id(),
                crate::kernel::SCHED_PROC_FORK.id(),
                crate::kernel::SCHED_PROC_EXEC.id(),
                crate::kernel::SCHED_PROC_EXIT.id(),
                crate::kernel::SCHED_PROC_WAIT.id(),
                crate::kernel::SCHED_TICK.id(),
                crate::kernel::SCHED_IPI_PIT.id(),
                crate::kernel::SCHED_IPI_WAKE.id(),
            ]
        );
    }

    #[test]
    fn unknown_items() {
        let err = select(&CATALOG, "syscall,mm.fault").expect_err("unknown");
        assert_eq!(err.item(), "mm.fault");
        assert!(select(&CATALOG, "syscall.").is_err());
    }
}
