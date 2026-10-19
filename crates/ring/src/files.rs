//! A ring's file table: the slots that SQEs with
//! [`SqeFlags::FIXED_FILE`](crate::SqeFlags::FIXED_FILE) name instead of descriptors
//! ([`Request::RegisterFiles`](crate::abi::Request::RegisterFiles)).
//!
//! [`FileTable`] is generic over what a slot holds, so the kernel keeps its file descriptions
//! in it and the host tests keep plain values. It never drops what it holds on its own: every
//! file leaves the table through a return value ([`FileTable::unregister`],
//! [`FileTable::take_all`], or [`Refused::files`] when a registration fails), because closing a
//! file description in the kernel needs a lock token that `Drop` does not have. Only built with
//! the `alloc` feature.

use alloc::vec::Vec;
use core::fmt;

use crate::abi::MAX_FIXED_FILES;

/// Why the table refused a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableError {
    /// The slots run past [`MAX_FIXED_FILES`] (`EINVAL`).
    Range {
        /// The first slot.
        first: u32,
        /// How many slots.
        count: usize,
    },
    /// The slot already holds a file (`EBUSY`).
    Occupied {
        /// The slot.
        slot: u32,
    },
    /// The table could not grow (`ENOMEM`).
    NoMemory,
    /// A registration with no files (`EINVAL`), which would report success for nothing.
    Empty,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range { first, count } => write!(
                f,
                "{count} slots from {first} run past the table's {MAX_FIXED_FILES}"
            ),
            Self::Occupied { slot } => write!(f, "slot {slot} already holds a file"),
            Self::NoMemory => f.write_str("the file table could not grow"),
            Self::Empty => f.write_str("a registration needs at least one file"),
        }
    }
}

impl core::error::Error for TableError {}

/// A registration the table refused, with the files it was given, so the caller can close
/// them.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused<T> {
    /// Why.
    pub error: TableError,
    /// The files, none of which was registered.
    pub files: Vec<T>,
}

/// The slots of one ring.
#[derive(Debug)]
pub struct FileTable<T> {
    /// Grows to the highest slot registered so far.
    slots: Vec<Option<T>>,
}

impl<T> Default for FileTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// The slots `first..first + count` as indices, if they are all below [`MAX_FIXED_FILES`].
fn range(first: u32, count: usize) -> Result<core::ops::Range<usize>, TableError> {
    let start = first as usize;
    match start.checked_add(count) {
        Some(end) if end <= MAX_FIXED_FILES as usize => Ok(start..end),
        _ => Err(TableError::Range { first, count }),
    }
}

impl<T> FileTable<T> {
    /// An empty table.
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// The file in `slot`, if it holds one.
    pub fn get(&self, slot: u32) -> Option<&T> {
        self.slots.get(slot as usize)?.as_ref()
    }

    /// How many slots hold a file.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// Whether no slot holds a file.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// Puts `files` into the slots from `first` on, all of them or none.
    ///
    /// # Errors
    ///
    /// A [`Refused`] holding every one of `files`: [`TableError::Range`] when the slots run past
    /// [`MAX_FIXED_FILES`], [`TableError::Occupied`] when one of them holds a file,
    /// [`TableError::NoMemory`] when the table cannot grow, and [`TableError::Empty`] when
    /// `files` is empty.
    pub fn register(&mut self, first: u32, files: Vec<T>) -> Result<(), Refused<T>> {
        if let Err(error) = self.check_register(first, files.len()) {
            return Err(Refused { error, files });
        }
        let start = first as usize;
        for (i, file) in files.into_iter().enumerate() {
            // `check_register` grew the table to cover every slot.
            if let Some(slot) = self.slots.get_mut(start + i) {
                *slot = Some(file);
            }
        }
        Ok(())
    }

    /// Checks that `count` slots from `first` are free, and grows the table to hold them.
    fn check_register(&mut self, first: u32, count: usize) -> Result<(), TableError> {
        if count == 0 {
            return Err(TableError::Empty);
        }
        let slots = range(first, count)?;
        if let Some(i) = slots.clone().find(|&i| self.get(i as u32).is_some()) {
            return Err(TableError::Occupied { slot: i as u32 });
        }
        if slots.end > self.slots.len() {
            self.slots
                .try_reserve_exact(slots.end - self.slots.len())
                .map_err(|_| TableError::NoMemory)?;
            self.slots.resize_with(slots.end, || None);
        }
        Ok(())
    }

    /// Empties `count` slots from `first` on and returns the files they held, in slot order.
    /// Empty slots are skipped.
    ///
    /// # Errors
    ///
    /// [`TableError::Range`] when the slots run past [`MAX_FIXED_FILES`]; nothing is removed
    /// then.
    pub fn unregister(&mut self, first: u32, count: u32) -> Result<Vec<T>, TableError> {
        let slots = range(first, count as usize)?;
        let end = slots.end.min(self.slots.len());
        let start = slots.start.min(end);
        Ok(self.slots[start..end]
            .iter_mut()
            .filter_map(Option::take)
            .collect())
    }

    /// Empties every slot and returns the files, in slot order, as when the ring is closed.
    pub fn take_all(&mut self) -> Vec<T> {
        core::mem::take(&mut self.slots)
            .into_iter()
            .flatten()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn files_register_and_resolve() {
        let mut table = FileTable::new();
        assert!(table.is_empty());
        table.register(2, vec!['a', 'b']).expect("free slots");
        assert_eq!(table.get(1), None);
        assert_eq!(table.get(2), Some(&'a'));
        assert_eq!(table.get(3), Some(&'b'));
        assert_eq!(table.get(4), None);
        assert_eq!(table.get(u32::MAX), None);
        assert_eq!(table.len(), 2);
        table.register(0, vec!['c']).expect("a free slot below");
        assert_eq!(table.get(0), Some(&'c'));
    }

    #[test]
    fn a_refused_registration_hands_every_file_back() {
        let mut table = FileTable::new();
        table.register(4, vec![1]).expect("free");
        assert_eq!(
            table.register(3, vec![10, 11, 12]),
            Err(Refused {
                error: TableError::Occupied { slot: 4 },
                files: vec![10, 11, 12],
            })
        );
        assert_eq!(table.get(3), None);
        assert_eq!(table.get(4), Some(&1));

        let last = MAX_FIXED_FILES - 1;
        table.register(last, vec![7]).expect("the last slot");
        assert_eq!(
            table.register(last, vec![8, 9]),
            Err(Refused {
                error: TableError::Range {
                    first: last,
                    count: 2
                },
                files: vec![8, 9],
            })
        );
        assert_eq!(
            table.register(0, Vec::new()),
            Err(Refused {
                error: TableError::Empty,
                files: Vec::new(),
            })
        );
        assert_eq!(
            table.register(u32::MAX, vec![5]).map_err(|r| r.error),
            Err(TableError::Range {
                first: u32::MAX,
                count: 1
            })
        );
    }

    #[test]
    fn unregister_returns_what_the_slots_held() {
        let mut table = FileTable::new();
        table.register(0, vec![1, 2, 3]).expect("free");
        table.register(6, vec![7]).expect("free");
        assert_eq!(table.unregister(1, 10), Ok(vec![2, 3, 7]));
        assert_eq!(table.get(0), Some(&1));
        assert_eq!(table.unregister(1, 10), Ok(vec![]));
        assert_eq!(table.unregister(500, 3), Ok(vec![]));
        assert_eq!(
            table.unregister(MAX_FIXED_FILES, 1),
            Err(TableError::Range {
                first: MAX_FIXED_FILES,
                count: 1
            })
        );
        assert_eq!(table.unregister(0, 0), Ok(vec![]));
        // An emptied slot takes a file again.
        table.register(2, vec![9]).expect("emptied");
        assert_eq!(table.take_all(), vec![1, 9]);
        assert!(table.is_empty());
        assert_eq!(table.get(0), None);
    }
}
