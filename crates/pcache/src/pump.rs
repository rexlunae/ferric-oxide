//! What a file system's pump does with a batch of records it could not hand on.
//!
//! The pump (`client::spawn_pump` on Redox) takes records off the queue and forwards them to
//! the file system. Taking a record off the queue is final: the kernel does not queue it
//! again, and the readers waiting for its pages wait until the fill is answered or its
//! object or queue closes. So a batch the pump fails to forward must still be answered.
//! [`fail_fills`] answers every fill in it as failed with [`EIO`], through the same answer
//! the file system gives a fill it cannot read ([`crate::abi::Request::Filled`] with
//! [`crate::abi::Outcome::Failed`]). The readers get `EIO` and the next read fills again.
//! When even that answer fails, the pump closes the queue, which fails every fill left.
//!
//! The logic is here, apart from the Redox calls, so it is tested on the host.

use core::fmt;
use core::num::NonZeroU16;

use crate::abi::{Generation, ObjectId, Record, RecordError, RecordPages};

/// `EIO`, the errno a fill the pump could not forward fails with.
pub const EIO: NonZeroU16 = NonZeroU16::new(5).expect("5 is not zero");

/// A fill to answer as failed: copied from its record, with the errno to fail it with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailedFill {
    /// The object the record named.
    pub object: ObjectId,
    /// The record's generation, for the answer.
    pub generation: Generation,
    /// The pages the record named.
    pub pages: RecordPages,
    /// The errno to fail the fill with.
    pub errno: NonZeroU16,
}

/// Why [`fail_fills`] could not answer every fill of a batch.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailFillsError<E> {
    /// A record of the batch could not be decoded, so its fill (if it was one) is unanswered.
    Record(RecordError),
    /// The answer to the fill of `object` failed.
    Answer {
        /// The object of the fill.
        object: ObjectId,
        /// The answer's error.
        source: E,
    },
}

impl<E> fmt::Display for FailFillsError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(_) => f.write_str("a record of the batch could not be decoded"),
            Self::Answer { object, .. } => {
                write!(f, "failing a fill of object {} failed", object.get())
            }
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for FailFillsError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Record(source) => Some(source),
            Self::Answer { source, .. } => Some(source),
        }
    }
}

/// Answers every fill record in `records` (a batch as a queue read returns it) as failed
/// with [`EIO`], calling `answer` once for each. Other records are let go: writeback,
/// release and access records are not sent yet, and none of them has a reader waiting.
/// Returns how many fills were answered.
///
/// It goes on past a failure, so one bad answer does not leave the other fills' readers
/// waiting.
///
/// # Errors
///
/// The first failure, [`FailFillsError::Record`] for a record that cannot be decoded or
/// [`FailFillsError::Answer`] for an answer that failed. Either way some reader may still
/// be waiting, and the caller should close the queue.
///
/// # Examples
///
/// ```
/// use ferric_pcache::abi::{Generation, ObjectId, PageRange, Record, RecordPages};
/// use ferric_pcache::pump::{EIO, fail_fills};
///
/// let pages = RecordPages::new(PageRange::new(0, 1).unwrap()).unwrap();
/// let fill = Record::Fill { object: ObjectId::new(7), generation: Generation::new(1), pages };
/// let mut answered = Vec::new();
/// let n = fail_fills(&fill.to_bytes(), |fill| {
///     answered.push(fill);
///     Ok::<(), ()>(())
/// });
/// assert_eq!(n, Ok(1));
/// assert_eq!(answered[0].errno, EIO);
/// ```
pub fn fail_fills<E>(
    records: &[u8],
    mut answer: impl FnMut(FailedFill) -> Result<(), E>,
) -> Result<usize, FailFillsError<E>> {
    let mut answered = 0;
    let mut first_error = None;
    for record in Record::decode_all(records) {
        let result = match record {
            Ok(Record::Fill {
                object,
                generation,
                pages,
            }) => answer(FailedFill {
                object,
                generation,
                pages,
                errno: EIO,
            })
            .map(|()| answered += 1)
            .map_err(|source| FailFillsError::Answer { object, source }),
            Ok(_) => Ok(()),
            Err(err) => Err(FailFillsError::Record(err)),
        };
        if let Err(err) = result {
            first_error.get_or_insert(err);
        }
    }
    match first_error {
        None => Ok(answered),
        Some(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::PageRange;

    fn fill(object: u64, first: u64, count: u32) -> Record {
        let pages = PageRange::new(first, count).and_then(RecordPages::new);
        Record::Fill {
            object: ObjectId::new(object),
            generation: Generation::new(3),
            pages: pages.expect("a valid record"),
        }
    }

    /// Lays `records` out as a queue read returns them.
    fn batch<const N: usize>(records: [Record; N]) -> [[u8; Record::BYTES]; N] {
        records.map(Record::to_bytes)
    }

    #[test]
    fn every_fill_is_answered_with_eio() {
        let batch = batch([
            fill(1, 0, 2),
            Record::Accessed {
                object: ObjectId::new(1),
                generation: Generation::new(3),
            },
            fill(2, 5, 1),
        ]);
        let mut seen = [None; 2];
        let n = fail_fills(batch.as_flattened(), |fill| {
            let slot = seen.iter_mut().find(|s| s.is_none()).expect("two fills");
            *slot = Some(fill);
            Ok::<(), ()>(())
        });
        assert_eq!(n, Ok(2));
        let [a, b] = seen.map(|s| s.expect("answered"));
        assert_eq!((a.object.get(), a.pages.pages().first()), (1, 0));
        assert_eq!((b.object.get(), b.pages.pages().first()), (2, 5));
        assert!(a.errno == EIO && b.errno == EIO);
        assert_eq!(a.generation, Generation::new(3));
    }

    #[test]
    fn a_failed_answer_does_not_stop_the_others() {
        let batch = batch([fill(1, 0, 1), fill(2, 0, 1), fill(3, 0, 1)]);
        let mut tried = 0;
        let result = fail_fills(batch.as_flattened(), |fill| {
            tried += 1;
            if fill.object.get() == 2 {
                Err("refused")
            } else {
                Ok(())
            }
        });
        assert_eq!(tried, 3);
        assert_eq!(
            result,
            Err(FailFillsError::Answer {
                object: ObjectId::new(2),
                source: "refused"
            })
        );
    }

    #[test]
    fn a_malformed_record_is_an_error_after_the_rest_are_answered() {
        let mut bytes = [0u8; 3 * Record::BYTES];
        bytes[..Record::BYTES].copy_from_slice(&fill(1, 0, 1).to_bytes());
        // An unknown kind in the middle, then a good fill.
        bytes[Record::BYTES] = 0xff;
        bytes[2 * Record::BYTES..].copy_from_slice(&fill(2, 0, 1).to_bytes());
        let mut answered = 0;
        let result = fail_fills(&bytes, |_| {
            answered += 1;
            Ok::<(), ()>(())
        });
        assert_eq!(answered, 2);
        assert!(matches!(result, Err(FailFillsError::Record(_))));
    }

    /// A reader waits on a fill whose record the pump took off the queue but could not
    /// forward. Failing the batch answers the fill with EIO, so the reader's next look gets
    /// the failure instead of waiting forever, and the read after that fills again.
    #[cfg(feature = "alloc")]
    #[test]
    fn a_reader_of_an_unforwarded_fill_gets_eio_and_does_not_hang() {
        extern crate std;
        use crate::abi::{Outcome, PAGE_SIZE};
        use crate::object::{Attempt, PageIndex, Read};
        use std::vec::Vec;

        let mut index: PageIndex<u64> = PageIndex::new();
        index.set_size(4 * PAGE_SIZE);
        let pages = PageRange::new(0, 2).expect("a valid range");
        let mut reader = Attempt::new();
        let Read::Wait { fills } = index.start_read(pages, &mut reader) else {
            panic!("an empty index needs fills");
        };
        // The pump took the records off the queue, then forwarding them failed.
        let records: Vec<[u8; Record::BYTES]> = fills
            .iter()
            .map(|&pages| {
                Record::Fill {
                    object: ObjectId::new(9),
                    generation: Generation::new(1),
                    pages,
                }
                .to_bytes()
            })
            .collect();
        let answered = fail_fills(records.as_flattened(), |fill| {
            index.complete_fill(fill.pages, Outcome::Failed(fill.errno), Vec::new())
        });
        assert_eq!(answered, Ok(fills.len()));
        // Still waiting would be `Read::Wait`; the reader gets its EIO instead.
        assert_eq!(index.start_read(pages, &mut reader), Read::Failed);
        // A new read fills again.
        let mut next = Attempt::new();
        assert!(matches!(
            index.start_read(pages, &mut next),
            Read::Wait { fills } if !fills.is_empty()
        ));
    }
}
