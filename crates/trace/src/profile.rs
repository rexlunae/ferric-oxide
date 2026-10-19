//! The kernel profiler's interface (`kernel.profiling`): its sample format and its control
//! file, shared by the kernel and `ferric-trace profile` / `flamegraph`.
//!
//! The profiler is upstream's: a CPU set aside for it sends an NMI to every other CPU on a
//! timer, and each NMI writes one *sample* to its CPU's buffer, read (destructively) from
//! `/scheme/debug/profiling-<n>` by root. Writing `1` or `0` to `/scheme/debug/ctl-profiling`
//! starts or stops sampling. Ferric Oxide adds a read of `ctl-profiling`, which gives one
//! [`ControlLine`]: the address the kernel's text was loaded at, so that a host tool can
//! symbolize kernel addresses under KASLR (`kernel.kaslr_image`).
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
//! [`Samples`] splits a buffer into [`Sample`]s.

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

/// The samples of a buffer, in order. Iteration stops at the first error.
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
}

impl<'a> Iterator for Samples<'a> {
    type Item = Result<Sample<'a>, SampleError>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.at;
        let header = *self.words.get(at)?;
        if header & !LENGTH_MASK != SAMPLE_MAGIC {
            self.at = self.words.len();
            return Some(Err(SampleError::BadHeader { at }));
        }
        let len = (header & LENGTH_MASK) as usize;
        let Some(sample) = self
            .words
            .get(at..at.saturating_add(len))
            .filter(|_| len >= SAMPLE_FIXED_WORDS)
        else {
            self.at = self.words.len();
            return Some(Err(SampleError::BadLength { at }));
        };
        self.at = at + len;
        Some(Ok(Sample {
            ip: sample[1],
            tsc: sample[2],
            // The kernel stores a u32 widened to a word.
            context: sample[3] as u32,
            frames: &sample[SAMPLE_FIXED_WORDS..],
        }))
    }
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
    fn bad_samples_stop_the_split() {
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
}
