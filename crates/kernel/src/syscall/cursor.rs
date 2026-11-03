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
    /// The update for a `write` on a description with `positioned` and open/`F_SETFL`
    /// `flags`.
    pub fn for_write(positioned: bool, flags: u32) -> Self {
        if !positioned {
            Self::Unpositioned
        } else if flags & O_APPEND as u32 != 0 {
            Self::ToEnd
        } else {
            Self::Advance
        }
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
            (Self::ToEnd, Some(end)) => end,
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
        let update = CursorUpdate::for_write(false, (O_WRONLY | O_APPEND) as u32);
        assert_eq!(update, CursorUpdate::Unpositioned);
        assert_eq!(update.apply(7, 4, Some(20)), 7);
    }

    #[test]
    fn plain_writes_advance_by_the_bytes_written() {
        let update = CursorUpdate::for_write(true, O_RDWR as u32);
        assert_eq!(update, CursorUpdate::Advance);
        assert_eq!(update.apply(6, 4, None), 10);
        assert_eq!(update.apply(u64::MAX - 1, 4, None), u64::MAX);
    }

    #[test]
    fn appends_move_to_the_reported_end_of_file() {
        // A 16-byte file appended to from cursor 0: the bytes land at 16..20.
        let update = CursorUpdate::for_write(true, (O_RDWR | O_APPEND) as u32);
        assert_eq!(update, CursorUpdate::ToEnd);
        assert_eq!(update.apply(0, 4, Some(20)), 20);
        // A cursor past the end (after an lseek) also returns to the end.
        assert_eq!(update.apply(100, 4, Some(20)), 20);
    }

    #[test]
    fn appends_without_a_reported_size_advance_by_the_bytes_written() {
        let update = CursorUpdate::for_write(true, (O_WRONLY | O_APPEND) as u32);
        assert_eq!(update.apply(3, 4, None), 7);
    }
}
