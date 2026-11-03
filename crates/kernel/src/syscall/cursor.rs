//! Where a `write` leaves a positioned description's cursor.
//!
//! The kernel keeps the cursor of a positioned description (POSIX ties it to the
//! description, not the descriptor) and passes it to the scheme as the write offset. With
//! `O_APPEND` the scheme ignores that offset and writes at the end of the file, but its
//! reply carries only a byte count. POSIX (`write`, and `open`'s `O_APPEND`) puts the
//! cursor at the end of the appended bytes, so after an append the kernel asks the scheme
//! for the file's size (`fsize`) and moves the cursor there, rather than adding the byte
//! count to an offset the scheme did not use.
//!
//! Only a scheme that said at open that it appends ([`APPENDS`] in its reply) gets this:
//! others, such as raw disks, write at the offset they are given whatever the flags, and
//! their `fsize` (a disk's capacity) is not where the write ended, so their cursor advances
//! by the count. A write of no bytes changes nothing, so it leaves the cursor where it was
//! (POSIX `write`: "no other results").
//!
//! The size is read in a second request, after the write's reply, because the scheme
//! write response has no room for the offset it wrote at (a [`Cqe`] holds one 64-bit
//! result) and the description lock cannot be held across scheme calls. The scheme serves
//! the two requests in order, so the size includes this write. If another description
//! changes the file in between (another append, or a truncation), the cursor lands at that
//! later end of file rather than at the end of this write, and two threads appending
//! through one description at once can leave the cursor at the end the earlier one saw.
//! The data itself is placed by the scheme and is not affected. An exact position needs a
//! write response that carries it, which would change the scheme protocol.
//!
//! [`Cqe`]: syscall::schemev2::Cqe
//! [`APPENDS`]: ferric_abi::new_fd::APPENDS

use syscall::flag::O_APPEND;

/// How a successful `write` moves its description's cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorUpdate {
    /// The description is not positioned: the scheme keeps any position itself.
    Unpositioned,
    /// The scheme wrote at the cursor: it advances by the bytes written.
    Advance,
    /// The scheme appended (`O_APPEND`): the cursor moves to the end of the file after the
    /// write, as the scheme reports it.
    ToEnd,
}

impl CursorUpdate {
    /// The update for a `write` on a description with open/`F_SETFL` `flags`, which is
    /// `positioned`, and whose scheme `appends` (writes `O_APPEND` writes at the end).
    pub fn for_write(positioned: bool, appends: bool, flags: u32) -> Self {
        if !positioned {
            Self::Unpositioned
        } else if appends && flags & O_APPEND as u32 != 0 {
            Self::ToEnd
        } else {
            Self::Advance
        }
    }

    /// Whether to ask the scheme for the end of the file after `written` bytes were
    /// written: only after an append that wrote something.
    pub fn needs_end(self, written: usize) -> bool {
        self == Self::ToEnd && written != 0
    }

    /// The cursor after `written` bytes were written with the cursor at `cursor`.
    ///
    /// `end` is the file size the scheme reported after an append ([`Self::ToEnd`]), or
    /// `None` when it was not asked or could not report one (no `fsize`, or the request
    /// failed). The write itself succeeded either way, so without a size the cursor
    /// advances by `written`, as it did before appends were tracked.
    pub fn apply(self, cursor: u64, written: usize, end: Option<u64>) -> u64 {
        match (self, end) {
            (Self::Unpositioned, _) => cursor,
            (Self::ToEnd, Some(end)) if written != 0 => end,
            (Self::Advance | Self::ToEnd, _) => {
                cursor.saturating_add(u64::try_from(written).unwrap_or(u64::MAX))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CursorUpdate;
    use syscall::flag::{O_APPEND, O_RDWR, O_WRONLY};

    #[test]
    fn unpositioned_descriptions_keep_their_cursor() {
        let update = CursorUpdate::for_write(false, true, (O_WRONLY | O_APPEND) as u32);
        assert_eq!(update, CursorUpdate::Unpositioned);
        assert_eq!(update.apply(7, 4, Some(20)), 7);
    }

    #[test]
    fn plain_writes_advance_by_the_bytes_written() {
        let update = CursorUpdate::for_write(true, true, O_RDWR as u32);
        assert_eq!(update, CursorUpdate::Advance);
        assert_eq!(update.apply(6, 4, None), 10);
        assert_eq!(update.apply(u64::MAX - 1, 4, None), u64::MAX);
    }

    #[test]
    fn appends_move_to_the_reported_end_of_file() {
        // A 16-byte file appended to from cursor 0: the bytes land at 16..20.
        let update = CursorUpdate::for_write(true, true, (O_RDWR | O_APPEND) as u32);
        assert_eq!(update, CursorUpdate::ToEnd);
        assert_eq!(update.apply(0, 4, Some(20)), 20);
        // A cursor past the end (after an lseek) also returns to the end.
        assert_eq!(update.apply(100, 4, Some(20)), 20);
    }

    #[test]
    fn appends_without_a_reported_size_advance_by_the_bytes_written() {
        let update = CursorUpdate::for_write(true, true, (O_WRONLY | O_APPEND) as u32);
        assert_eq!(update.apply(3, 4, None), 7);
    }

    /// A raw disk writes at the offset it is given, even with `O_APPEND`, and its size is
    /// its capacity: the cursor must follow the write, not jump to the capacity.
    #[test]
    fn appends_on_schemes_that_do_not_append_advance_by_the_bytes_written() {
        let update = CursorUpdate::for_write(true, false, (O_RDWR | O_APPEND) as u32);
        assert_eq!(update, CursorUpdate::Advance);
        assert!(!update.needs_end(512));
        assert_eq!(update.apply(0, 512, None), 512);
    }

    #[test]
    fn empty_appends_leave_the_cursor_where_it_was() {
        let update = CursorUpdate::for_write(true, true, (O_RDWR | O_APPEND) as u32);
        assert!(!update.needs_end(0));
        assert!(update.needs_end(1));
        assert_eq!(update.apply(3, 0, None), 3);
        assert_eq!(update.apply(3, 0, Some(20)), 3);
    }
}
