//! The per-instance handle table.
//!
//! A guest names its descriptors with small numbers that index this table, never with the
//! runtime's own fd numbers, so it cannot reach the descriptors the runtime keeps for
//! itself (the module file, the code cache, trace and kernel fds). Each instance has its
//! own table, which colocated mode (several instances in one process) also needs. New
//! handles take the lowest free number, as POSIX requires for `open`.

use alloc::vec::Vec;

use crate::errno::Errno;

/// A table of guest fd numbers and the host handles `H` they stand for.
#[derive(Debug, Clone)]
pub struct HandleTable<H> {
    slots: Vec<Option<H>>,
    limit: u32,
}

impl<H> HandleTable<H> {
    /// An empty table that holds at most `limit` handles.
    pub const fn new(limit: u32) -> Self {
        Self {
            slots: Vec::new(),
            limit,
        }
    }

    /// Adds a handle at the lowest free number and returns that number.
    ///
    /// # Errors
    ///
    /// [`Errno::EMFILE`] when the table is full.
    pub fn insert(&mut self, handle: H) -> Result<u32, Errno> {
        if let Some(index) = self.slots.iter().position(Option::is_none) {
            self.slots[index] = Some(handle);
            return u32::try_from(index).map_err(|_| Errno::EMFILE);
        }
        let index = u32::try_from(self.slots.len()).map_err(|_| Errno::EMFILE)?;
        if index >= self.limit {
            return Err(Errno::EMFILE);
        }
        self.slots.push(Some(handle));
        Ok(index)
    }

    /// Puts a handle at `fd`, as `dup2` and `spawn`'s fd list do, and returns the handle it
    /// replaces.
    ///
    /// # Errors
    ///
    /// [`Errno::EBADF`] if `fd` is at or past the limit.
    pub fn insert_at(&mut self, fd: u32, handle: H) -> Result<Option<H>, Errno> {
        if fd >= self.limit {
            return Err(Errno::EBADF);
        }
        let index = usize::try_from(fd).map_err(|_| Errno::EBADF)?;
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        Ok(self.slots[index].replace(handle))
    }

    /// The handle behind `fd`.
    ///
    /// # Errors
    ///
    /// [`Errno::EBADF`] if `fd` names no handle.
    pub fn get(&self, fd: u32) -> Result<&H, Errno> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.slots.get(index))
            .and_then(Option::as_ref)
            .ok_or(Errno::EBADF)
    }

    /// Removes `fd` and returns its handle, for the runtime to close.
    ///
    /// # Errors
    ///
    /// [`Errno::EBADF`] if `fd` names no handle.
    pub fn remove(&mut self, fd: u32) -> Result<H, Errno> {
        let handle = usize::try_from(fd)
            .ok()
            .and_then(|index| self.slots.get_mut(index))
            .and_then(Option::take)
            .ok_or(Errno::EBADF)?;
        while self.slots.last().is_some_and(Option::is_none) {
            self.slots.pop();
        }
        Ok(handle)
    }

    /// The fds in use and their handles, in order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &H)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            let fd = u32::try_from(index).ok()?;
            slot.as_ref().map(|handle| (fd, handle))
        })
    }

    /// How many fds are in use.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// Whether no fd is in use.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_take_the_lowest_free_number() {
        let mut table = HandleTable::new(4);
        assert_eq!(table.insert("stdin"), Ok(0));
        assert_eq!(table.insert("stdout"), Ok(1));
        assert_eq!(table.insert("file"), Ok(2));
        assert_eq!(table.remove(1), Ok("stdout"));
        assert_eq!(table.get(1), Err(Errno::EBADF));
        assert_eq!(table.insert("socket"), Ok(1));
        assert_eq!(table.insert("dir"), Ok(3));
        assert_eq!(table.insert("full"), Err(Errno::EMFILE));
        assert_eq!(table.len(), 4);
        assert_eq!(table.remove(9), Err(Errno::EBADF));
        assert_eq!(table.get(u32::MAX), Err(Errno::EBADF));
    }

    #[test]
    fn fds_can_be_placed() {
        let mut table = HandleTable::new(8);
        assert_eq!(table.insert_at(5, 'a'), Ok(None));
        assert_eq!(table.insert_at(5, 'b'), Ok(Some('a')));
        assert_eq!(table.insert('c'), Ok(0));
        assert_eq!(table.insert_at(8, 'd'), Err(Errno::EBADF));
        let fds: Vec<(u32, char)> = table.iter().map(|(fd, handle)| (fd, *handle)).collect();
        assert_eq!(fds, [(0, 'c'), (5, 'b')]);
        assert_eq!(table.remove(5), Ok('b'));
        assert_eq!(table.remove(0), Ok('c'));
        assert!(table.is_empty());
    }
}
