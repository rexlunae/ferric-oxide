//! The queue socket: its path, its `SYS_CALL` requests and its statistics record.
//!
//! `open("/scheme/network.<nic>/queue")` ([`QUEUE_PATH`]) gives an unbound queue socket. The
//! application sets it up with `SYS_CALL` on it:
//!
//! | Request | Metadata words | Payload |
//! | --- | --- | --- |
//! | [`Request::Bind`] | `[verb, frame size, fill, rx, tx, completion, mode, cpu]` | the UMEM grant and the ring grant, as `CallFlags::FD` descriptors in that order |
//! | [`Request::AddFilter`] | `[verb]` | a [`Filter`](crate::filter::Filter), [`Filter::BYTES`](crate::filter::Filter::BYTES) bytes; the reply is the [`FilterId`] |
//! | [`Request::RemoveFilter`] | `[verb, filter ID]` | none |
//! | [`Request::Kick`] | `[verb]` | none |
//! | [`Request::Stats`] | `[verb]` | the reply is a [`Stats`] record, [`Stats::BYTES`] bytes |
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is
//! written down, and [`Stats::to_bytes`] and [`Stats::from_bytes`] the one place the
//! statistics layout is. Closing the socket stops its queue, removes its filters and unmaps
//! its grants.

use thiserror::Error;

use crate::umem::FrameSize;

/// The path under a network scheme that gives a queue socket.
pub const QUEUE_PATH: &str = "queue";

const VERB_BIND: u64 = 1;
const VERB_ADD_FILTER: u64 = 2;
const VERB_REMOVE_FILTER: u64 = 3;
const VERB_KICK: u64 = 4;
const VERB_STATS: u64 = 5;

/// The number of entries of a ring: a power of two from [`RingSize::MIN`] to
/// [`RingSize::MAX`], so free-running indices wrap onto slots with a mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RingSize(u32);

impl RingSize {
    /// The fewest entries a ring may have.
    pub const MIN: u32 = 8;
    /// The most entries a ring may have.
    pub const MAX: u32 = 1 << 16;

    /// A ring of `entries` entries, or `None` when that is not a power of two in range.
    pub const fn new(entries: u32) -> Option<Self> {
        if !entries.is_power_of_two() || entries < Self::MIN || entries > Self::MAX {
            return None;
        }
        Some(Self(entries))
    }

    /// The number of entries.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// How the driver serves a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Mode {
    /// Zero-copy where the NIC has a free queue and the filters fit in hardware; copy mode
    /// otherwise.
    Auto,
    /// Copy mode: software filters and one copy, on any NIC.
    Copy,
    /// Zero-copy only: `Bind` or `AddFilter` fails with `ENOSPC` when the hardware cannot do
    /// it.
    ZeroCopy,
}

impl Mode {
    const fn to_word(self) -> u64 {
        match self {
            Self::Auto => 1,
            Self::Copy => 2,
            Self::ZeroCopy => 3,
        }
    }

    const fn from_word(word: u64) -> Option<Self> {
        match word {
            1 => Some(Self::Auto),
            2 => Some(Self::Copy),
            3 => Some(Self::ZeroCopy),
            _ => None,
        }
    }
}

/// The parameters of [`Request::Bind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bind {
    /// The UMEM's frame size.
    pub frame_size: FrameSize,
    /// The Fill ring's size.
    pub fill: RingSize,
    /// The RX ring's size.
    pub rx: RingSize,
    /// The TX ring's size.
    pub tx: RingSize,
    /// The Completion ring's size.
    pub completion: RingSize,
    /// How the driver serves the socket.
    pub mode: Mode,
    /// The logical CPU whose queue interrupt and driver worker serve the socket.
    pub cpu: u32,
}

/// A filter's ID on its socket, from [`Request::AddFilter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FilterId(u64);

impl FilterId {
    /// Filter `id`.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A call on a queue socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Request {
    /// Attach the UMEM and the rings and start the queue.
    Bind(Bind),
    /// Steer the frames a filter matches to the socket.
    AddFilter,
    /// Remove a filter.
    RemoveFilter(FilterId),
    /// Wake the driver: TX entries are waiting, or Fill entries after it set `NEED_WAKEUP`.
    Kick,
    /// Report the socket's [`Stats`].
    Stats,
}

impl Request {
    /// The most metadata words a request takes.
    pub const MAX_WORDS: usize = 8;

    /// The metadata words, and how many of them are used.
    pub const fn encode(self) -> ([u64; Self::MAX_WORDS], usize) {
        let mut words = [0; Self::MAX_WORDS];
        let len = match self {
            Self::Bind(bind) => {
                words = [
                    VERB_BIND,
                    bind.frame_size.bytes() as u64,
                    bind.fill.get() as u64,
                    bind.rx.get() as u64,
                    bind.tx.get() as u64,
                    bind.completion.get() as u64,
                    bind.mode.to_word(),
                    bind.cpu as u64,
                ];
                8
            }
            Self::AddFilter => {
                words[0] = VERB_ADD_FILTER;
                1
            }
            Self::RemoveFilter(id) => {
                words[0] = VERB_REMOVE_FILTER;
                words[1] = id.get();
                2
            }
            Self::Kick => {
                words[0] = VERB_KICK;
                1
            }
            Self::Stats => {
                words[0] = VERB_STATS;
                1
            }
        };
        (words, len)
    }

    /// The request `words` encode.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match it, and [`DecodeError::Value`] for a frame size, ring
    /// size, mode or CPU number out of range.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        let expected = match verb {
            VERB_BIND => 7,
            VERB_REMOVE_FILTER => 1,
            VERB_ADD_FILTER | VERB_KICK | VERB_STATS => 0,
            _ => return Err(DecodeError::Verb),
        };
        if args.len() != expected {
            return Err(DecodeError::Length);
        }
        Ok(match verb {
            VERB_BIND => Self::Bind(decode_bind(args)?),
            VERB_ADD_FILTER => Self::AddFilter,
            VERB_REMOVE_FILTER => Self::RemoveFilter(FilterId(args[0])),
            VERB_KICK => Self::Kick,
            _ => Self::Stats,
        })
    }
}

fn decode_bind(args: &[u64]) -> Result<Bind, DecodeError> {
    let &[frame_size, fill, rx, tx, completion, mode, cpu] = args else {
        return Err(DecodeError::Length);
    };
    let ring = |word: u64| {
        u32::try_from(word)
            .ok()
            .and_then(RingSize::new)
            .ok_or(DecodeError::Value)
    };
    Ok(Bind {
        frame_size: FrameSize::from_bytes(frame_size).ok_or(DecodeError::Value)?,
        fill: ring(fill)?,
        rx: ring(rx)?,
        tx: ring(tx)?,
        completion: ring(completion)?,
        mode: Mode::from_word(mode).ok_or(DecodeError::Value)?,
        cpu: u32::try_from(cpu).map_err(|_| DecodeError::Value)?,
    })
}

/// Why metadata words are not a valid [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    #[error("missing or unknown queue socket verb")]
    Verb,
    /// The number of words does not match the verb.
    #[error("wrong number of metadata words for the queue socket verb")]
    Length,
    /// A word has a value the call does not allow.
    #[error("a queue socket call word is out of range")]
    Value,
}

/// The mode a bound socket runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ActiveMode {
    /// Software filters and one copy.
    Copy,
    /// A hardware queue of its own.
    ZeroCopy,
}

/// What [`Request::Stats`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stats {
    /// The mode the socket runs in.
    pub mode: ActiveMode,
    /// Frames published on the RX ring.
    pub rx_packets: u64,
    /// Frames taken from the TX ring and sent.
    pub tx_packets: u64,
    /// Matching frames dropped because the Fill ring was empty.
    pub fill_empty_drops: u64,
    /// Matching frames dropped because the RX ring was full or the copy path fell behind.
    pub overflow_drops: u64,
    /// Fill and TX entries dropped because they failed the UMEM checks.
    pub invalid_descriptors: u64,
}

impl Stats {
    /// The size of the record: six little-endian `u64`s, the mode first (1 copy, 2
    /// zero-copy).
    pub const BYTES: usize = 48;

    /// The record.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mode: u64 = match self.mode {
            ActiveMode::Copy => 1,
            ActiveMode::ZeroCopy => 2,
        };
        let words = [
            mode,
            self.rx_packets,
            self.tx_packets,
            self.fill_empty_drops,
            self.overflow_drops,
            self.invalid_descriptors,
        ];
        let mut out = [0; Self::BYTES];
        for (chunk, word) in out.chunks_exact_mut(8).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// The statistics `bytes` hold.
    ///
    /// # Errors
    ///
    /// [`StatsError::Length`] when `bytes` is not [`Stats::BYTES`] long, and
    /// [`StatsError::Mode`] for an unknown mode.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StatsError> {
        if bytes.len() != Self::BYTES {
            return Err(StatsError::Length);
        }
        let mut words = [0u64; 6];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            let mut le = [0; 8];
            le.copy_from_slice(chunk);
            *word = u64::from_le_bytes(le);
        }
        let [
            mode,
            rx_packets,
            tx_packets,
            fill_empty_drops,
            overflow_drops,
            invalid_descriptors,
        ] = words;
        let mode = match mode {
            1 => ActiveMode::Copy,
            2 => ActiveMode::ZeroCopy,
            _ => return Err(StatsError::Mode),
        };
        Ok(Self {
            mode,
            rx_packets,
            tx_packets,
            fill_empty_drops,
            overflow_drops,
            invalid_descriptors,
        })
    }
}

/// Why bytes are not a [`Stats`] record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum StatsError {
    /// The buffer is not [`Stats::BYTES`] long.
    #[error("a queue socket statistics record is {} bytes", Stats::BYTES)]
    Length,
    /// The mode is unknown.
    #[error("unknown queue socket mode")]
    Mode,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::ToString;

    fn ring(entries: u32) -> RingSize {
        RingSize::new(entries).expect("a valid ring size")
    }

    fn bind() -> Bind {
        Bind {
            frame_size: FrameSize::Bytes2048,
            fill: ring(4096),
            rx: ring(2048),
            tx: ring(2048),
            completion: ring(4096),
            mode: Mode::Auto,
            cpu: 3,
        }
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::Bind(bind()),
            Request::Bind(Bind {
                frame_size: FrameSize::Bytes4096,
                mode: Mode::ZeroCopy,
                cpu: u32::MAX,
                ..bind()
            }),
            Request::Bind(Bind {
                mode: Mode::Copy,
                ..bind()
            }),
            Request::AddFilter,
            Request::RemoveFilter(FilterId::new(u64::MAX)),
            Request::Kick,
            Request::Stats,
        ] {
            let (words, len) = request.encode();
            assert_eq!(Request::decode(&words[..len]), Ok(request));
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[0]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[99]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[VERB_KICK, 0]), Err(DecodeError::Length));
        assert_eq!(
            Request::decode(&[VERB_REMOVE_FILTER]),
            Err(DecodeError::Length)
        );
        let (good, len) = Request::Bind(bind()).encode();
        assert_eq!(Request::decode(&good[..len - 1]), Err(DecodeError::Length));
        for (at, value) in [
            (1, 1024),
            (2, 4),
            (3, 3000),
            (4, 1 << 17),
            (5, u64::MAX),
            (6, 0),
            (7, 1 << 32),
        ] {
            let mut bad = good;
            bad[at] = value;
            assert_eq!(
                Request::decode(&bad[..len]),
                Err(DecodeError::Value),
                "word {at}"
            );
        }
        assert!(!DecodeError::Value.to_string().is_empty());
    }

    #[test]
    fn ring_sizes() {
        assert_eq!(RingSize::new(RingSize::MIN).map(RingSize::get), Some(8));
        assert_eq!(
            RingSize::new(RingSize::MAX).map(RingSize::get),
            Some(1 << 16)
        );
        for bad in [0, 4, 12, 1 << 17] {
            assert_eq!(RingSize::new(bad), None, "{bad}");
        }
    }

    #[test]
    fn stats_round_trip() {
        let stats = Stats {
            mode: ActiveMode::ZeroCopy,
            rx_packets: 1,
            tx_packets: 2,
            fill_empty_drops: 3,
            overflow_drops: 4,
            invalid_descriptors: u64::MAX,
        };
        let bytes = stats.to_bytes();
        assert_eq!(Stats::from_bytes(&bytes), Ok(stats));
        assert_eq!(Stats::from_bytes(&bytes[..47]), Err(StatsError::Length));
        let mut bad = bytes;
        bad[0] = 0;
        assert_eq!(Stats::from_bytes(&bad), Err(StatsError::Mode));
        assert!(!StatsError::Mode.to_string().is_empty());
    }
}
