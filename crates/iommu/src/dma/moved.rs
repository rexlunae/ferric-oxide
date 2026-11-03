//! Ordering of DMA releases when memory moves away from an address space (`mremap`).
//!
//! A move takes the source range apart one piece (one grant) at a time, and any piece can
//! fail to move, for example when a page table for its new place cannot be allocated. A piece
//! that fails stays mapped where it was, so the devices of the source's bound domain must keep
//! reaching it: its DMA mappings may be released ([`DmaMappings::release`]) only once it has
//! actually left. [`move_pieces`] enforces that order, so a move that fails part way leaves
//! exactly the moved pieces released and the rest untouched.
//!
//! The kernel's `AddrSpace::r#move` uses it with one piece per source grant; the part of the
//! source that a shrinking move unmaps follows the `munmap` rule instead, and is released
//! whether or not the unmap finished, because memory it removed may already be on its way
//! back to the allocator.
//!
//! [`DmaMappings::release`]: crate::dma::DmaMappings::release
//! [`move_pieces`]: crate::dma::moved::move_pieces

/// Moves `pieces` in order with `move_piece`, and calls `released` with the span each one
/// left from, right after it has moved and before the next one starts.
///
/// `move_piece` returns the span its piece occupied in the source. Pieces after a failed one
/// are not attempted, and `released` is never called for the piece that failed.
///
/// # Errors
///
/// The first error `move_piece` returns; the pieces before it have moved and been released.
///
/// # Examples
///
/// ```
/// use ferric_iommu::dma::{DmaMappings, Mapping, Span, moved::move_pieces};
///
/// // Two buffers reached from 0x1000 and 0x3000; the second cannot be moved.
/// let mut maps = DmaMappings::new();
/// maps.insert(Mapping::new(0x8000_0000, 0x1000, 0x4000, true, Span::new(0x1000, 0x1000), ()))?;
/// maps.insert(Mapping::new(0x9000_0000, 0x1000, 0x5000, true, Span::new(0x3000, 0x1000), ()))?;
///
/// let moved = move_pieces(
///     [0x1000, 0x3000],
///     |start| if start == 0x1000 { Ok(Span::new(start, 0x1000)) } else { Err("no memory") },
///     |left| drop(maps.release(left)),
/// );
/// assert_eq!(moved, Err("no memory"));
/// // Only the buffer that left lost its DMA mapping.
/// assert_eq!(maps.iova_of(0x8000_0000), None);
/// assert_eq!(maps.iova_of(0x9000_0000), Some(0x5000));
/// # Ok::<(), ferric_iommu::dma::Overlap>(())
/// ```
pub fn move_pieces<P, S, E>(
    pieces: impl IntoIterator<Item = P>,
    mut move_piece: impl FnMut(P) -> Result<S, E>,
    mut released: impl FnMut(S),
) -> Result<(), E> {
    for piece in pieces {
        released(move_piece(piece)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec;
    use std::vec::Vec;

    use super::move_pieces;
    use crate::dma::{DmaMappings, Mapping, Span};

    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Moved(u64),
        Released(u64),
    }

    /// Runs a move of pieces `0..count` where piece `fail_at` (if any) fails, and returns the
    /// result with the order of events.
    fn run(count: u64, fail_at: Option<u64>) -> (Result<(), u64>, Vec<Event>) {
        let events = core::cell::RefCell::new(Vec::new());
        let result = move_pieces(
            0..count,
            |piece| {
                if Some(piece) == fail_at {
                    return Err(piece);
                }
                events.borrow_mut().push(Event::Moved(piece));
                Ok(piece)
            },
            |piece| events.borrow_mut().push(Event::Released(piece)),
        );
        (result, events.into_inner())
    }

    #[test]
    fn every_piece_is_released_right_after_it_moves() {
        let (result, events) = run(3, None);
        assert_eq!(result, Ok(()));
        assert_eq!(
            events,
            vec![
                Event::Moved(0),
                Event::Released(0),
                Event::Moved(1),
                Event::Released(1),
                Event::Moved(2),
                Event::Released(2),
            ]
        );
    }

    #[test]
    fn a_failed_piece_and_those_after_it_are_not_released() {
        let (result, events) = run(4, Some(2));
        assert_eq!(result, Err(2));
        assert_eq!(
            events,
            vec![
                Event::Moved(0),
                Event::Released(0),
                Event::Moved(1),
                Event::Released(1),
            ]
        );
    }

    #[test]
    fn a_failed_first_piece_releases_nothing() {
        let (result, events) = run(2, Some(0));
        assert_eq!(result, Err(0));
        assert!(events.is_empty());
    }

    #[test]
    fn no_pieces_is_a_successful_move() {
        assert_eq!(run(0, None), (Ok(()), Vec::new()));
    }

    /// Three one-page grants at 0x1000, 0x2000 and 0x3000, each reached by one DMA mapping.
    fn three_buffers() -> DmaMappings<u64> {
        let mut maps = DmaMappings::new();
        for i in 0..3u64 {
            let user = Span::new(0x1000 * (i + 1), 0x1000);
            maps.insert(Mapping::new(
                0x8000_0000 + 0x1000 * i,
                0x1000,
                0x4000 * (i + 1),
                true,
                user,
                i,
            ))
            .expect("distinct frames");
        }
        maps
    }

    #[test]
    fn a_partial_move_keeps_the_mappings_of_the_memory_that_stayed() {
        let mut maps = three_buffers();
        let mut gone = Vec::new();
        let result = move_pieces(
            [0x1000u64, 0x2000, 0x3000],
            |start| {
                if start == 0x2000 {
                    Err(())
                } else {
                    Ok(Span::new(start, 0x1000))
                }
            },
            |left| gone.extend(maps.release(left).into_iter().map(|m| m.data)),
        );
        assert_eq!(result, Err(()));
        assert_eq!(gone, vec![0]);
        assert_eq!(maps.iova_of(0x8000_0000), None);
        assert_eq!(maps.iova_of(0x8000_1000), Some(0x8000));
        assert_eq!(maps.iova_of(0x8000_2000), Some(0xc000));
    }

    #[test]
    fn a_full_move_releases_what_releasing_the_whole_range_would() {
        let mut piecewise = three_buffers();
        let mut whole = three_buffers();
        let mut gone = Vec::new();
        move_pieces(
            [0x1000u64, 0x2000, 0x3000],
            |start| Ok::<_, ()>(Span::new(start, 0x1000)),
            |left| gone.extend(piecewise.release(left).into_iter().map(|m| m.data)),
        )
        .expect("every piece moves");
        let mut all: Vec<_> = whole
            .release(Span::new(0x1000, 0x3000))
            .into_iter()
            .map(|m| m.data)
            .collect();
        all.sort_unstable();
        assert_eq!(gone, all);
        assert_eq!(piecewise.drain().count(), 0);
    }
}
