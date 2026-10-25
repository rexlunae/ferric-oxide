//! Initialized, fixed-size records transferred by kernel wait queues.
//!
//! Only the ABI types with a padding-free representation implement `QueueRecord`.
//! Keeping conversion here prevents a generic queue from exposing padding or
//! uninitialized bytes of arbitrary Rust values to user space.

use alloc::collections::VecDeque;
use syscall::{schemev2::Sqe, Event};

/// A record with a fully initialized byte representation of `size_of::<Self>()`.
pub(crate) trait QueueRecord: Sized {
    /// Borrows the contiguous, initialized bytes of a record slice.
    fn as_bytes(records: &[Self]) -> &[u8];
}

impl QueueRecord for u8 {
    fn as_bytes(records: &[Self]) -> &[u8] {
        records
    }
}

macro_rules! abi_record {
    ($record:ty, $size:expr) => {
        // Both types are repr(C). These field-size sums verify that their layout
        // has no padding; their integer/bitflag fields are all initialized.
        const _: () = assert!(size_of::<$record>() == $size);

        impl QueueRecord for $record {
            fn as_bytes(records: &[Self]) -> &[u8] {
                // SAFETY: The ABI record has no padding (checked above), all bytes
                // are initialized, u8 needs no alignment, and the result borrows
                // exactly the memory covered by the input slice.
                unsafe {
                    core::slice::from_raw_parts(records.as_ptr().cast(), size_of_val(records))
                }
            }
        }
    };
}

abi_record!(Event, 3 * size_of::<usize>());
abi_record!(Sqe, 1 + 1 + 2 + 4 + 6 * 8 + 8);

/// Borrows the longest FIFO prefix of whole records fitting in `capacity` bytes.
///
/// The two byte slices preserve `VecDeque`'s wrap point and require no allocation
/// or per-record copying. A zero-sized record cannot produce any bytes.
fn record_prefix<T: QueueRecord>(records: &VecDeque<T>, capacity: usize) -> (&[u8], &[u8]) {
    let count = capacity.checked_div(size_of::<T>()).unwrap_or(0);
    let (first, second) = records.as_slices();
    let first_count = count.min(first.len());
    let second_count = count.saturating_sub(first_count).min(second.len());
    (
        T::as_bytes(first.split_at(first_count).0),
        T::as_bytes(second.split_at(second_count).0),
    )
}

/// Copies a whole-record prefix and removes it only after both segments succeed.
///
/// `copy` receives the destination byte offset and must copy the entire slice
/// or return an error. Empty segments are not passed to it.
///
/// # Errors
///
/// Propagates the copy error without removing any queued records. Bytes written
/// by an earlier successful segment may already be visible at the destination.
pub(crate) fn copy_records<T: QueueRecord, E>(
    records: &mut VecDeque<T>,
    capacity: usize,
    mut copy: impl FnMut(usize, &[u8]) -> Result<(), E>,
) -> Result<usize, E> {
    let (first, second) = record_prefix(records, capacity);
    if !first.is_empty() {
        copy(0, first)?;
    }
    if !second.is_empty() {
        copy(first.len(), second)?;
    }
    // The byte slices together cover a prefix of an existing queue allocation.
    let bytes = first.len().saturating_add(second.len());
    let count = bytes.checked_div(size_of::<T>()).unwrap_or(0);
    records.drain(..count);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn event(id: usize) -> Event {
        Event {
            id,
            flags: syscall::flag::EventFlags::EVENT_READ,
            data: id + 10,
        }
    }

    #[test]
    fn event_bytes_match_the_abi() {
        let events = [event(1), event(2)];
        let expected: Vec<u8> = events
            .iter()
            .flat_map(|e| {
                [e.id, e.flags.bits(), e.data]
                    .into_iter()
                    .flat_map(usize::to_ne_bytes)
            })
            .collect();
        assert_eq!(Event::as_bytes(&events), expected);
    }

    #[test]
    fn sqe_bytes_match_the_abi() {
        let sqe = Sqe {
            opcode: 7,
            sqe_flags: syscall::schemev2::SqeFlags::empty(),
            _rsvd: 0,
            tag: 0x12345678,
            args: [1, 2, 3, 4, 5, 6],
            caller: 42,
        };
        let mut expected = alloc::vec![7, 0, 0, 0];
        expected.extend(sqe.tag.to_ne_bytes());
        for word in sqe.args.into_iter().chain([sqe.caller]) {
            expected.extend(word.to_ne_bytes());
        }
        assert_eq!(Sqe::as_bytes(&[sqe]), expected);
    }

    #[test]
    fn prefix_never_splits_a_record() {
        let records = VecDeque::from([event(1), event(2), event(3)]);
        let size = size_of::<Event>();
        for capacity in 0..=size * 4 {
            let (first, second) = record_prefix(&records, capacity);
            let bytes: Vec<_> = first.iter().chain(second).copied().collect();
            let count = (capacity / size).min(records.len());
            assert_eq!(bytes.len(), count * size);
            let expected: Vec<Event> = records.iter().copied().take(count).collect();
            assert_eq!(bytes, Event::as_bytes(&expected));
        }
    }

    #[test]
    fn wrapped_queue_preserves_fifo_order_at_every_capacity() {
        let records = wrapped_events();
        let expected: Vec<_> = records.iter().copied().collect();
        for capacity in 0..=size_of::<Event>() * 5 {
            let (first, second) = record_prefix(&records, capacity);
            let bytes: Vec<_> = first.iter().chain(second).copied().collect();
            let count = (capacity / size_of::<Event>()).min(records.len());
            assert_eq!(bytes, Event::as_bytes(&expected[..count]));
        }
    }

    #[test]
    fn byte_and_empty_queues() {
        let queue = VecDeque::from([1_u8, 2, 3]);
        let (first, second) = record_prefix(&queue, 2);
        assert_eq!(first, [1, 2]);
        assert!(second.is_empty());
        let empty = VecDeque::<Sqe>::new();
        assert_eq!(record_prefix(&empty, usize::MAX), (&[][..], &[][..]));
    }

    fn wrapped_events() -> VecDeque<Event> {
        let mut records = VecDeque::with_capacity(4);
        for id in 0..4 {
            records.push_back(event(id));
        }
        records.pop_front();
        records.pop_front();
        records.push_back(event(4));
        records.push_back(event(5));
        assert!(!records.as_slices().1.is_empty());
        records
    }

    #[test]
    fn successful_copy_dequeues_only_whole_records() {
        let mut records = wrapped_events();
        let mut output = Vec::new();
        let copied = copy_records(&mut records, 3 * size_of::<Event>() + 1, |offset, bytes| {
            assert_eq!(offset, output.len());
            output.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(copied, 3 * size_of::<Event>());
        assert_eq!(output, Event::as_bytes(&[event(2), event(3), event(4)]));
        assert_eq!(records.len(), 1);
        assert_eq!(records.front().unwrap().id, 5);
    }

    #[test]
    fn failed_copy_keeps_every_record_even_after_a_successful_segment() {
        for fail_at in 0..2 {
            let mut records = wrapped_events();
            let expected: Vec<_> = records.iter().map(|event| event.id).collect();
            let mut calls = 0;
            let result = copy_records(&mut records, usize::MAX, |_, _| {
                let call = calls;
                calls += 1;
                if call == fail_at {
                    Err(())
                } else {
                    Ok(())
                }
            });
            assert_eq!(result, Err(()));
            assert_eq!(calls, fail_at + 1);
            assert_eq!(
                records.iter().map(|event| event.id).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn short_and_empty_reads_do_not_invoke_copy() {
        let mut records = wrapped_events();
        for capacity in [0, size_of::<Event>() - 1] {
            let bytes = copy_records(&mut records, capacity, |_, _| -> Result<(), ()> {
                panic!("no whole record fits")
            })
            .unwrap();
            assert_eq!(bytes, 0);
            assert_eq!(records.len(), 4);
        }
    }
}
