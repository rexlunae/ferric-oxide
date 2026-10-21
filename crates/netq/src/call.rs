//! The queue socket: its path, its `SYS_CALL` requests and its statistics record.
//!
//! `open("/scheme/network.<nic>/queue")` ([`QUEUE_PATH`]) gives an unbound queue socket. The
//! application sets it up with `SYS_CALL` on it:
//!
//! | Request | Call | Metadata words | Payload |
//! | --- | --- | --- | --- |
//! | [`Request::Attach`] | `CallFlags::WRITE \| CallFlags::FD` | `[verb]` | the descriptor numbers of the UMEM grant and the ring grant, in that order (they move to the driver) |
//! | [`Request::Bind`] | plain | `[verb]` | a [`Bind`] record, [`Bind::BYTES`] bytes |
//! | [`Request::AddFilter`] | plain | `[verb]` | a [`Filter`](crate::filter::Filter), [`Filter::BYTES`](crate::filter::Filter::BYTES) bytes; the reply is the [`FilterId`] |
//! | [`Request::RemoveFilter`] | plain | `[verb, filter ID]` | none |
//! | [`Request::Own`] | `CallFlags::WRITE \| CallFlags::FD` | `[verb]` | the descriptor number of a netstack `udp` or `tcp` socket bound on the port (it moves to the driver, which keeps it open, so netstack keeps the port reserved) |
//! | [`Request::Kick`] | plain | `[verb]` | none |
//! | [`Request::Stats`] | plain | `[verb]` | the reply is a [`Stats`] record, [`Stats::BYTES`] bytes |
//!
//! `Attach` comes before `Bind`, which starts the queue on the attached grants; attaching
//! again before `Bind` replaces them. `Own` comes after `Bind`: a filter on a TCP or UDP
//! destination port needs the port owned first ([`crate::own`]).
//!
//! The kernel gives a scheme at most [`Request::CALL_WORDS`] metadata words of a plain call,
//! padded with zeros to that many (the caller's word count does not reach the scheme), and only
//! the first word of a descriptor-passing call. So every request fits in three words, the
//! `Attach` verb in one, and [`Request::decode`] reads a request from the words exactly as the
//! scheme receives them: words a request does not use must be zero.
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is
//! written down, [`Bind::to_bytes`] and [`Bind::from_bytes`] the one place the bind record's
//! is, and [`Stats::to_bytes`] and [`Stats::from_bytes`] the one place the statistics
//! layout is. Closing the socket stops its queue, removes its filters and unmaps its grants.

use bitflags::bitflags;
use thiserror::Error;

use crate::umem::FrameSize;

/// The path under a network scheme that gives a queue socket.
pub const QUEUE_PATH: &str = "queue";

const VERB_BIND: u64 = 1;
const VERB_ADD_FILTER: u64 = 2;
const VERB_REMOVE_FILTER: u64 = 3;
const VERB_KICK: u64 = 4;
const VERB_STATS: u64 = 5;
const VERB_ATTACH: u64 = 6;
const VERB_OWN: u64 = 7;

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

/// The parameters of [`Request::Bind`], its payload.
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
    /// Options.
    pub flags: BindFlags,
}

bitflags! {
    /// The options of [`Bind`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct BindFlags: u32 {
        /// Send TX frames unchecked. Without it the driver validates each TX frame
        /// ([`crate::own::Ownership::check_tx`]): the NIC's source MAC, and TCP or UDP from an
        /// owned port. Queue sockets are root's, so any socket may ask for it; unprivileged
        /// sockets will not.
        const RAW_TX = 1;
    }
}

impl Bind {
    /// The size of the record: eight little-endian `u32`s, the frame size in bytes, the Fill,
    /// RX, TX and Completion ring sizes in entries, the mode (1 auto, 2 copy, 3 zero-copy),
    /// the CPU, and the [`BindFlags`] (zero before flags existed, so older records still
    /// decode).
    pub const BYTES: usize = 32;

    /// The record.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let words = [
            self.frame_size.bytes(),
            self.fill.get(),
            self.rx.get(),
            self.tx.get(),
            self.completion.get(),
            self.mode.to_word() as u32,
            self.cpu,
            self.flags.bits(),
        ];
        let mut out = [0; Self::BYTES];
        for (chunk, word) in out.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// The parameters `bytes` hold.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Length`] when `bytes` is not [`Bind::BYTES`] long, and
    /// [`DecodeError::Value`] for a frame size, ring size or mode out of range, or an unknown
    /// flag.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != Self::BYTES {
            return Err(DecodeError::Length);
        }
        let mut words = [0u32; 8];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
            let mut le = [0; 4];
            le.copy_from_slice(chunk);
            *word = u32::from_le_bytes(le);
        }
        let [frame_size, fill, rx, tx, completion, mode, cpu, flags] = words;
        let flags = BindFlags::from_bits(flags).ok_or(DecodeError::Value)?;
        let ring = |entries| RingSize::new(entries).ok_or(DecodeError::Value);
        Ok(Self {
            frame_size: FrameSize::from_bytes(u64::from(frame_size)).ok_or(DecodeError::Value)?,
            fill: ring(fill)?,
            rx: ring(rx)?,
            tx: ring(tx)?,
            completion: ring(completion)?,
            mode: Mode::from_word(u64::from(mode)).ok_or(DecodeError::Value)?,
            cpu,
            flags,
        })
    }
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
    /// Hand the driver the UMEM grant and the ring grant (a descriptor-passing call).
    Attach,
    /// Start the queue on the attached grants, with the [`Bind`] parameters in the payload.
    Bind,
    /// Steer the frames a filter matches to the socket.
    AddFilter,
    /// Remove a filter.
    RemoveFilter(FilterId),
    /// Wake the driver: TX entries are waiting, or Fill entries after it set `NEED_WAKEUP`.
    Kick,
    /// Report the socket's [`Stats`].
    Stats,
    /// Own a TCP or UDP port: pass a netstack socket bound on it (a descriptor-passing call).
    Own,
}

impl Request {
    /// The most metadata words the kernel hands a scheme for a plain call
    /// (`crates/kernel/src/syscall/fs.rs`, `call`).
    pub const CALL_WORDS: usize = 3;

    /// The metadata words the kernel hands a scheme for a descriptor-passing call (the
    /// `Sendfd` request carries only the first).
    pub const FD_CALL_WORDS: usize = 1;

    /// Whether the request is a descriptor-passing call (`CallFlags::FD`) rather than a plain
    /// one. A driver refuses a request that arrives on the other kind of call.
    pub const fn passes_descriptors(self) -> bool {
        matches!(self, Self::Attach | Self::Own)
    }

    /// The metadata words, and how many of them are used.
    pub const fn encode(self) -> ([u64; Self::CALL_WORDS], usize) {
        let mut words = [0; Self::CALL_WORDS];
        let len = match self {
            Self::Attach => {
                words[0] = VERB_ATTACH;
                1
            }
            Self::Bind => {
                words[0] = VERB_BIND;
                1
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
            Self::Own => {
                words[0] = VERB_OWN;
                1
            }
        };
        (words, len)
    }

    /// The request `words` encode, as the scheme receives them: up to
    /// [`CALL_WORDS`](Self::CALL_WORDS) words, where those the request does not use are zero.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, and [`DecodeError::Length`] when
    /// a word the request needs is missing, or a word it does not use is not zero.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        let used = match verb {
            VERB_REMOVE_FILTER => 1,
            VERB_ATTACH | VERB_BIND | VERB_ADD_FILTER | VERB_KICK | VERB_STATS | VERB_OWN => 0,
            _ => return Err(DecodeError::Verb),
        };
        if args.len() < used
            || args.len() >= Self::CALL_WORDS
            || args[used..].iter().any(|&w| w != 0)
        {
            return Err(DecodeError::Length);
        }
        Ok(match verb {
            VERB_ATTACH => Self::Attach,
            VERB_BIND => Self::Bind,
            VERB_ADD_FILTER => Self::AddFilter,
            VERB_REMOVE_FILTER => Self::RemoveFilter(FilterId(args[0])),
            VERB_KICK => Self::Kick,
            VERB_OWN => Self::Own,
            _ => Self::Stats,
        })
    }
}

/// Why metadata words are not a valid [`Request`], or bytes not a valid [`Bind`] record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    #[error("missing or unknown queue socket verb")]
    Verb,
    /// A word the verb needs is missing, a word it does not use is not zero, or a payload
    /// record has the wrong length.
    #[error("queue socket call words or payload do not fit the verb")]
    Length,
    /// A payload field has a value the call does not allow.
    #[error("a queue socket call field is out of range")]
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
    /// TX frames dropped by TX validation ([`crate::own::Ownership::check_tx`]).
    pub tx_rejected: u64,
}

impl Stats {
    /// The size of the record: seven little-endian `u64`s, the mode first (1 copy, 2
    /// zero-copy), then the counters in field order.
    pub const BYTES: usize = 56;

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
            self.tx_rejected,
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
        let mut words = [0u64; 7];
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
            tx_rejected,
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
            tx_rejected,
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
            flags: BindFlags::empty(),
        }
    }

    /// The metadata a user-space scheme receives for a plain `SYS_CALL` with `words`, as the
    /// kernel builds it: `call` (`crates/kernel/src/syscall/fs.rs`) copies at most
    /// [`Request::CALL_WORDS`] words in as bytes, and `call_generic`
    /// (`crates/kernel/src/scheme/user.rs`) puts them in the request zero-padded to that many.
    fn through_plain_call(words: &[u64]) -> [u64; Request::CALL_WORDS] {
        let mut bytes = [0u8; Request::CALL_WORDS * 8];
        let sent: std::vec::Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let copied = sent.len().min(bytes.len());
        bytes[..copied].copy_from_slice(&sent[..copied]);
        let mut meta = [0u64; Request::CALL_WORDS];
        for (word, chunk) in meta.iter_mut().zip(bytes.chunks_exact(8)).take(copied / 8) {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(chunk);
            *word = u64::from_ne_bytes(raw);
        }
        meta
    }

    /// The metadata a user-space scheme receives for a descriptor-passing `SYS_CALL`: the
    /// `Sendfd` request carries only the first word (`kfdwrite` in
    /// `crates/kernel/src/scheme/user.rs`).
    fn through_fd_call(words: &[u64]) -> [u64; Request::FD_CALL_WORDS] {
        [words.first().copied().unwrap_or(0)]
    }

    #[test]
    fn requests_round_trip_through_the_kernel() {
        for request in [
            Request::Attach,
            Request::Bind,
            Request::AddFilter,
            Request::RemoveFilter(FilterId::new(u64::MAX)),
            Request::Kick,
            Request::Stats,
            Request::Own,
        ] {
            let (words, len) = request.encode();
            let sent = &words[..len];
            assert_eq!(Request::decode(sent), Ok(request), "{request:?}");
            if request.passes_descriptors() {
                assert!(len <= Request::FD_CALL_WORDS);
                assert_eq!(Request::decode(&through_fd_call(sent)), Ok(request));
            } else {
                assert_eq!(Request::decode(&through_plain_call(sent)), Ok(request));
            }
        }
    }

    #[test]
    fn bind_records_round_trip() {
        for bind in [
            bind(),
            Bind {
                frame_size: FrameSize::Bytes4096,
                mode: Mode::ZeroCopy,
                cpu: u32::MAX,
                ..bind()
            },
            Bind {
                mode: Mode::Copy,
                fill: ring(RingSize::MIN),
                completion: ring(RingSize::MAX),
                flags: BindFlags::RAW_TX,
                ..bind()
            },
        ] {
            assert_eq!(Bind::from_bytes(&bind.to_bytes()), Ok(bind));
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[0]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[99, 0, 0]), Err(DecodeError::Verb));
        assert_eq!(
            Request::decode(&[VERB_KICK, 1, 0]),
            Err(DecodeError::Length)
        );
        assert_eq!(
            Request::decode(&[VERB_KICK, 0, 7]),
            Err(DecodeError::Length)
        );
        assert_eq!(
            Request::decode(&[VERB_KICK, 0, 0, 0]),
            Err(DecodeError::Length)
        );
        assert_eq!(
            Request::decode(&[VERB_REMOVE_FILTER]),
            Err(DecodeError::Length)
        );
        assert_eq!(
            Request::decode(&[VERB_REMOVE_FILTER, 5, 1]),
            Err(DecodeError::Length)
        );

        let good = bind().to_bytes();
        assert_eq!(Bind::from_bytes(&good[..31]), Err(DecodeError::Length));
        for (word, value) in [
            (0, 1024),
            (1, 4),
            (2, 3000),
            (3, 1 << 17),
            (4, u32::MAX),
            (5, 0),
            (5, 4),
            (7, 2),
            (7, u32::MAX),
        ] {
            let mut bad = good;
            bad[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(
                Bind::from_bytes(&bad),
                Err(DecodeError::Value),
                "word {word}"
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
            tx_rejected: 5,
        };
        let bytes = stats.to_bytes();
        assert_eq!(Stats::from_bytes(&bytes), Ok(stats));
        assert_eq!(Stats::from_bytes(&bytes[..55]), Err(StatsError::Length));
        let mut bad = bytes;
        bad[0] = 0;
        assert_eq!(Stats::from_bytes(&bad), Err(StatsError::Mode));
        assert!(!StatsError::Mode.to_string().is_empty());
    }
}
