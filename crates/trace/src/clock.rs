//! The clock behind record timestamps, and how the kernel picks it (roadmap T3d).
//!
//! A record's second word is a timestamp, compared across CPUs to order records and
//! subtracted to get durations. The kernel takes it from one [`Clock`] for the whole boot,
//! chosen once by [`choose`] from what the machine offers:
//!
//! 1. [`Clock::Tsc`] when the TSC is invariant: CPUID leaf `0x8000_0007` reports it in EDX
//!    bit 8 ([`invariant_tsc`]). Such a TSC ticks at a constant rate through P-, C- and
//!    T-state changes, and the CPUs' TSCs are synchronized at reset, so one `RDTSC` is
//!    the whole cost. This is documented in the Intel SDM, Vol. 3B, section 18.17.1
//!    ("Invariant TSC"), and in AMD's APM, Vol. 3, CPUID `Fn8000_0007_EDX` bit 8
//!    (`TscInvariant`).
//! 2. [`Clock::KvmClock`] when the TSC is not invariant but a KVM host offers a stable
//!    paravirtual clock (`KVM_FEATURE_CLOCKSOURCE2` and `KVM_FEATURE_CLOCKSOURCE_STABLE_BIT`).
//!    Its readings are nanoseconds and agree across CPUs. Hosts often hide the invariant TSC
//!    bit from guests because the bit does not survive migration, so this is the usual clock
//!    in a VM.
//! 3. [`Clock::Hpet`] when neither is there but the HPET has a 64-bit main counter. It is
//!    one counter for all CPUs, read through MMIO, which is slower, so it comes last.
//! 4. Otherwise [`Clock::Tsc`] anyway, with [`Choice::reliable`] false: timestamps on
//!    different CPUs may then drift apart, and the kernel says so in its log.
//!
//! The `kernel.trace` scheme's `clock` file ([`crate::abi::CLOCK_PATH`]) holds one
//! [`ClockLine`]: the clock, and the length of one tick in femtoseconds when the kernel
//! knows it. Decoders use the line to turn timestamps into time. For the TSC the length
//! is not known (the kernel does not calibrate it), so a decoder is told the frequency.

use core::fmt;

use crate::abi::LineError;

/// The CPUID leaf whose EDX bit [`INVARIANT_TSC_BIT`] says the TSC is invariant.
pub const INVARIANT_TSC_LEAF: u32 = 0x8000_0007;

/// The bit of CPUID `0x8000_0007` EDX that says the TSC is invariant.
pub const INVARIANT_TSC_BIT: u32 = 8;

/// Whether EDX of CPUID leaf [`INVARIANT_TSC_LEAF`] reports an invariant TSC.
///
/// `max_extended_leaf` is EAX of CPUID `0x8000_0000`: a CPU whose highest extended leaf is
/// lower has no such leaf, and the EDX value means nothing.
///
/// # Examples
///
/// ```
/// use ferric_trace::clock::invariant_tsc;
///
/// assert!(invariant_tsc(0x8000_0008, 1 << 8));
/// assert!(!invariant_tsc(0x8000_0008, 0));
/// assert!(!invariant_tsc(0x8000_0004, 1 << 8));
/// ```
pub const fn invariant_tsc(max_extended_leaf: u32, edx: u32) -> bool {
    max_extended_leaf >= INVARIANT_TSC_LEAF && edx & (1 << INVARIANT_TSC_BIT) != 0
}

/// Where record timestamps come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Clock {
    /// The time stamp counter (`RDTSC`), in cycles.
    Tsc = 0,
    /// The KVM paravirtual clock, in nanoseconds.
    KvmClock = 1,
    /// The HPET main counter, in its ticks.
    Hpet = 2,
}

impl Clock {
    /// Every clock, in [`Clock::raw`] order.
    pub const ALL: [Clock; 3] = [Clock::Tsc, Clock::KvmClock, Clock::Hpet];

    /// The clock's name in a [`ClockLine`].
    pub const fn name(self) -> &'static str {
        match self {
            Clock::Tsc => "tsc",
            Clock::KvmClock => "kvmclock",
            Clock::Hpet => "hpet",
        }
    }

    /// The clock with name `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|clock| clock.name() == name)
    }

    /// The clock as a number, for storing it in an atomic.
    pub const fn raw(self) -> u8 {
        self as u8
    }

    /// The clock numbered `raw`.
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Clock::Tsc),
            1 => Some(Clock::KvmClock),
            2 => Some(Clock::Hpet),
            _ => None,
        }
    }
}

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The length of one KVM clock tick: a nanosecond, in femtoseconds.
pub const KVM_CLOCK_PERIOD_FS: u64 = 1_000_000;

/// What the machine offers, as [`choose`] needs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offered {
    /// The TSC is invariant ([`invariant_tsc`]).
    pub invariant_tsc: bool,
    /// A stable KVM paravirtual clock is set up on the CPUs.
    pub kvm_clock: bool,
    /// The HPET's main counter is 64 bits wide and running, with this tick length in
    /// femtoseconds.
    pub hpet_period_fs: Option<u64>,
}

/// The clock [`choose`] picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The clock.
    pub clock: Clock,
    /// Its tick length in femtoseconds, when known.
    pub period_fs: Option<u64>,
    /// Whether timestamps from different CPUs can be compared: false only for a TSC that is
    /// not invariant, which [`choose`] falls back to when nothing better is there.
    pub reliable: bool,
}

impl Choice {
    /// Why [`choose`] picked this clock, for the kernel's log.
    pub const fn reason(self) -> &'static str {
        match (self.clock, self.reliable) {
            (Clock::Tsc, true) => "the TSC is invariant",
            (Clock::KvmClock, _) => "the TSC is not invariant; the KVM clock is stable",
            (Clock::Hpet, _) => "the TSC is not invariant and there is no KVM clock",
            (Clock::Tsc, false) => {
                "the TSC is not invariant and there is no other clock: timestamps of different CPUs may disagree"
            }
        }
    }

    /// The [`ClockLine`] the scheme serves for this choice.
    pub const fn line(self) -> ClockLine {
        ClockLine {
            clock: self.clock,
            period_fs: self.period_fs,
        }
    }
}

/// Picks the timestamp clock from what the machine offers, in the order the
/// [module docs](self) give.
///
/// # Examples
///
/// ```
/// use ferric_trace::clock::{Clock, Offered, choose};
///
/// let vm = Offered { kvm_clock: true, ..Offered::default() };
/// assert_eq!(choose(vm).clock, Clock::KvmClock);
/// let bare = Offered { invariant_tsc: true, kvm_clock: true, ..Offered::default() };
/// assert_eq!(choose(bare).clock, Clock::Tsc);
/// ```
pub const fn choose(offered: Offered) -> Choice {
    if offered.invariant_tsc {
        return Choice {
            clock: Clock::Tsc,
            period_fs: None,
            reliable: true,
        };
    }
    if offered.kvm_clock {
        return Choice {
            clock: Clock::KvmClock,
            period_fs: Some(KVM_CLOCK_PERIOD_FS),
            reliable: true,
        };
    }
    if let Some(period_fs) = offered.hpet_period_fs {
        return Choice {
            clock: Clock::Hpet,
            period_fs: Some(period_fs),
            reliable: true,
        };
    }
    Choice {
        clock: Clock::Tsc,
        period_fs: None,
        reliable: false,
    }
}

/// The line of the `clock` file: `clock <name> period_fs <femtoseconds|unknown>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockLine {
    /// The clock timestamps come from.
    pub clock: Clock,
    /// The length of one tick in femtoseconds, when the kernel knows it.
    pub period_fs: Option<u64>,
}

impl ClockLine {
    /// The field written for an unknown tick length.
    pub const UNKNOWN: &'static str = "unknown";

    /// Parses the line, with or without its `\n`.
    ///
    /// # Errors
    ///
    /// [`LineError::Fields`] for the wrong fields, [`LineError::Clock`] for a clock name
    /// this crate does not know, and [`LineError::Number`] for a tick length that is neither
    /// a positive number nor `unknown`.
    pub fn parse(line: &str) -> Result<Self, LineError> {
        let [clock_tag, name, period_tag, period] = crate::abi::fields(line)?;
        if (clock_tag, period_tag) != ("clock", "period_fs") {
            return Err(LineError::Fields);
        }
        let clock = Clock::from_name(name).ok_or(LineError::Clock)?;
        let period_fs = match period {
            Self::UNKNOWN => None,
            digits => match crate::abi::number::<u64>(digits)? {
                0 => return Err(LineError::Number),
                fs => Some(fs),
            },
        };
        Ok(Self { clock, period_fs })
    }

    /// Ticks per microsecond, when the tick length is known: what a decoder divides tick
    /// counts by to get microseconds.
    pub fn ticks_per_us(&self) -> Option<f64> {
        self.period_fs.map(|fs| 1e9 / fs as f64)
    }
}

impl fmt::Display for ClockLine {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.period_fs {
            Some(fs) => writeln!(f, "clock {} period_fs {fs}", self.clock),
            None => writeln!(f, "clock {} period_fs {}", self.clock, Self::UNKNOWN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn invariant_tsc_needs_the_leaf_and_the_bit() {
        assert!(invariant_tsc(INVARIANT_TSC_LEAF, 1 << INVARIANT_TSC_BIT));
        assert!(invariant_tsc(0x8000_0021, u32::MAX));
        assert!(!invariant_tsc(0x8000_0021, !(1 << INVARIANT_TSC_BIT)));
        assert!(!invariant_tsc(INVARIANT_TSC_LEAF - 1, u32::MAX));
    }

    #[test]
    fn choice_order() {
        let none = Offered::default();
        let hpet = Offered {
            hpet_period_fs: Some(10_000_000),
            ..none
        };
        let kvm = Offered {
            kvm_clock: true,
            ..hpet
        };
        let tsc = Offered {
            invariant_tsc: true,
            ..kvm
        };
        assert_eq!(
            choose(tsc),
            Choice {
                clock: Clock::Tsc,
                period_fs: None,
                reliable: true
            }
        );
        assert_eq!(choose(kvm).clock, Clock::KvmClock);
        assert_eq!(choose(kvm).period_fs, Some(KVM_CLOCK_PERIOD_FS));
        assert_eq!(choose(hpet).clock, Clock::Hpet);
        assert_eq!(choose(hpet).period_fs, Some(10_000_000));
        assert_eq!(
            choose(none),
            Choice {
                clock: Clock::Tsc,
                period_fs: None,
                reliable: false
            }
        );
    }

    #[test]
    fn every_choice_has_a_reason() {
        let mut reasons = std::vec::Vec::new();
        for invariant_tsc in [false, true] {
            for kvm_clock in [false, true] {
                for hpet_period_fs in [None, Some(1)] {
                    let choice = choose(Offered {
                        invariant_tsc,
                        kvm_clock,
                        hpet_period_fs,
                    });
                    reasons.push(choice.reason());
                }
            }
        }
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), 4);
    }

    #[test]
    fn clocks_round_trip() {
        for clock in Clock::ALL {
            assert_eq!(Clock::from_raw(clock.raw()), Some(clock));
            assert_eq!(Clock::from_name(clock.name()), Some(clock));
        }
        assert_eq!(Clock::from_raw(3), None);
        assert_eq!(Clock::from_name("rtc"), None);
    }

    #[test]
    fn lines_round_trip() {
        for line in [
            ClockLine {
                clock: Clock::Tsc,
                period_fs: None,
            },
            ClockLine {
                clock: Clock::KvmClock,
                period_fs: Some(KVM_CLOCK_PERIOD_FS),
            },
            ClockLine {
                clock: Clock::Hpet,
                period_fs: Some(69_841_279),
            },
        ] {
            assert_eq!(ClockLine::parse(&line.to_string()), Ok(line));
        }
        assert_eq!(
            ClockLine {
                clock: Clock::Tsc,
                period_fs: None
            }
            .to_string(),
            "clock tsc period_fs unknown\n"
        );
    }

    #[test]
    fn bad_lines() {
        assert_eq!(
            ClockLine::parse("clock rtc period_fs 1"),
            Err(LineError::Clock)
        );
        assert_eq!(
            ClockLine::parse("clock tsc period_fs 0"),
            Err(LineError::Number)
        );
        assert_eq!(
            ClockLine::parse("clock tsc period_fs x"),
            Err(LineError::Number)
        );
        assert_eq!(ClockLine::parse("clock tsc"), Err(LineError::Fields));
        assert_eq!(
            ClockLine::parse("time tsc period_fs 1"),
            Err(LineError::Fields)
        );
    }

    #[test]
    fn ticks_per_us() {
        let kvm = choose(Offered {
            kvm_clock: true,
            ..Offered::default()
        })
        .line();
        assert_eq!(kvm.ticks_per_us(), Some(1000.0));
        let hpet = ClockLine {
            clock: Clock::Hpet,
            period_fs: Some(10_000_000),
        };
        assert_eq!(hpet.ticks_per_us(), Some(100.0));
        assert_eq!(
            ClockLine {
                clock: Clock::Tsc,
                period_fs: None
            }
            .ticks_per_us(),
            None
        );
    }
}
