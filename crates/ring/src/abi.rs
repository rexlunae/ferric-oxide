//! The `kernel.ring` scheme's interface and the memory layout every ring shares.
//!
//! An application opens [`RING_PATH`] beneath the scheme root and gets an unset ring
//! descriptor. [`Request::Setup`] gives it a submission queue (SQ) and a completion queue (CQ)
//! of the sizes asked for, in memory the kernel allocates; the application maps that memory
//! with `mmap` (`MAP_SHARED`, offset 0, [`Layout::map_len`] bytes, read-write). It writes
//! [`Sqe`](crate::Sqe)s into the SQ, calls [`Request::Enter`], and reads one
//! [`Cqe`](crate::Cqe) per SQE from the CQ.
//!
//! | Call | Encoding |
//! | --- | --- |
//! | [`Request::Setup`] | `SYS_CALL` on the ring, metadata `[1 \| notify << 8, sq_entries, cq_entries]` |
//! | [`Request::Enter`] | `SYS_CALL` on the ring, metadata `[2, to_submit]`; returns the SQEs consumed |
//! | [`Request::RegisterFiles`] | `SYS_CALL` on the ring with `CallFlags::FD` (and `FD_CLONE` to keep them), the descriptors as `usize`s in the payload, metadata `[3, first]`; returns how many were registered |
//! | [`Request::UnregisterFiles`] | `SYS_CALL` on the ring, metadata `[4, first, count]`; returns how many slots held a file |
//! | `mmap` | `MAP_SHARED`, offset 0, length [`Layout::map_len`] |
//! | `fevent` | `EVENT_READ` while the CQ holds completions |
//!
//! # Memory
//!
//! Page 0 holds the header, 64-bit words grouped so that each side's words sit on a cache
//! line of their own ([`header`]). The SQ's slots start at page 1, the CQ's at the next page
//! after the SQ ([`Layout`]). Both sides compute the layout from the entry counts, so neither
//! reads it back from memory the other side can write.
//!
//! # File table
//!
//! A ring has a table of [`MAX_FIXED_FILES`] slots. [`Request::RegisterFiles`] puts the
//! descriptors it is sent into consecutive slots from `first`; an SQE with
//! [`SqeFlags::FIXED_FILE`](crate::SqeFlags::FIXED_FILE) then names a slot instead of a
//! descriptor, so the kernel skips the descriptor lookup and the file stays open for the ring
//! even if the application closes its descriptor. A slot keeps its file until
//! [`Request::UnregisterFiles`] or until the ring is closed.
//!
//! # Errors
//!
//! `Setup`: `EINVAL` for entry counts that [`Layout::new`] refuses or an unknown notify mode,
//! `EEXIST` when the ring is already set up, `EOPNOTSUPP` for a notify mode the kernel was
//! built without, `ENOMEM` when the memory cannot be allocated. `mmap`: `EINVAL` before
//! `Setup`, for an offset other than 0, a length other than [`Layout::map_len`], or a mapping
//! that is not `MAP_SHARED`. `Enter`: `EINVAL` before `Setup` or when the ring's indices are
//! corrupt, `EBUSY` when another `Enter` on the ring is running or when the CQ has no room for
//! the next chain's completions and nothing was consumed. `RegisterFiles`: `EINVAL` for slots
//! past [`MAX_FIXED_FILES`] or a ring descriptor (a ring may not hold itself or another ring
//! open), `EBUSY` when a slot already holds a file; nothing is registered then, and the
//! descriptors sent are closed. `UnregisterFiles`: `EINVAL` for slots past
//! [`MAX_FIXED_FILES`].

use core::fmt;

/// The scheme's name: it is reached at `/scheme/kernel.ring`.
pub const SCHEME_NAME: &str = "kernel.ring";

/// The scheme's kernel global scheme number, as bootstrap sees it in the kernel's scheme list.
pub const SCHEME_ID: u8 = 15;

/// The path, relative to the scheme root, that opens a new ring.
pub const RING_PATH: &str = "ring";

/// The page size the ring memory is made of.
pub const PAGE_SIZE: usize = 4096;

/// The first header word: `FRNG` in ASCII, little-endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"FRNG\0\0\0\0");

/// The layout and entry format version this crate describes.
pub const VERSION: u64 = 1;

/// The most SQ entries a ring may have.
pub const MAX_SQ_ENTRIES: u32 = 4096;

/// The most CQ entries a ring may have.
pub const MAX_CQ_ENTRIES: u32 = 8192;

/// The slots of a ring's file table ([`Request::RegisterFiles`]).
pub const MAX_FIXED_FILES: u32 = 1024;

/// The most SQEs one chain may link together ([`chain`](crate::chain)).
pub const MAX_CHAIN: usize = 32;

/// The indices of the header's words, in 64-bit words from the start of the ring memory.
///
/// Each group is on its own 64-byte cache line, written by one side only. A side never reads
/// back a word it writes: it keeps its own index privately and only stores it here.
pub mod header {
    /// [`MAGIC`](super::MAGIC), written at setup.
    pub const MAGIC: usize = 0;
    /// [`VERSION`](super::VERSION), written at setup.
    pub const VERSION: usize = 1;
    /// The SQ's entry count, written at setup (informational).
    pub const SQ_ENTRIES: usize = 2;
    /// The CQ's entry count, written at setup (informational).
    pub const CQ_ENTRIES: usize = 3;
    /// The ring's [`NotifyMode`](super::NotifyMode), written at setup (informational).
    pub const NOTIFY: usize = 4;
    /// The SQ's tail: the index after the last SQE the application published.
    pub const SQ_TAIL: usize = 8;
    /// The SQ's head: the index of the next SQE the consumer will take.
    pub const SQ_HEAD: usize = 16;
    /// The SQ consumer's flags ([`NEED_WAKEUP`](crate::notify::NEED_WAKEUP)).
    pub const SQ_FLAGS: usize = 17;
    /// The CQ's tail: the index after the last CQE the producer published.
    pub const CQ_TAIL: usize = 24;
    /// Completions the CQ's producer had to drop (0 for kernel-brokered rings, which never
    /// overflow).
    pub const CQ_OVERFLOW: usize = 25;
    /// The CQ's head: the index of the next CQE the application will read.
    pub const CQ_HEAD: usize = 32;
    /// The CQ consumer's flags ([`NEED_WAKEUP`](crate::notify::NEED_WAKEUP)).
    pub const CQ_FLAGS: usize = 33;
    /// Reserved for the SQ doorbell of WAITPKG rings.
    pub const SQ_DOORBELL: usize = 40;
    /// Reserved for the CQ doorbell of WAITPKG rings.
    pub const CQ_DOORBELL: usize = 41;
}

/// How the two sides of a ring wake each other, chosen per ring at [`Request::Setup`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
#[repr(u8)]
pub enum NotifyMode {
    /// The eventfd-style mode: the submitter calls [`Request::Enter`], and waits for
    /// completions with an `fevent` on the ring descriptor.
    #[default]
    Event = 0,
    /// A polling kernel context on a dedicated core consumes the SQ; the submitter enters only
    /// when the SQ's flags ask for a wakeup. Needs a kernel built with `kernel.rings_sqpoll`.
    SqPoll = 1,
    /// The waiting side sleeps in `UMWAIT` on the producer's tail; needs WAITPKG and a kernel
    /// built with `kernel.cpu.waitpkg`.
    Umwait = 2,
}

impl NotifyMode {
    /// The mode numbered `raw`, if there is one.
    pub const fn from_raw(raw: u64) -> Option<Self> {
        Some(match raw {
            0 => Self::Event,
            1 => Self::SqPoll,
            2 => Self::Umwait,
            _ => return None,
        })
    }

    /// The mode to ask for when the kernel refuses this one with `EOPNOTSUPP`, or `None` for
    /// [`NotifyMode::Event`], which every kernel with rings supports.
    pub const fn fallback(self) -> Option<Self> {
        match self {
            Self::Event => None,
            Self::SqPoll | Self::Umwait => Some(Self::Event),
        }
    }
}

/// A call on a ring descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Allocate the ring's memory: `sq_entries` SQ slots, `cq_entries` CQ slots (0 for twice
    /// `sq_entries`), woken as `notify` says.
    Setup {
        /// SQ slots, a power of two.
        sq_entries: u32,
        /// CQ slots, a power of two of at least `sq_entries`, or 0 for twice `sq_entries`.
        cq_entries: u32,
        /// How the sides wake each other.
        notify: NotifyMode,
    },
    /// Consume up to `to_submit` SQEs, run them, and post their CQEs.
    Enter {
        /// The most SQEs to consume.
        to_submit: u32,
    },
    /// Put the descriptors sent with the call into the file table, from slot `first` on.
    ///
    /// Only the metadata is encoded here: the descriptors go in the payload of a
    /// `CallFlags::FD` call.
    RegisterFiles {
        /// The first slot.
        first: u32,
    },
    /// Empty `count` slots of the file table from `first` on, closing their files.
    UnregisterFiles {
        /// The first slot.
        first: u32,
        /// How many slots.
        count: u32,
    },
}

/// Why metadata words are not a [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The first word names no call.
    Verb(u64),
    /// `Setup` names no [`NotifyMode`].
    Notify(u64),
    /// An entry count, a slot or a slot count does not fit in 32 bits.
    Entries(u64),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Verb(raw) => write!(f, "no ring call has verb {raw}"),
            Self::Notify(raw) => write!(f, "no notify mode is numbered {raw}"),
            Self::Entries(raw) => write!(f, "{raw} does not fit in 32 bits"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// The verb of [`Request::Setup`].
const VERB_SETUP: u64 = 1;
/// The verb of [`Request::Enter`].
const VERB_ENTER: u64 = 2;
/// The verb of [`Request::RegisterFiles`].
const VERB_REGISTER_FILES: u64 = 3;
/// The verb of [`Request::UnregisterFiles`].
const VERB_UNREGISTER_FILES: u64 = 4;

impl Request {
    /// The metadata words of the call and how many of them to send.
    pub fn encode(self) -> ([u64; 3], usize) {
        match self {
            Self::Setup {
                sq_entries,
                cq_entries,
                notify,
            } => (
                [
                    VERB_SETUP | (notify as u64) << 8,
                    u64::from(sq_entries),
                    u64::from(cq_entries),
                ],
                3,
            ),
            Self::Enter { to_submit } => ([VERB_ENTER, u64::from(to_submit), 0], 2),
            Self::RegisterFiles { first } => ([VERB_REGISTER_FILES, u64::from(first), 0], 2),
            Self::UnregisterFiles { first, count } => (
                [VERB_UNREGISTER_FILES, u64::from(first), u64::from(count)],
                3,
            ),
        }
    }

    /// The call in `words`; missing words count as zero, as the kernel fills them.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] when the verb, the notify mode or an entry count is not valid.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let word = |i: usize| words.get(i).copied().unwrap_or(0);
        let entries = |i: usize| u32::try_from(word(i)).map_err(|_| DecodeError::Entries(word(i)));
        match word(0) & 0xff {
            VERB_SETUP => {
                let raw = word(0) >> 8;
                Ok(Self::Setup {
                    sq_entries: entries(1)?,
                    cq_entries: entries(2)?,
                    notify: NotifyMode::from_raw(raw).ok_or(DecodeError::Notify(raw))?,
                })
            }
            VERB_ENTER => Ok(Self::Enter {
                to_submit: entries(1)?,
            }),
            VERB_REGISTER_FILES => Ok(Self::RegisterFiles { first: entries(1)? }),
            VERB_UNREGISTER_FILES => Ok(Self::UnregisterFiles {
                first: entries(1)?,
                count: entries(2)?,
            }),
            _ => Err(DecodeError::Verb(word(0))),
        }
    }
}

/// Why entry counts make no ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutError {
    /// An entry count is zero or not a power of two.
    NotPowerOfTwo {
        /// The count.
        entries: u32,
    },
    /// An entry count is over [`MAX_SQ_ENTRIES`] or [`MAX_CQ_ENTRIES`].
    TooMany {
        /// The count.
        entries: u32,
        /// The limit.
        max: u32,
    },
    /// The CQ has fewer entries than the SQ.
    CqSmallerThanSq {
        /// SQ entries.
        sq_entries: u32,
        /// CQ entries.
        cq_entries: u32,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPowerOfTwo { entries } => {
                write!(f, "{entries} entries: the count must be a power of two")
            }
            Self::TooMany { entries, max } => {
                write!(f, "{entries} entries: at most {max} are allowed")
            }
            Self::CqSmallerThanSq {
                sq_entries,
                cq_entries,
            } => write!(
                f,
                "a CQ of {cq_entries} entries is smaller than the SQ of {sq_entries}"
            ),
        }
    }
}

impl core::error::Error for LayoutError {}

/// Where one queue's words are in the ring memory, in 64-bit words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLayout {
    /// The producer's index word (the tail).
    pub tail: usize,
    /// The consumer's index word (the head).
    pub head: usize,
    /// The consumer's flags word.
    pub flags: usize,
    /// The first word of slot 0.
    pub base: usize,
    /// The slot count, a power of two.
    pub entries: u32,
}

/// The layout of a ring's memory, computed from its entry counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    sq_entries: u32,
    cq_entries: u32,
}

/// `bytes` rounded up to whole pages; the counts are bounded, so this cannot overflow.
const fn page_round(bytes: usize) -> usize {
    bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE
}

impl Layout {
    /// The layout of a ring with `sq_entries` SQ slots and `cq_entries` CQ slots, or twice
    /// `sq_entries` when `cq_entries` is 0.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] when a count is not a power of two, is over its maximum, or the CQ is
    /// smaller than the SQ.
    pub const fn new(sq_entries: u32, cq_entries: u32) -> Result<Self, LayoutError> {
        let cq_entries = if cq_entries == 0 {
            sq_entries.saturating_mul(2)
        } else {
            cq_entries
        };
        if !sq_entries.is_power_of_two() {
            return Err(LayoutError::NotPowerOfTwo {
                entries: sq_entries,
            });
        }
        if sq_entries > MAX_SQ_ENTRIES {
            return Err(LayoutError::TooMany {
                entries: sq_entries,
                max: MAX_SQ_ENTRIES,
            });
        }
        if !cq_entries.is_power_of_two() {
            return Err(LayoutError::NotPowerOfTwo {
                entries: cq_entries,
            });
        }
        if cq_entries > MAX_CQ_ENTRIES {
            return Err(LayoutError::TooMany {
                entries: cq_entries,
                max: MAX_CQ_ENTRIES,
            });
        }
        if cq_entries < sq_entries {
            return Err(LayoutError::CqSmallerThanSq {
                sq_entries,
                cq_entries,
            });
        }
        Ok(Self {
            sq_entries,
            cq_entries,
        })
    }

    /// SQ slots.
    pub const fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// CQ slots.
    pub const fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// The byte offset of the SQ's first slot.
    pub const fn sq_offset(&self) -> usize {
        PAGE_SIZE
    }

    /// The byte offset of the CQ's first slot.
    pub const fn cq_offset(&self) -> usize {
        self.sq_offset() + page_round(self.sq_entries as usize * crate::Sqe::BYTES)
    }

    /// The length of the ring memory in bytes, a whole number of pages.
    pub const fn map_len(&self) -> usize {
        self.cq_offset() + page_round(self.cq_entries as usize * crate::Cqe::BYTES)
    }

    /// The length of the ring memory in pages.
    pub const fn pages(&self) -> usize {
        self.map_len() / PAGE_SIZE
    }

    /// The length of the ring memory in 64-bit words.
    pub const fn words(&self) -> usize {
        self.map_len() / 8
    }

    /// The SQ's words.
    pub const fn sq(&self) -> QueueLayout {
        QueueLayout {
            tail: header::SQ_TAIL,
            head: header::SQ_HEAD,
            flags: header::SQ_FLAGS,
            base: self.sq_offset() / 8,
            entries: self.sq_entries,
        }
    }

    /// The CQ's words.
    pub const fn cq(&self) -> QueueLayout {
        QueueLayout {
            tail: header::CQ_TAIL,
            head: header::CQ_HEAD,
            flags: header::CQ_FLAGS,
            base: self.cq_offset() / 8,
            entries: self.cq_entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::Setup {
                sq_entries: 64,
                cq_entries: 0,
                notify: NotifyMode::Event,
            },
            Request::Setup {
                sq_entries: 4096,
                cq_entries: 8192,
                notify: NotifyMode::Umwait,
            },
            Request::Enter { to_submit: 7 },
            Request::RegisterFiles { first: 5 },
            Request::UnregisterFiles {
                first: 1,
                count: u32::MAX,
            },
            Request::Enter {
                to_submit: u32::MAX,
            },
        ] {
            let (words, len) = request.encode();
            assert_eq!(Request::decode(&words[..len]), Ok(request));
        }
    }

    #[test]
    fn decoding_refuses_bad_words() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb(0)));
        assert_eq!(Request::decode(&[9]), Err(DecodeError::Verb(9)));
        assert_eq!(
            Request::decode(&[1 | 7 << 8, 8, 16]),
            Err(DecodeError::Notify(7))
        );
        assert_eq!(
            Request::decode(&[2, u64::from(u32::MAX) + 1]),
            Err(DecodeError::Entries(u64::from(u32::MAX) + 1))
        );
        // Missing words are zero, as the kernel passes them.
        assert_eq!(Request::decode(&[2]), Ok(Request::Enter { to_submit: 0 }));
    }

    #[test]
    fn layout_places_queues_on_their_own_pages() {
        let layout = Layout::new(64, 0).expect("valid");
        assert_eq!(layout.cq_entries(), 128);
        assert_eq!(layout.sq_offset(), PAGE_SIZE);
        // 64 SQEs of 64 bytes are one page, 128 CQEs of 32 bytes another.
        assert_eq!(layout.cq_offset(), 2 * PAGE_SIZE);
        assert_eq!(layout.map_len(), 3 * PAGE_SIZE);
        assert_eq!(layout.sq().base, 512);
        assert_eq!(layout.cq().base, 1024);

        let small = Layout::new(1, 1).expect("valid");
        assert_eq!(small.map_len(), 3 * PAGE_SIZE);

        let big = Layout::new(MAX_SQ_ENTRIES, MAX_CQ_ENTRIES).expect("valid");
        assert_eq!(big.pages(), 1 + 64 + 64);
    }

    #[test]
    fn layout_refuses_bad_counts() {
        assert_eq!(
            Layout::new(0, 0),
            Err(LayoutError::NotPowerOfTwo { entries: 0 })
        );
        assert_eq!(
            Layout::new(48, 0),
            Err(LayoutError::NotPowerOfTwo { entries: 48 })
        );
        assert_eq!(
            Layout::new(8192, 0),
            Err(LayoutError::TooMany {
                entries: 8192,
                max: MAX_SQ_ENTRIES
            })
        );
        assert_eq!(
            Layout::new(8, 4),
            Err(LayoutError::CqSmallerThanSq {
                sq_entries: 8,
                cq_entries: 4
            })
        );
        assert_eq!(
            Layout::new(4096, 16384),
            Err(LayoutError::TooMany {
                entries: 16384,
                max: MAX_CQ_ENTRIES
            })
        );
    }

    #[test]
    fn header_groups_do_not_share_cache_lines() {
        let line = |word: usize| word / 8;
        let groups = [
            [header::MAGIC, header::NOTIFY],
            [header::SQ_TAIL, header::SQ_TAIL],
            [header::SQ_HEAD, header::SQ_FLAGS],
            [header::CQ_TAIL, header::CQ_OVERFLOW],
            [header::CQ_HEAD, header::CQ_FLAGS],
            [header::SQ_DOORBELL, header::CQ_DOORBELL],
        ];
        for (i, [first, last]) in groups.iter().enumerate() {
            assert_eq!(line(*first), line(*last));
            assert_eq!(line(*first), i);
        }
    }

    #[test]
    fn notify_modes_fall_back_to_event() {
        assert_eq!(NotifyMode::Umwait.fallback(), Some(NotifyMode::Event));
        assert_eq!(NotifyMode::SqPoll.fallback(), Some(NotifyMode::Event));
        assert_eq!(NotifyMode::Event.fallback(), None);
        assert_eq!(NotifyMode::from_raw(3), None);
    }
}
