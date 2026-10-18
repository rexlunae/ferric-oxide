//! The binary record format.
//!
//! A record is a sequence of 64-bit words, stored and copied out little-endian:
//!
//! | Word | Bits | Content |
//! | --- | --- | --- |
//! | 0 | 0..8 | length of the record in words, `2 + argument count` |
//! | 0 | 8..16 | argument count, at most [`MAX_ARGS`] |
//! | 0 | 16..32 | event id |
//! | 0 | 32..64 | reserved, zero |
//! | 1 | 0..64 | timestamp (TSC cycles on x86_64) |
//! | 2.. | 0..64 | the arguments, in the order the event's `args` names them |
//!
//! The length is redundant with the argument count on purpose: a reader that lost its place
//! (or reads a corrupt buffer) finds out at the next header instead of misreading everything
//! after it.

use core::fmt;

use crate::{EventId, MAX_ARGS};

/// Words in a record header (the header word and the timestamp).
pub const HEADER_WORDS: usize = 2;

/// Words in the longest record.
pub const MAX_RECORD_WORDS: usize = HEADER_WORDS + MAX_ARGS;

/// Bytes in a word.
pub const WORD_BYTES: usize = 8;

/// The first word of a record: its length, argument count and event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    nargs: u8,
    event: EventId,
}

impl Header {
    /// The header of a record of `event` with `nargs` arguments, or `None` when `nargs` is above
    /// [`MAX_ARGS`].
    pub const fn new(event: EventId, nargs: usize) -> Option<Self> {
        if nargs > MAX_ARGS {
            return None;
        }
        Some(Self {
            nargs: nargs as u8,
            event,
        })
    }

    /// The event the record is of.
    pub const fn event(self) -> EventId {
        self.event
    }

    /// The number of arguments.
    pub const fn arg_count(self) -> usize {
        self.nargs as usize
    }

    /// The record's length in words, header included.
    pub const fn words(self) -> usize {
        HEADER_WORDS + self.nargs as usize
    }

    /// The header word.
    pub const fn encode(self) -> u64 {
        self.words() as u64 | (self.nargs as u64) << 8 | (self.event.get() as u64) << 16
    }

    /// Reads a header word.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Reserved`] when the reserved bits are not zero, and
    /// [`DecodeError::Length`] when the length does not match the argument count or the
    /// argument count is above [`MAX_ARGS`].
    pub const fn decode(word: u64) -> Result<Self, DecodeError> {
        if word >> 32 != 0 {
            return Err(DecodeError::Reserved { word });
        }
        let words = (word & 0xff) as usize;
        let nargs = ((word >> 8) & 0xff) as usize;
        if nargs > MAX_ARGS || words != HEADER_WORDS + nargs {
            return Err(DecodeError::Length { words, nargs });
        }
        Ok(Self {
            nargs: nargs as u8,
            event: EventId::new((word >> 16) as u16),
        })
    }
}

/// Writes the record of `event` at `timestamp` with `args` into the start of `out`, and returns
/// its length in words, or `None` when there are more than [`MAX_ARGS`] arguments or `out` is
/// too short.
pub fn encode(out: &mut [u64], event: EventId, timestamp: u64, args: &[u64]) -> Option<usize> {
    let header = Header::new(event, args.len())?;
    let out = out.get_mut(..header.words())?;
    let (head, rest) = out.split_at_mut(HEADER_WORDS);
    head.copy_from_slice(&[header.encode(), timestamp]);
    rest.copy_from_slice(args);
    Some(header.words())
}

/// Why a buffer is not a sequence of whole, valid records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The buffer ends inside a record: `needed` bytes are left of it, `available` are there.
    Truncated {
        /// The bytes the record still needs.
        needed: usize,
        /// The bytes left in the buffer.
        available: usize,
    },
    /// A header word has reserved bits set.
    Reserved {
        /// The header word.
        word: u64,
    },
    /// A header's length does not match its argument count.
    Length {
        /// The length in words the header gives.
        words: usize,
        /// The argument count the header gives.
        nargs: usize,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated { needed, available } => write!(
                f,
                "truncated record: {needed} more bytes needed, {available} left"
            ),
            DecodeError::Reserved { word } => {
                write!(f, "record header {word:#018x} has reserved bits set")
            }
            DecodeError::Length { words, nargs } => {
                write!(f, "record header gives {words} words for {nargs} arguments")
            }
        }
    }
}

impl core::error::Error for DecodeError {}

/// One decoded record, borrowing its arguments from the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    header: Header,
    timestamp: u64,
    args: &'a [u8],
}

impl<'a> Record<'a> {
    /// The event the record is of.
    pub fn event(&self) -> EventId {
        self.header.event()
    }

    /// When the event happened (TSC cycles on x86_64).
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// The number of arguments.
    pub fn arg_count(&self) -> usize {
        self.header.arg_count()
    }

    /// Argument `index`, or `None` past the last one.
    pub fn arg(&self, index: usize) -> Option<u64> {
        let start = index.checked_mul(WORD_BYTES)?;
        self.args
            .get(start..start.checked_add(WORD_BYTES)?)
            .map(word_at)
    }

    /// The arguments in order.
    pub fn args(&self) -> impl Iterator<Item = u64> + 'a {
        self.args.chunks_exact(WORD_BYTES).map(word_at)
    }

    /// The record's length in bytes.
    pub fn len_bytes(&self) -> usize {
        self.header.words() * WORD_BYTES
    }
}

/// The little-endian word in `bytes`, which holds exactly [`WORD_BYTES`] bytes.
fn word_at(bytes: &[u8]) -> u64 {
    let mut word = [0; WORD_BYTES];
    word.copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

/// The records in a buffer of whole records, as the trace scheme's `cpu/<n>` files return them.
///
/// Yields each record in turn. After the first error it yields that error and then nothing.
#[derive(Clone, Debug)]
pub struct Records<'a> {
    bytes: &'a [u8],
    failed: bool,
}

impl<'a> Records<'a> {
    /// The records in `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            failed: false,
        }
    }

    /// The next record, or `None` when the buffer is empty.
    fn decode_next(&mut self) -> Option<Result<Record<'a>, DecodeError>> {
        if self.bytes.is_empty() {
            return None;
        }
        let header = match self.bytes.get(..WORD_BYTES) {
            Some(word) => Header::decode(word_at(word)),
            None => Err(DecodeError::Truncated {
                needed: WORD_BYTES,
                available: self.bytes.len(),
            }),
        };
        let header = match header {
            Ok(header) => header,
            Err(err) => return Some(Err(err)),
        };
        let len = header.words() * WORD_BYTES;
        let Some((record, rest)) = self.bytes.split_at_checked(len) else {
            return Some(Err(DecodeError::Truncated {
                needed: len,
                available: self.bytes.len(),
            }));
        };
        self.bytes = rest;
        let (timestamp, args) = record[WORD_BYTES..].split_at(WORD_BYTES);
        Some(Ok(Record {
            header,
            timestamp: word_at(timestamp),
            args,
        }))
    }
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<Record<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let next = self.decode_next();
        if matches!(next, Some(Err(_))) {
            self.failed = true;
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn bytes(words: &[u64]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn header_round_trips() {
        for nargs in 0..=MAX_ARGS {
            let header = Header::new(EventId::new(0x1234), nargs).expect("in range");
            assert_eq!(header.words(), 2 + nargs);
            assert_eq!(Header::decode(header.encode()), Ok(header));
        }
        assert_eq!(Header::new(EventId::new(0), MAX_ARGS + 1), None);
    }

    #[test]
    fn header_layout() {
        let header = Header::new(EventId::new(2), 4).expect("in range");
        assert_eq!(header.encode(), 0x0002_0406);
    }

    #[test]
    fn bad_headers() {
        assert_eq!(
            Header::decode(1 << 40),
            Err(DecodeError::Reserved { word: 1 << 40 })
        );
        // Says 3 words but 2 arguments.
        assert_eq!(
            Header::decode(0x0203),
            Err(DecodeError::Length { words: 3, nargs: 2 })
        );
        assert_eq!(
            Header::decode(0x090b),
            Err(DecodeError::Length {
                words: 11,
                nargs: 9
            })
        );
    }

    #[test]
    fn encode_then_decode() {
        let mut words = [0u64; 32];
        let a = encode(&mut words, EventId::new(1), 10, &[7, 8]).expect("fits");
        let b = encode(&mut words[a..], EventId::new(0), 11, &[]).expect("fits");
        let c = encode(&mut words[a + b..], EventId::new(2), 12, &[u64::MAX; 8]).expect("fits");
        let buf = bytes(&words[..a + b + c]);

        let records: Vec<_> = Records::new(&buf).collect::<Result<_, _>>().expect("valid");
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].event(), EventId::new(1));
        assert_eq!(records[0].timestamp(), 10);
        assert_eq!(records[0].args().collect::<Vec<_>>(), [7, 8]);
        assert_eq!(records[0].arg(1), Some(8));
        assert_eq!(records[0].arg(2), None);
        assert_eq!(records[0].len_bytes(), 32);
        assert_eq!(records[1].arg_count(), 0);
        assert_eq!(records[2].args().filter(|&a| a == u64::MAX).count(), 8);
    }

    #[test]
    fn encode_refuses_what_does_not_fit() {
        let mut words = [0u64; 3];
        assert_eq!(encode(&mut words, EventId::new(0), 0, &[1, 2]), None);
        assert_eq!(encode(&mut [0; 16], EventId::new(0), 0, &[0; 9]), None);
        assert_eq!(encode(&mut words, EventId::new(0), 0, &[1]), Some(3));
    }

    #[test]
    fn truncated_buffers() {
        let mut words = [0u64; 4];
        encode(&mut words, EventId::new(1), 10, &[7, 8]).expect("fits");
        let buf = bytes(&words);
        let mut records = Records::new(&buf[..20]);
        assert_eq!(
            records.next(),
            Some(Err(DecodeError::Truncated {
                needed: 32,
                available: 20
            }))
        );
        assert_eq!(records.next(), None);

        let mut records = Records::new(&buf[..5]);
        assert_eq!(
            records.next(),
            Some(Err(DecodeError::Truncated {
                needed: 8,
                available: 5
            }))
        );
    }

    #[test]
    fn corruption_stops_decoding() {
        let mut words = [0u64; 8];
        let a = encode(&mut words, EventId::new(1), 10, &[]).expect("fits");
        words[a] = u64::MAX;
        let buf = bytes(&words);
        let mut records = Records::new(&buf);
        assert!(matches!(records.next(), Some(Ok(_))));
        assert!(matches!(
            records.next(),
            Some(Err(DecodeError::Reserved { .. }))
        ));
        assert_eq!(records.next(), None);
    }

    #[test]
    fn errors_display() {
        use std::string::ToString;
        let err = DecodeError::Length { words: 3, nargs: 2 };
        assert_eq!(
            err.to_string(),
            "record header gives 3 words for 2 arguments"
        );
    }
}
