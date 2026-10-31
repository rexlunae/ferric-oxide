//! Command identifiers held until completion, independently of submission-ring positions.
//!
//! NVMe Base Specification 2.0c, section 3.3.3.2, Figures 90–91 distinguishes the
//! submission head (entries the controller consumed) from a completed command's identifier.
//! A consumed entry may be reused while its command remains outstanding. Each queue pair
//! therefore needs its own identifier pool, released only when it consumes a completion.
//! See <https://nvmexpress.org/wp-content/uploads/NVM-Express-Base-Specification-2.0c-2022.10.04-Ratified.pdf>.

const WORD_BITS: usize = u64::BITS as usize;
const ID_COUNT: usize = u16::MAX as usize + 1;

/// A queue's 16-bit command identifiers, reserved until their completions arrive.
///
/// The bitmap uses 8 KiB and performs no allocation. At most 65,535 identifiers may
/// be outstanding at once, as required by [NVMe Base Specification 2.1], section 4.2.1.
///
/// [NVMe Base Specification 2.1]: https://nvmexpress.org/wp-content/uploads/NVM-Express-Base-Specification-Revision-2.1-2024.08.05-Ratified.pdf
///
/// # Examples
///
/// ```
/// use ferric_blkqueue::command_ids::CommandIds;
///
/// let mut ids = CommandIds::new();
/// let first = ids.allocate().unwrap();
/// let second = ids.allocate().unwrap();
/// assert_ne!(first, second);
/// assert!(ids.release(first));
/// assert!(ids.release(second));
/// ```
pub struct CommandIds {
    occupied: [u64; ID_COUNT / WORD_BITS],
    next: u16,
    in_flight: usize,
}

impl CommandIds {
    /// Creates an empty identifier pool for one queue pair.
    pub const fn new() -> Self {
        Self {
            occupied: [0; ID_COUNT / WORD_BITS],
            next: 0,
            in_flight: 0,
        }
    }

    /// Reserves an unused identifier, or returns `None` at the outstanding-command limit.
    pub fn allocate(&mut self) -> Option<u16> {
        if self.in_flight == u16::MAX as usize {
            return None;
        }
        loop {
            let id = self.next;
            self.next = self.next.wrapping_add(1);
            let word = &mut self.occupied[usize::from(id) / WORD_BITS];
            let bit = 1 << (usize::from(id) % WORD_BITS);
            if *word & bit == 0 {
                *word |= bit;
                self.in_flight += 1;
                return Some(id);
            }
        }
    }

    /// Releases a completed identifier, returning whether it was outstanding.
    ///
    /// A `false` result indicates an unknown or duplicate completion. Submission-ring
    /// consumption and cancellation of a waiting future must not release an identifier.
    pub fn release(&mut self, id: u16) -> bool {
        let word = &mut self.occupied[usize::from(id) / WORD_BITS];
        let bit = 1 << (usize::from(id) % WORD_BITS);
        if *word & bit == 0 {
            return false;
        }
        *word &= !bit;
        self.in_flight -= 1;
        true
    }
}

impl Default for CommandIds {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_submission_slots_do_not_release_commands() {
        let mut ids = CommandIds::new();
        let outstanding: [u16; 7] = core::array::from_fn(|_| ids.allocate().unwrap());
        // A controller may fetch all seven entries, then complete only the last.
        assert!(ids.release(outstanding[6]));
        // Wrap an eight-entry submission ring while its first six commands still wait.
        // Using the submission tail as the CID would overwrite those commands' waiters.
        for _ in 0..24 {
            let id = ids.allocate().unwrap();
            assert!(!outstanding[..6].contains(&id));
            assert!(ids.release(id));
        }
        for id in &outstanding[..6] {
            assert!(ids.release(*id));
        }
    }

    #[test]
    fn identifier_wrap_skips_an_outstanding_command() {
        let mut ids = CommandIds::new();
        let retained = ids.allocate().unwrap();
        for expected in 1..ID_COUNT {
            let id = ids.allocate().unwrap();
            assert_eq!(usize::from(id), expected);
            assert!(ids.release(id));
        }
        assert_eq!(ids.allocate(), Some(1));
        assert!(ids.release(retained));
    }

    #[test]
    fn exhausted_pool_resumes_after_out_of_order_completion() {
        let mut ids = CommandIds::new();
        for expected in 0..u16::MAX {
            assert_eq!(ids.allocate(), Some(expected));
        }
        assert_eq!(ids.allocate(), None);
        assert!(ids.release(31));
        assert_eq!(ids.allocate(), Some(u16::MAX));
        assert_eq!(ids.allocate(), None);
        assert!(ids.release(17));
        assert_eq!(ids.allocate(), Some(17));
        assert_eq!(ids.allocate(), None);
    }

    #[test]
    fn unknown_and_duplicate_completions_leave_pool_unchanged() {
        let mut ids = CommandIds::new();
        assert!(!ids.release(0));
        let id = ids.allocate().unwrap();
        assert!(ids.release(id));
        assert!(!ids.release(id));
        assert_eq!(ids.in_flight, 0);
    }

    #[test]
    fn queue_pairs_have_independent_identifiers() {
        let mut first = CommandIds::new();
        let mut second = CommandIds::new();
        assert_eq!(first.allocate(), Some(0));
        assert_eq!(second.allocate(), Some(0));
        assert!(first.release(0));
        assert!(second.release(0));
    }
}
