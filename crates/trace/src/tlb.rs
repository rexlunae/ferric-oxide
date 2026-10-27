//! TLB and address-space switch counters: the kernel's per-CPU counts behind
//! `/scheme/sys/tlb` (`kernel.tlb_stats`), and the file's line format.
//!
//! The kernel keeps one [`CpuCounters`] in each CPU's per-CPU block and adds to it where it
//! loads CR3, invalidates TLB entries, sends or answers a TLB shootdown, and invalidates an
//! IOMMU's IOTLB. Each [`Counter`] says what it counts and where. The `sys` scheme writes the
//! counters with this module's [`fmt::Display`] implementations, and tools read them back
//! with [`Snapshot::parse`], so the format has one definition:
//!
//! ```text
//! tlb kpti on pcid off
//! cpu 0 syscalls 812 irqs 40 context_switches 96 cr3_switch 51 cr3_empty 9 cr3_same 36 ...
//! cpu 1 syscalls 77 irqs 12 context_switches 31 cr3_switch 18 cr3_empty 4 cr3_same 9 ...
//! ```
//!
//! - A [`ModeLine`] comes first: whether kernel page-table isolation (KPTI) is on, and
//!   whether it uses PCIDs. They say what a CR3 load costs: every CR3 load the kernel
//!   makes flushes the TLB of the kernel's PCID, and with PCID also the user PCID; without
//!   PCID, KPTI also flushes it twice per system call or interrupt from user space.
//! - A [`CpuLine`] per CPU: `cpu <n>`, then every [`Counter`] as `<name> <value>`, in
//!   [`Counter::ALL`] order.
//!
//! The counters only grow (they wrap at `u64::MAX`). A file read is one snapshot, taken when
//! the read starts; a benchmark reads the file before and after a run and subtracts
//! ([`Counters::delta`]). There is no reset, so concurrent readers never disturb each other.
//!
//! # Examples
//!
//! ```
//! use ferric_trace::tlb::{Counter, Snapshot};
//!
//! let before = Snapshot::parse("tlb kpti on pcid on\ncpu 0 shootdowns 4\ncpu 1 shootdowns 1\n")
//!     .unwrap();
//! let after = Snapshot::parse("tlb kpti on pcid on\ncpu 0 shootdowns 9\ncpu 1 shootdowns 2\n")
//!     .unwrap();
//! let run = after.total.delta(&before.total);
//! assert_eq!(run.get(Counter::Shootdowns), 6);
//! ```

use core::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

/// The file's name in the `sys` scheme: `/scheme/sys/tlb`.
pub const SYS_FILE: &str = "tlb";

/// One of the per-CPU counts. The name is the key in a [`CpuLine`].
///
/// "Local" counts are of work this CPU did on its own TLB; "shootdown" counts on the CPU that
/// changed an address space and had to make other CPUs flush too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum Counter {
    /// System calls entered on this CPU (x86_64 `syscall` instruction). With KPTI each is a
    /// pair of CR3 switches (to the kernel table and back), which flush without PCID.
    Syscalls,
    /// Interrupts this CPU handled, the `sys:stat` count (`CpuStats::irq`), repeated here so
    /// one snapshot holds everything a run needs.
    Irqs,
    /// Context switches on this CPU, the `sys:stat` count, repeated here likewise.
    ContextSwitches,
    /// Context switches that loaded another address space's table into CR3. The kernel never
    /// sets CR3's no-flush bit there, so each flushed this CPU's non-global TLB entries (and,
    /// with KPTI and PCID, the user PCID's too).
    Cr3Switch,
    /// Context switches to a context without an address space (a kernel context), which load
    /// the empty table into CR3: a flush like [`Counter::Cr3Switch`].
    Cr3Empty,
    /// Context switches that kept the address space (threads of one process, or two kernel
    /// contexts): no CR3 load, no flush.
    Cr3Same,
    /// Single-page invalidations (`invlpg`, plus `invpcid` for the user PCID with KPTI and
    /// PCID) on the paths that change user mappings: page faults (demand paging, copy on
    /// write) and the grant code. Boot-time and kernel-text changes are not counted.
    FlushPage,
    /// Full flushes of this CPU's TLB other than address-space switches: the local flush of a
    /// `Flusher` batch, and each shootdown request this CPU answered
    /// ([`Counter::ShootdownsHandled`]). Each reloads CR3.
    FlushFull,
    /// `Flusher` batches flushed by this CPU: one per change to a user address space's
    /// mappings (or per 32 queued pages of a larger one) that needed TLB maintenance.
    FlushBatches,
    /// Queued page entries in those batches. A batch can have none (a change that only
    /// marked it dirty).
    FlushBatchPages,
    /// Batches that also made other CPUs flush, because the address space was loaded on
    /// them: shootdowns this CPU started.
    Shootdowns,
    /// The other CPUs those shootdowns waited for (target CPUs, summed over shootdowns).
    ShootdownTargets,
    /// Queued page entries in the batches that were shootdowns.
    ShootdownPages,
    /// TSC ticks this CPU spun waiting for the targets' acknowledgements, summed over its
    /// shootdowns. The TSC is not calibrated here; divide by its frequency for time.
    ShootdownWaitCycles,
    /// TLB shootdown IPIs this CPU sent.
    TlbIpiSent,
    /// TLB shootdown IPIs this CPU took. One can find its request already answered, by the
    /// CPU's own polling while it waited on a shootdown of its own or at a context switch.
    TlbIpiReceived,
    /// Shootdown requests this CPU answered with a full flush, whether in the IPI handler,
    /// while polling or at a context switch.
    ShootdownsHandled,
    /// IOMMU domain operations (DMA map, unmap, protect) on this CPU that invalidated the
    /// IOTLB of at least one unit.
    IotlbFlushes,
    /// IOTLB invalidations of single units for those (one per attached unit).
    IotlbUnitFlushes,
    /// Bytes of IOVA space those operations invalidated (once per operation, not per unit).
    IotlbBytes,
    /// TSC ticks spent in those invalidations, waiting for the units to complete them.
    IotlbCycles,
}

impl Counter {
    /// The number of counters.
    pub const COUNT: usize = 21;

    /// Every counter, in the order a [`CpuLine`] lists them.
    pub const ALL: [Counter; Self::COUNT] = [
        Counter::Syscalls,
        Counter::Irqs,
        Counter::ContextSwitches,
        Counter::Cr3Switch,
        Counter::Cr3Empty,
        Counter::Cr3Same,
        Counter::FlushPage,
        Counter::FlushFull,
        Counter::FlushBatches,
        Counter::FlushBatchPages,
        Counter::Shootdowns,
        Counter::ShootdownTargets,
        Counter::ShootdownPages,
        Counter::ShootdownWaitCycles,
        Counter::TlbIpiSent,
        Counter::TlbIpiReceived,
        Counter::ShootdownsHandled,
        Counter::IotlbFlushes,
        Counter::IotlbUnitFlushes,
        Counter::IotlbBytes,
        Counter::IotlbCycles,
    ];

    /// The counter's key in the file.
    pub const fn name(self) -> &'static str {
        match self {
            Counter::Syscalls => "syscalls",
            Counter::Irqs => "irqs",
            Counter::ContextSwitches => "context_switches",
            Counter::Cr3Switch => "cr3_switch",
            Counter::Cr3Empty => "cr3_empty",
            Counter::Cr3Same => "cr3_same",
            Counter::FlushPage => "flush_page",
            Counter::FlushFull => "flush_full",
            Counter::FlushBatches => "flush_batches",
            Counter::FlushBatchPages => "flush_batch_pages",
            Counter::Shootdowns => "shootdowns",
            Counter::ShootdownTargets => "shootdown_targets",
            Counter::ShootdownPages => "shootdown_pages",
            Counter::ShootdownWaitCycles => "shootdown_wait_cycles",
            Counter::TlbIpiSent => "tlb_ipi_sent",
            Counter::TlbIpiReceived => "tlb_ipi_received",
            Counter::ShootdownsHandled => "shootdowns_handled",
            Counter::IotlbFlushes => "iotlb_flushes",
            Counter::IotlbUnitFlushes => "iotlb_unit_flushes",
            Counter::IotlbBytes => "iotlb_bytes",
            Counter::IotlbCycles => "iotlb_cycles",
        }
    }

    /// The counter named `name`, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One CPU's counters as the kernel keeps them, in its per-CPU block.
///
/// Only the owning CPU adds to them, with a relaxed load and store rather than a locked
/// read-modify-write, as `CpuStats` does: other CPUs only read. The kernel adds with
/// interrupts off, so an interrupt cannot add to the same counter between the load and the
/// store; where that could still happen, a count is lost, never corrupted.
#[derive(Debug)]
pub struct CpuCounters([AtomicU64; Counter::COUNT]);

impl CpuCounters {
    /// Every counter at zero.
    pub const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; Counter::COUNT])
    }

    /// Adds `n` to `counter`, wrapping. Call it only on the CPU that owns these counters.
    #[inline(always)]
    pub fn add(&self, counter: Counter, n: u64) {
        let cell = &self.0[counter.index()];
        cell.store(
            cell.load(Ordering::Relaxed).wrapping_add(n),
            Ordering::Relaxed,
        );
    }

    /// The counters' values now, with each `(counter, value)` of `extra` put in place of the
    /// counter's own: for the counts the kernel keeps elsewhere ([`Counter::Irqs`],
    /// [`Counter::ContextSwitches`]) and copies in when it makes a snapshot.
    pub fn snapshot_with(&self, extra: &[(Counter, u64)]) -> Counters {
        let mut out = Counters::new();
        for counter in Counter::ALL {
            out.0[counter.index()] = self.0[counter.index()].load(Ordering::Relaxed);
        }
        for &(counter, value) in extra {
            out.set(counter, value);
        }
        out
    }
}

impl Default for CpuCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// A set of counter values: one CPU's, or a sum or difference of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters([u64; Counter::COUNT]);

impl Counters {
    /// Every counter at zero.
    pub const fn new() -> Self {
        Self([0; Counter::COUNT])
    }

    /// The value of `counter`.
    pub const fn get(&self, counter: Counter) -> u64 {
        self.0[counter.index()]
    }

    /// Sets `counter` to `value`.
    pub fn set(&mut self, counter: Counter, value: u64) {
        self.0[counter.index()] = value;
    }

    /// The sum of these counters and `other`'s, wrapping.
    #[must_use]
    pub fn sum(&self, other: &Self) -> Self {
        let mut out = *self;
        for (a, b) in out.0.iter_mut().zip(other.0) {
            *a = a.wrapping_add(b);
        }
        out
    }

    /// How much each counter grew since `earlier`, wrapping, as the counters do.
    #[must_use]
    pub fn delta(&self, earlier: &Self) -> Self {
        let mut out = *self;
        for (a, b) in out.0.iter_mut().zip(earlier.0) {
            *a = a.wrapping_sub(b);
        }
        out
    }

    /// Address-space switches that loaded CR3: [`Counter::Cr3Switch`] plus
    /// [`Counter::Cr3Empty`].
    pub const fn cr3_loads(&self) -> u64 {
        self.get(Counter::Cr3Switch)
            .wrapping_add(self.get(Counter::Cr3Empty))
    }

    /// Local TLB flushes of either kind: [`Counter::FlushPage`] plus [`Counter::FlushFull`].
    /// Address-space switches ([`Counters::cr3_loads`]) are not included.
    pub const fn local_flushes(&self) -> u64 {
        self.get(Counter::FlushPage)
            .wrapping_add(self.get(Counter::FlushFull))
    }
}

/// The first line: what a CR3 load means on this boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeLine {
    /// Kernel page-table isolation is on: every entry from and return to user space switches
    /// CR3.
    pub kpti: bool,
    /// KPTI uses PCIDs, so those entry and exit switches do not flush.
    pub pcid: bool,
}

impl fmt::Display for ModeLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "tlb kpti {} pcid {}",
            on_off(self.kpti),
            on_off(self.pcid)
        )
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// One CPU's line: its logical id and every counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuLine {
    /// The logical CPU id.
    pub cpu: u32,
    /// Its counters.
    pub counters: Counters,
}

impl fmt::Display for CpuLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cpu {}", self.cpu)?;
        for counter in Counter::ALL {
            write!(f, " {} {}", counter.name(), self.counters.get(counter))?;
        }
        f.write_str("\n")
    }
}

/// Why the file does not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// The first line is not a `tlb kpti <on|off> pcid <on|off>` line.
    Mode,
    /// A later line does not start with `cpu <n>`.
    Cpu,
    /// A line has a key without a value.
    Fields,
    /// A key is not a [`Counter`] name.
    UnknownCounter,
    /// A value is not a number.
    Number,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParseError::Mode => "the first line is not a tlb mode line",
            ParseError::Cpu => "a line does not start with cpu <n>",
            ParseError::Fields => "a key has no value",
            ParseError::UnknownCounter => "unknown counter",
            ParseError::Number => "a value is not a number",
        })
    }
}

impl core::error::Error for ParseError {}

/// A whole file read: the mode, and the counters summed over every CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The mode line.
    pub mode: ModeLine,
    /// Every CPU's counters, summed.
    pub total: Counters,
    /// The number of CPU lines.
    pub cpus: u32,
}

impl Snapshot {
    /// Parses a file read. Counters a line leaves out count as zero, so a reader keeps
    /// working with a kernel that lists fewer.
    ///
    /// # Errors
    ///
    /// A [`ParseError`] for the first line that does not parse.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut lines = text.lines();
        let mode = parse_mode(lines.next().ok_or(ParseError::Mode)?)?;
        let mut total = Counters::new();
        let mut cpus = 0_u32;
        for line in lines.filter(|line| !line.is_empty()) {
            total = total.sum(&parse_cpu(line)?.counters);
            cpus = cpus.saturating_add(1);
        }
        Ok(Self { mode, total, cpus })
    }
}

fn parse_mode(line: &str) -> Result<ModeLine, ParseError> {
    let flag = |word: Option<&str>| match word {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        _ => Err(ParseError::Mode),
    };
    let mut words = line.split_ascii_whitespace();
    if (words.next(), words.next()) != (Some("tlb"), Some("kpti")) {
        return Err(ParseError::Mode);
    }
    let kpti = flag(words.next())?;
    if words.next() != Some("pcid") {
        return Err(ParseError::Mode);
    }
    let pcid = flag(words.next())?;
    if words.next().is_some() {
        return Err(ParseError::Mode);
    }
    Ok(ModeLine { kpti, pcid })
}

/// Parses one `cpu <n> <name> <value> ...` line.
///
/// # Errors
///
/// [`ParseError::Cpu`] when the line does not start with `cpu <n>`, [`ParseError::Fields`]
/// when a key has no value, [`ParseError::UnknownCounter`] for a key that is not a counter and
/// [`ParseError::Number`] for a value that is not a number.
pub fn parse_cpu(line: &str) -> Result<CpuLine, ParseError> {
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("cpu") {
        return Err(ParseError::Cpu);
    }
    let cpu = words
        .next()
        .and_then(|w| w.parse().ok())
        .ok_or(ParseError::Cpu)?;
    let mut counters = Counters::new();
    while let Some(key) = words.next() {
        let value = words.next().ok_or(ParseError::Fields)?;
        let counter = Counter::from_name(key).ok_or(ParseError::UnknownCounter)?;
        counters.set(counter, value.parse().map_err(|_| ParseError::Number)?);
    }
    Ok(CpuLine { cpu, counters })
}

/// A run's counter deltas per I/O operation, as `key=value` fields for a `FERRIC_BENCH`
/// line, with three decimals:
///
/// `tlb_ops=… flushes_per_op=… flush_full_per_op=… flush_page_per_op=…
/// shootdowns_per_op=… tlb_ipis_per_op=… cr3_loads_per_op=… syscalls_per_op=…
/// iotlb_flushes_per_op=… targets_per_shootdown=… pages_per_shootdown=…
/// wait_cycles_per_shootdown=…`
///
/// "Flushes" are local flushes of both kinds on every CPU ([`Counters::local_flushes`]);
/// CR3 loads are counted separately. The per-shootdown figures are 0 without shootdowns.
#[derive(Clone, Copy, Debug)]
pub struct PerOp<'a> {
    /// The counters' growth over the run, summed over CPUs.
    pub delta: &'a Counters,
    /// The I/O operations the run completed.
    pub ops: u64,
}

impl fmt::Display for PerOp<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.delta;
        let shootdowns = d.get(Counter::Shootdowns);
        write!(f, "tlb_ops={}", self.ops)?;
        let per_op = [
            ("flushes_per_op", d.local_flushes()),
            ("flush_full_per_op", d.get(Counter::FlushFull)),
            ("flush_page_per_op", d.get(Counter::FlushPage)),
            ("shootdowns_per_op", shootdowns),
            ("tlb_ipis_per_op", d.get(Counter::TlbIpiSent)),
            ("cr3_loads_per_op", d.cr3_loads()),
            ("syscalls_per_op", d.get(Counter::Syscalls)),
            ("iotlb_flushes_per_op", d.get(Counter::IotlbFlushes)),
        ];
        for (key, amount) in per_op {
            write!(f, " {key}={}", Ratio(amount, self.ops))?;
        }
        let per_shootdown = [
            ("targets_per_shootdown", d.get(Counter::ShootdownTargets)),
            ("pages_per_shootdown", d.get(Counter::ShootdownPages)),
            (
                "wait_cycles_per_shootdown",
                d.get(Counter::ShootdownWaitCycles),
            ),
        ];
        for (key, amount) in per_shootdown {
            write!(f, " {key}={}", Ratio(amount, shootdowns))?;
        }
        Ok(())
    }
}

/// `amount / per` with three decimals, rounded down; `0.000` when `per` is zero.
struct Ratio(u64, u64);

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Ratio(amount, per) = *self;
        let thousandths = match per {
            0 => 0,
            per => u128::from(amount) * 1000 / u128::from(per),
        };
        write!(f, "{}.{:03}", thousandths / 1000, thousandths % 1000)
    }
}

#[cfg(test)]
mod tests {
    use std::{format, string::ToString};

    use super::*;

    #[test]
    fn names_are_unique_and_in_order() {
        for (i, counter) in Counter::ALL.into_iter().enumerate() {
            assert_eq!(counter.index(), i);
            assert_eq!(Counter::from_name(counter.name()), Some(counter));
            assert_eq!(
                Counter::ALL
                    .iter()
                    .filter(|c| c.name() == counter.name())
                    .count(),
                1
            );
        }
        assert_eq!(Counter::from_name("nope"), None);
    }

    #[test]
    fn the_file_round_trips() {
        let a = CpuCounters::new();
        a.add(Counter::Cr3Switch, 3);
        a.add(Counter::Shootdowns, 2);
        a.add(Counter::ShootdownWaitCycles, 1000);
        let b = CpuCounters::new();
        b.add(Counter::Cr3Switch, 1);
        b.add(Counter::FlushPage, 5);
        let mode = ModeLine {
            kpti: true,
            pcid: false,
        };
        let text = format!(
            "{mode}{}{}",
            CpuLine {
                cpu: 0,
                counters: a.snapshot_with(&[(Counter::Irqs, 7)]),
            },
            CpuLine {
                cpu: 1,
                counters: b.snapshot_with(&[]),
            }
        );
        assert!(text.starts_with("tlb kpti on pcid off\ncpu 0 syscalls 0 irqs 7 "));
        let snap = Snapshot::parse(&text).unwrap();
        assert_eq!(snap.mode, mode);
        assert_eq!(snap.cpus, 2);
        assert_eq!(snap.total.get(Counter::Cr3Switch), 4);
        assert_eq!(snap.total.get(Counter::Irqs), 7);
        assert_eq!(snap.total.get(Counter::FlushPage), 5);
        assert_eq!(snap.total.get(Counter::ShootdownWaitCycles), 1000);
    }

    #[test]
    fn bad_files_are_refused() {
        assert_eq!(Snapshot::parse(""), Err(ParseError::Mode));
        assert_eq!(
            Snapshot::parse("tlb kpti maybe pcid on"),
            Err(ParseError::Mode)
        );
        assert_eq!(
            Snapshot::parse("tlb kpti on pcid on\nprocessor 0"),
            Err(ParseError::Cpu)
        );
        assert_eq!(
            Snapshot::parse("tlb kpti on pcid on\ncpu 0 shootdowns"),
            Err(ParseError::Fields)
        );
        assert_eq!(
            Snapshot::parse("tlb kpti on pcid on\ncpu 0 frobs 1"),
            Err(ParseError::UnknownCounter)
        );
        assert_eq!(
            Snapshot::parse("tlb kpti on pcid on\ncpu 0 shootdowns x"),
            Err(ParseError::Number)
        );
        assert!(!ParseError::Number.to_string().is_empty());
    }

    #[test]
    fn deltas_wrap_like_the_counters() {
        let mut before = Counters::new();
        before.set(Counter::FlushFull, u64::MAX);
        let mut after = Counters::new();
        after.set(Counter::FlushFull, 1);
        assert_eq!(after.delta(&before).get(Counter::FlushFull), 2);
    }

    #[test]
    fn per_op_figures() {
        let mut d = Counters::new();
        d.set(Counter::FlushFull, 3);
        d.set(Counter::FlushPage, 1);
        d.set(Counter::Shootdowns, 2);
        d.set(Counter::ShootdownTargets, 6);
        d.set(Counter::ShootdownPages, 5);
        d.set(Counter::ShootdownWaitCycles, 9001);
        d.set(Counter::Cr3Switch, 7);
        d.set(Counter::Cr3Empty, 1);
        let line = PerOp { delta: &d, ops: 8 }.to_string();
        assert_eq!(
            line,
            "tlb_ops=8 flushes_per_op=0.500 flush_full_per_op=0.375 flush_page_per_op=0.125 \
             shootdowns_per_op=0.250 tlb_ipis_per_op=0.000 cr3_loads_per_op=1.000 \
             syscalls_per_op=0.000 iotlb_flushes_per_op=0.000 targets_per_shootdown=3.000 \
             pages_per_shootdown=2.500 wait_cycles_per_shootdown=4500.500"
        );
        let none = PerOp {
            delta: &Counters::new(),
            ops: 0,
        }
        .to_string();
        assert!(none.contains("flushes_per_op=0.000"));
    }
}
