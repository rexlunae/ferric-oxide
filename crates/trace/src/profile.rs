//! The kernel profiler's interface (`kernel.profiling`): its sample format and its control
//! file, shared by the kernel and `ferric-trace profile` / `flamegraph`.
//!
//! The profiler is upstream's: a CPU set aside for it sends an NMI to every other CPU on a
//! timer, and each NMI writes one *sample* to its CPU's buffer, read (destructively) from
//! `/scheme/debug/profiling-<n>` by root. Writing `1` or `0` to `/scheme/debug/ctl-profiling`
//! starts or stops sampling. Ferric Oxide adds a read of `ctl-profiling`, which gives one
//! [`ControlLine`]: the address the kernel's text was loaded at, so that a host tool can
//! symbolize kernel addresses under KASLR (`kernel.kaslr_image`). It also adds an owner:
//! writing `x` takes the profiler for that open file (and stops sampling), other files'
//! writes then fail with `EBUSY`, and closing the owner's file stops sampling and frees it.
//! [`ProfilerOwner`] is that rule; the kernel keeps it, and the sampling switch it sets, under
//! one lock, so a closing owner cannot stop the sampling of the next.
//!
//! A sample is a run of native-endian machine words (`u64` on x86_64):
//!
//! | Word | Content |
//! | --- | --- |
//! | 0 | [`SAMPLE_MAGIC`] with the sample's length in words in the low 8 bits |
//! | 1 | the interrupted instruction pointer |
//! | 2 | the TSC at the NMI |
//! | 3 | the interrupted context's debug id (as `sched.switch` records name it) |
//! | 4.. | return addresses, innermost first: kernel frames, and for a sample taken in the kernel during a system call, then the user instruction pointer and user frames |
//!
//! [`Samples`] splits a buffer into [`Sample`]s. A buffer can hold words that are not a whole
//! sample (one torn by a kernel that wrote part of a sample into a nearly full buffer, before
//! Ferric Oxide made it write whole samples only), so [`Samples`] reports such words as one
//! error and carries on at the next whole sample.

use core::fmt;

/// The sample header without its length.
pub const SAMPLE_MAGIC: u64 = 0xfedf_ac00;

/// The bits of the header word that hold the length.
pub const LENGTH_MASK: u64 = 0xff;

/// The words before a sample's frames.
pub const SAMPLE_FIXED_WORDS: usize = 4;

/// The control file, under `/scheme/debug/`.
pub const CTL_FILE: &str = "ctl-profiling";

/// The prefix of the per-CPU buffer files, under `/scheme/debug/`: `profiling-<cpu>`.
pub const CPU_FILE_PREFIX: &str = "profiling-";

/// The lowest kernel (upper half) address on x86_64; anything below is user space.
pub const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;

/// One sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample<'a> {
    /// The interrupted instruction pointer.
    pub ip: u64,
    /// The TSC at the sample.
    pub tsc: u64,
    /// The interrupted context's debug id.
    pub context: u32,
    /// The return addresses, innermost first.
    pub frames: &'a [u64],
}

impl Sample<'_> {
    /// The stack from the leaf outward: the instruction pointer, then the frames.
    pub fn stack(&self) -> impl Iterator<Item = u64> + '_ {
        core::iter::once(self.ip).chain(self.frames.iter().copied())
    }
}

/// Why a buffer does not split into samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleError {
    /// The word at `at` is not a sample header.
    BadHeader {
        /// The word's index.
        at: usize,
    },
    /// The sample at `at` is longer than the words left, or shorter than its fixed part.
    BadLength {
        /// The header's index.
        at: usize,
    },
}

impl fmt::Display for SampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SampleError::BadHeader { at } => write!(f, "word {at} is not a sample header"),
            SampleError::BadLength { at } => write!(f, "the sample at word {at} has a bad length"),
        }
    }
}

impl core::error::Error for SampleError {}

/// The samples of a buffer, in order. Words that do not start a whole sample give one error
/// for the run of them, and iteration resumes at the next word that does.
#[derive(Clone, Debug)]
pub struct Samples<'a> {
    words: &'a [u64],
    at: usize,
}

impl<'a> Samples<'a> {
    /// The samples in `words`, a profiling buffer's contents.
    pub fn new(words: &'a [u64]) -> Self {
        Self { words, at: 0 }
    }

    /// The index of the word the next item starts at: where in the buffer the sample (or the
    /// run of bad words) that [`Iterator::next`] gives next begins.
    pub fn position(&self) -> usize {
        self.at
    }
}

impl<'a> Iterator for Samples<'a> {
    type Item = Result<Sample<'a>, SampleError>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.at;
        let header = *self.words.get(at)?;
        let Some(sample) = whole_sample(self.words, at) else {
            self.at = (at + 1..self.words.len())
                .find(|&next| whole_sample(self.words, next).is_some())
                .unwrap_or(self.words.len());
            return Some(Err(if header & !LENGTH_MASK != SAMPLE_MAGIC {
                SampleError::BadHeader { at }
            } else {
                SampleError::BadLength { at }
            }));
        };
        self.at = at + sample.len();
        Some(Ok(Sample {
            ip: sample[1],
            tsc: sample[2],
            // The kernel stores a u32 widened to a word.
            context: sample[3] as u32,
            frames: &sample[SAMPLE_FIXED_WORDS..],
        }))
    }
}

/// The whole sample starting at word `at`, if one does: a header, and a length that covers
/// the fixed words and fits in `words`.
fn whole_sample(words: &[u64], at: usize) -> Option<&[u64]> {
    let header = *words.get(at)?;
    if header & !LENGTH_MASK != SAMPLE_MAGIC {
        return None;
    }
    let len = (header & LENGTH_MASK) as usize;
    if len < SAMPLE_FIXED_WORDS {
        return None;
    }
    words.get(at..at.saturating_add(len))
}

/// What a read of `ctl-profiling` gives: `kernel_text <address>\n`, the run-time address of
/// the kernel's first text byte (`__text_start`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlLine {
    /// The run-time address of `__text_start`.
    pub text_start: u64,
}

/// Why a [`ControlLine`] does not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlLineError {
    /// The line is not `kernel_text 0x<hex>`.
    Format,
}

impl fmt::Display for ControlLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a `kernel_text 0x<address>` line")
    }
}

impl core::error::Error for ControlLineError {}

impl ControlLine {
    /// Parses the line, with or without its `\n`.
    ///
    /// # Errors
    ///
    /// [`ControlLineError::Format`] for anything else.
    pub fn parse(line: &str) -> Result<Self, ControlLineError> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let hex = line
            .strip_prefix("kernel_text 0x")
            .ok_or(ControlLineError::Format)?;
        let text_start = u64::from_str_radix(hex, 16).map_err(|_| ControlLineError::Format)?;
        Ok(Self { text_start })
    }
}

impl fmt::Display for ControlLine {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "kernel_text {:#x}", self.text_start)
    }
}

/// Who owns the profiler, and what each write to or close of `ctl-profiling` does to
/// sampling. The kernel holds it under one lock together with the sampling switch, and sets
/// the switch to what [`ProfilerOwner::write`] and [`ProfilerOwner::close`] return before it
/// lets go, so changes of owner and of sampling happen one at a time.
///
/// # Examples
///
/// ```
/// use ferric_trace::profile::{ControlWriteError, ProfilerOwner};
///
/// let mut owner = ProfilerOwner::new();
/// assert_eq!(owner.write(1, b'x'), Ok(false)); // handle 1 takes the profiler
/// assert_eq!(owner.write(1, b'1'), Ok(true)); // and starts sampling
/// assert_eq!(owner.write(2, b'0'), Err(ControlWriteError::Busy));
/// assert_eq!(owner.close(1), Some(false)); // its close stops sampling
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfilerOwner {
    owner: Option<usize>,
}

/// Why a write to `ctl-profiling` was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlWriteError {
    /// Another handle owns the profiler (`EBUSY`).
    Busy,
    /// Not `x`, `0` or `1` (`EINVAL`).
    Invalid,
}

impl fmt::Display for ControlWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "another handle owns the profiler",
            Self::Invalid => "a control write is x, 0 or 1",
        })
    }
}

impl core::error::Error for ControlWriteError {}

impl ProfilerOwner {
    /// Nobody owns the profiler.
    pub const fn new() -> Self {
        Self { owner: None }
    }

    /// The handle that owns the profiler, if one does.
    pub const fn owner(&self) -> Option<usize> {
        self.owner
    }

    /// A write of `byte` by handle `handle`, and whether sampling is on after it: `x` takes
    /// the profiler (again, for its owner) and stops sampling, and `1` and `0` start and stop
    /// it, for the owner or while nobody owns it.
    ///
    /// # Errors
    ///
    /// [`ControlWriteError::Busy`] when another handle owns the profiler, and
    /// [`ControlWriteError::Invalid`] for any other byte. Neither changes anything.
    pub fn write(&mut self, handle: usize, byte: u8) -> Result<bool, ControlWriteError> {
        let on = match byte {
            b'x' | b'0' => false,
            b'1' => true,
            _ => return Err(ControlWriteError::Invalid),
        };
        if self.owner.is_some_and(|owner| owner != handle) {
            return Err(ControlWriteError::Busy);
        }
        if byte == b'x' {
            self.owner = Some(handle);
        }
        Ok(on)
    }

    /// Handle `handle` closed. Returns `Some(false)`, sampling off, when it owned the
    /// profiler, which is then free, and `None`, sampling left as it is, otherwise: an old
    /// owner closing after another handle took the profiler does not touch its sampling.
    pub fn close(&mut self, handle: usize) -> Option<bool> {
        if self.owner == Some(handle) {
            self.owner = None;
            Some(false)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    #[test]
    fn samples_split_a_buffer() {
        let words = [
            SAMPLE_MAGIC | 6,
            0xffff_8000_0010_0000,
            100,
            7,
            0xffff_8000_0010_0100,
            0x40_1000,
            SAMPLE_MAGIC | 4,
            0x40_2000,
            200,
            8,
        ];
        let all: std::vec::Vec<_> = Samples::new(&words).collect();
        assert_eq!(all.len(), 2);
        let first = all[0].expect("valid");
        assert_eq!(first.context, 7);
        assert_eq!(
            first.stack().collect::<std::vec::Vec<_>>(),
            [0xffff_8000_0010_0000, 0xffff_8000_0010_0100, 0x40_1000]
        );
        let second = all[1].expect("valid");
        assert_eq!(
            (second.ip, second.tsc, second.frames.len()),
            (0x40_2000, 200, 0)
        );
    }

    #[test]
    fn bad_samples_are_errors() {
        assert_eq!(
            Samples::new(&[1, 2, 3, 4]).collect::<std::vec::Vec<_>>(),
            [Err(SampleError::BadHeader { at: 0 })]
        );
        assert_eq!(
            Samples::new(&[SAMPLE_MAGIC | 5, 1, 2, 3]).collect::<std::vec::Vec<_>>(),
            [Err(SampleError::BadLength { at: 0 })]
        );
        assert_eq!(
            Samples::new(&[SAMPLE_MAGIC | 2, 1]).collect::<std::vec::Vec<_>>(),
            [Err(SampleError::BadLength { at: 0 })]
        );
        assert_eq!(Samples::new(&[]).count(), 0);
    }

    #[test]
    fn position_is_where_the_next_item_starts() {
        let words = [SAMPLE_MAGIC | 4, 1, 2, 3, 7, SAMPLE_MAGIC | 5, 1, 2, 3, 9];
        let mut samples = Samples::new(&words);
        assert_eq!(samples.position(), 0);
        assert!(samples.next().unwrap().is_ok());
        assert_eq!(samples.position(), 4);
        assert!(samples.next().unwrap().is_err());
        assert_eq!(samples.position(), 5);
        assert!(samples.next().unwrap().is_ok());
        assert_eq!(samples.position(), words.len());
    }

    /// A sample torn by a full buffer (its header promises more words than were written)
    /// takes in the start of the next one, so the words after it do not line up; that run is
    /// one error, and the samples after it still decode.
    #[test]
    fn a_torn_sample_does_not_hide_the_ones_after_it() {
        let words = [
            SAMPLE_MAGIC | 5,
            0x40_1000,
            7,
            SAMPLE_MAGIC | 4,
            0x40_2000,
            8,
            3,
            SAMPLE_MAGIC | 4,
            0x40_3000,
            9,
            4,
        ];
        let samples: std::vec::Vec<_> = Samples::new(&words).collect();
        assert_eq!(samples.len(), 3);
        assert!(samples[0].is_ok());
        assert_eq!(samples[1], Err(SampleError::BadHeader { at: 5 }));
        let last = samples[2].unwrap();
        assert_eq!((last.ip, last.context), (0x40_3000, 4));
    }

    #[test]
    fn control_line_round_trips() {
        let line = ControlLine {
            text_start: 0xffff_ffff_8020_0000,
        };
        assert_eq!(line.to_string(), "kernel_text 0xffffffff80200000\n");
        assert_eq!(ControlLine::parse(&line.to_string()), Ok(line));
        for bad in [
            "",
            "kernel_text",
            "kernel_text 0x",
            "kernel_text 12",
            "text 0x1",
        ] {
            assert_eq!(
                ControlLine::parse(bad),
                Err(ControlLineError::Format),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_late_close_cannot_stop_the_next_owner() {
        // Review 4320669936: A owns and samples, closes; B takes the profiler and starts. A
        // close of A arriving late (or twice) must leave B sampling.
        let mut owner = ProfilerOwner::new();
        assert_eq!(owner.write(1, b'x'), Ok(false));
        assert_eq!(owner.write(1, b'1'), Ok(true));
        assert_eq!(owner.close(1), Some(false));
        assert_eq!(owner.write(2, b'x'), Ok(false));
        assert_eq!(owner.write(2, b'1'), Ok(true));
        assert_eq!(owner.close(1), None);
        assert_eq!(owner.owner(), Some(2));
        // Handles that never owned it neither stop it nor take it.
        assert_eq!(owner.close(3), None);
        assert_eq!(owner.write(3, b'0'), Err(ControlWriteError::Busy));
        assert_eq!(owner.write(3, b'x'), Err(ControlWriteError::Busy));
        assert_eq!(owner.close(2), Some(false));
        assert_eq!(owner.owner(), None);
    }

    #[test]
    fn without_an_owner_writes_are_upstreams() {
        let mut owner = ProfilerOwner::new();
        assert_eq!(owner.write(7, b'1'), Ok(true));
        assert_eq!(owner.write(8, b'0'), Ok(false));
        assert_eq!(owner.write(8, b'2'), Err(ControlWriteError::Invalid));
        assert_eq!(owner.owner(), None);
        // Taking it twice is fine for the owner.
        assert_eq!(owner.write(8, b'x'), Ok(false));
        assert_eq!(owner.write(8, b'x'), Ok(false));
        assert_eq!(owner.owner(), Some(8));
        assert!(!ControlWriteError::Busy.to_string().is_empty());
    }
}
