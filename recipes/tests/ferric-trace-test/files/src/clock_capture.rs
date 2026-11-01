//! Bounded mapped-ring reads for the clock probe, keeping only three write signatures.
//!
//! Each pass takes one `View::peek`, so producer activity and the caller's traced
//! commit cannot extend the pass. The caller commits the returned boundary after
//! validation, and polls all CPUs between bursts rather than waiting for empty rings.

#![forbid(unsafe_code)]

use core::fmt;

use ferric_trace::EventId;
use ferric_trace::kernel::{CATALOG, SYSCALL_ENTER};
use ferric_trace::record::{DecodeError, Records};
#[cfg(target_os = "redox")]
use ferric_trace::ring::ViewError;
use ferric_trace::ring::{Peek, ReadError, View, Words};

/// Ring counters sampled around the probe, in words except for dropped records.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Counters {
    /// The producer's next word.
    pub head: u64,
    /// The consumer's next word.
    pub tail: u64,
    /// Records dropped since boot.
    pub lost: u64,
}

impl Counters {
    /// Samples the mapped counters; these independent loads are diagnostic, not a transaction.
    pub fn sample<W: Words>(view: &View<W>) -> Self {
        Self {
            head: view.head(),
            tail: view.tail(),
            lost: view.lost(),
        }
    }
}

/// A batch cannot be read or does not match the shared trace catalog.
#[derive(Debug)]
pub(crate) enum CaptureError {
    /// The shared mapping's header is invalid.
    #[cfg(target_os = "redox")]
    View(ViewError),
    /// The supplied buffer cannot hold the next record.
    Read(ReadError),
    /// Copied bytes do not decode.
    Decode(DecodeError),
    /// A mapped record is corrupt.
    Corrupt { position: u64 },
    /// A record names an unknown event.
    Unknown { event: EventId },
    /// The event's argument count differs from the catalog.
    Arguments {
        event: EventId,
        actual: usize,
        expected: usize,
    },
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_os = "redox")]
            Self::View(_) => f.write_str("cannot view the mapped ring"),
            Self::Read(_) => f.write_str("cannot copy the mapped ring"),
            Self::Decode(_) => f.write_str("cannot decode a copied record"),
            Self::Corrupt { position } => write!(f, "corrupt record at word {position}"),
            Self::Unknown { event } => write!(f, "unknown event {event}"),
            Self::Arguments {
                event,
                actual,
                expected,
            } => {
                write!(
                    f,
                    "event {event} has {actual} arguments, expected {expected}"
                )
            }
        }
    }
}

impl core::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            #[cfg(target_os = "redox")]
            Self::View(source) => Some(source),
            Self::Read(source) => Some(source),
            Self::Decode(source) => Some(source),
            _ => None,
        }
    }
}

/// The two clock markers or the controlled intervening writes were lost or duplicated.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CountsError {
    actual: [u64; 3],
    expected_background: u64,
}

impl fmt::Display for CountsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} first and {} second marker writes, {} background writes; expected 1, 1 and {}",
            self.actual[0], self.actual[1], self.actual[2], self.expected_background
        )
    }
}

impl core::error::Error for CountsError {}

#[derive(Clone, Copy, Debug, Default)]
struct Hits {
    count: u64,
    timestamp: u64,
}

/// Constant-space marker collection across CPUs and bounded read passes.
#[derive(Debug)]
pub(crate) struct Markers {
    write_nr: u64,
    fd: u64,
    first_len: u64,
    hits: [Hits; 3],
}

impl Markers {
    /// Matches writes to `fd` with lengths `first_len`, `first_len + 1` and `first_len + 2`.
    pub fn new(write_nr: u64, fd: u64, first_len: u64) -> Self {
        Self {
            write_nr,
            fd,
            first_len,
            hits: [Hits::default(); 3],
        }
    }

    /// Validates one bounded batch and retains matching timestamps/counts, without consuming it.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] for unreadable, corrupt or catalog-incompatible records.
    /// The caller must not commit a failed batch.
    pub fn collect<W: Words>(
        &mut self,
        view: &View<W>,
        buffer: &mut [u8],
    ) -> Result<Peek, CaptureError> {
        let peek = view.peek(view.tail(), buffer).map_err(CaptureError::Read)?;
        if peek.is_corrupt() {
            return Err(CaptureError::Corrupt {
                position: peek.verified_end(),
            });
        }
        for record in Records::new(&buffer[..peek.len()]) {
            let record = record.map_err(CaptureError::Decode)?;
            let event = CATALOG.get(record.event()).ok_or(CaptureError::Unknown {
                event: record.event(),
            })?;
            if event.args().len() != record.arg_count() {
                return Err(CaptureError::Arguments {
                    event: record.event(),
                    actual: record.arg_count(),
                    expected: event.args().len(),
                });
            }
            if record.event() != SYSCALL_ENTER.id()
                || record.arg(0) != Some(self.write_nr)
                || record.arg(1) != Some(self.fd)
            {
                continue;
            }
            if let Some(index) = record
                .arg(3)
                .and_then(|len| len.checked_sub(self.first_len))
                .filter(|&index| index < 3)
            {
                let hit = &mut self.hits[index as usize];
                if hit.count == 0 {
                    hit.timestamp = record.timestamp();
                }
                hit.count = hit.count.saturating_add(1);
            }
        }
        Ok(peek)
    }

    /// Returns both timestamps only when all three write counts match exactly.
    ///
    /// # Errors
    ///
    /// Returns [`CountsError`] for missing or duplicated markers or background writes.
    pub fn timestamps(&self, expected_background: u64) -> Result<(u64, u64), CountsError> {
        let actual = self.hits.map(|hit| hit.count);
        if actual != [1, 1, expected_background] {
            return Err(CountsError {
                actual,
                expected_background,
            });
        }
        Ok((self.hits[0].timestamp, self.hits[1].timestamp))
    }
}

#[cfg(test)]
mod tests;
