//! Per-CPU interrupt delivery for Ferric Oxide (roadmap Phase 2.4).
//!
//! The kernel's `irq:` scheme hands out interrupt vectors per CPU (`irq:cpu-XX/N`). This crate
//! holds what the kernel and the drivers that use those vectors must agree on, in one place:
//!
//! - [`CpuId`] (a logical CPU, as the kernel numbers them and as `irq:cpu-XX` names them) and
//!   [`ApicId`] (what an MSI message's destination field holds), kept apart by type because
//!   mixing them misroutes interrupts on machines whose APIC ids are not `0..n`.
//! - [`Irq`], an interrupt number as the `irq:` scheme counts them (vector minus 32). Which
//!   CPU's row an interrupt is counted in is the kernel's call: the CPU it arrived on for a
//!   vector reserved there, the BSP for one from the PIC or I/O APIC.
//! - [`Counts`], the lock-free table of interrupt counts with one row per CPU, so a CPU
//!   counting an interrupt touches only its own row.
//! - [`CpuEntry`], one line of the root-only `irq:cpus` file, which maps each online logical
//!   CPU to its APIC id and NUMA node: written by the kernel ([`core::fmt::Display`]), read by
//!   pcid and drivers ([`core::str::FromStr`], [`parse_cpus`]).
//! - [`apic_of`] and [`ApicId::compatibility_destination`], with which pcid builds
//!   compatibility-format MSI messages for a logical CPU, and [`spread`], the order in which a
//!   driver gives its MSI-X vectors to CPUs.
//!
//! The crate is `no_std`, has no unsafe code and no dependencies, and is tested on the host.
//!
//! # Examples
//!
//! ```
//! use ferric_irq::{ApicId, Counts, CpuEntry, CpuId, Irq};
//!
//! let counts: Counts<4> = Counts::new();
//! let irq = Irq::new(18).unwrap();
//! counts.trigger(CpuId::new(1), irq).unwrap();
//! assert_eq!(counts.get(CpuId::new(1), irq), Ok(1));
//! // The same vector number on another CPU is another interrupt.
//! assert_eq!(counts.get(CpuId::new(2), irq), Ok(0));
//!
//! let line = CpuEntry { cpu: CpuId::new(1), apic: ApicId::new(2), node: None }.to_string();
//! assert_eq!(line, "cpu=01 apic=2 node=-");
//! assert_eq!(line.parse::<CpuEntry>().unwrap().apic, ApicId::new(2));
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate std;

use core::{
    fmt,
    num::ParseIntError,
    str::FromStr,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Interrupt numbers the `irq:` scheme counts: vectors 32 to 255 are IRQs 0 to 223.
pub const IRQS: usize = 224;

/// A logical CPU, numbered as the kernel numbers them (the BSP is 0) and as `irq:cpu-XX` names
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CpuId(u32);

impl CpuId {
    /// The bootstrap processor.
    pub const BSP: Self = Self(0);

    /// Logical CPU `id`.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A local APIC id: the destination an MSI message or interrupt remapping entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApicId(u32);

impl ApicId {
    /// APIC id `id`.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The destination ID a compatibility-format MSI address carries for this APIC (bits 19:12,
    /// physical destination mode), or `None` when it cannot name it: 0xFF is the broadcast
    /// destination, and ids above it need x2APIC with interrupt remapping (Intel SDM Vol. 3,
    /// 11.11.1 "Message Address Register Format").
    #[must_use]
    pub const fn compatibility_destination(self) -> Option<u8> {
        if self.0 < 0xff {
            Some(self.0 as u8)
        } else {
            None
        }
    }
}

/// An interrupt number as the `irq:` scheme counts them: the IDT vector minus 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Irq(u8);

impl Irq {
    /// IRQ `irq`, or `None` when it is [`IRQS`] or above.
    #[must_use]
    pub const fn new(irq: u8) -> Option<Self> {
        if (irq as usize) < IRQS {
            Some(Self(irq))
        } else {
            None
        }
    }

    /// The IRQ number vector `vector` is counted as, or `None` for an exception vector (below
    /// 32).
    #[must_use]
    pub const fn from_vector(vector: u8) -> Option<Self> {
        match vector.checked_sub(32) {
            Some(irq) => Some(Self(irq)),
            None => None,
        }
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// [`Counts`] has no row for a CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSuchCpu {
    /// The CPU asked for.
    pub cpu: CpuId,
}

impl fmt::Display for NoSuchCpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no interrupt counts for CPU {}", self.cpu.get())
    }
}

impl core::error::Error for NoSuchCpu {}

/// How many interrupts each IRQ has had, per CPU, for up to `CPUS` CPUs.
///
/// Every counter is an atomic with sequentially consistent accesses, so an interrupt handler
/// can count without a lock while handles read and compare. Each CPU's row is its own array,
/// so counting on one CPU does not write a cache line another CPU's interrupts write.
pub struct Counts<const CPUS: usize> {
    rows: [[AtomicUsize; IRQS]; CPUS],
}

impl<const CPUS: usize> Counts<CPUS> {
    /// All counts zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rows: [const { [const { AtomicUsize::new(0) }; IRQS] }; CPUS],
        }
    }

    /// The counter of `irq` on `cpu`.
    fn counter(&self, cpu: CpuId, irq: Irq) -> Result<&AtomicUsize, NoSuchCpu> {
        let row = usize::try_from(cpu.get())
            .ok()
            .and_then(|cpu| self.rows.get(cpu))
            .ok_or(NoSuchCpu { cpu })?;
        // `Irq` is below `IRQS` by construction.
        row.get(usize::from(irq.get())).ok_or(NoSuchCpu { cpu })
    }

    /// Counts one interrupt of `irq` on `cpu`, and returns the new count (it wraps).
    ///
    /// # Errors
    ///
    /// [`NoSuchCpu`] when `cpu` is `CPUS` or above.
    pub fn trigger(&self, cpu: CpuId, irq: Irq) -> Result<usize, NoSuchCpu> {
        let counter = self.counter(cpu, irq)?;
        Ok(counter.fetch_add(1, Ordering::SeqCst).wrapping_add(1))
    }

    /// The interrupts of `irq` on `cpu` so far.
    ///
    /// # Errors
    ///
    /// [`NoSuchCpu`] when `cpu` is `CPUS` or above.
    pub fn get(&self, cpu: CpuId, irq: Irq) -> Result<usize, NoSuchCpu> {
        Ok(self.counter(cpu, irq)?.load(Ordering::SeqCst))
    }

    /// The interrupts of `irq` on all CPUs together (the sum wraps), as `sys:irq` reports them.
    #[must_use]
    pub fn total(&self, irq: Irq) -> usize {
        self.rows.iter().fold(0, |sum, row| {
            row.get(usize::from(irq.get()))
                .map_or(sum, |count| sum.wrapping_add(count.load(Ordering::SeqCst)))
        })
    }
}

impl<const CPUS: usize> Default for Counts<CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

/// One online CPU, as a line of the `irq:cpus` file: `cpu=<logical, 2+ hex digits> apic=<APIC
/// id, decimal> node=<NUMA node, decimal, or ->`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuEntry {
    /// The logical CPU, as `irq:cpu-XX` names it.
    pub cpu: CpuId,
    /// Its local APIC id, for MSI destinations.
    pub apic: ApicId,
    /// Its NUMA node, when the kernel knows NUMA (`kernel.numa`).
    pub node: Option<u32>,
}

impl fmt::Display for CpuEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cpu={:02x} apic={}", self.cpu.get(), self.apic.get())?;
        match self.node {
            Some(node) => write!(f, " node={node}"),
            None => f.write_str(" node=-"),
        }
    }
}

/// Why an `irq:cpus` line did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// A field is missing or out of order; the field expected.
    Field(&'static str),
    /// A number does not parse; the field it is in.
    Number {
        /// The field.
        field: &'static str,
        /// Why.
        source: ParseIntError,
    },
    /// Something follows the last field.
    Trailing,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(field) => write!(f, "expected the `{field}=` field"),
            Self::Number { field, .. } => write!(f, "the `{field}` field is not a number"),
            Self::Trailing => f.write_str("unexpected text after the last field"),
        }
    }
}

impl core::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Number { source, .. } => Some(source),
            Self::Field(_) | Self::Trailing => None,
        }
    }
}

/// The value of `field` in the next of `words`.
fn field<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<&'a str, ParseError> {
    words
        .next()
        .and_then(|word| word.strip_prefix(field))
        .and_then(|rest| rest.strip_prefix('='))
        .ok_or(ParseError::Field(field))
}

/// `text` as a number in `radix`, for `field`.
fn number(text: &str, radix: u32, field: &'static str) -> Result<u32, ParseError> {
    u32::from_str_radix(text, radix).map_err(|source| ParseError::Number { field, source })
}

impl FromStr for CpuEntry {
    type Err = ParseError;

    fn from_str(line: &str) -> Result<Self, ParseError> {
        let mut words = line.split_ascii_whitespace();
        let cpu = CpuId::new(number(field(&mut words, "cpu")?, 16, "cpu")?);
        let apic = ApicId::new(number(field(&mut words, "apic")?, 10, "apic")?);
        let node = match field(&mut words, "node")? {
            "-" => None,
            node => Some(number(node, 10, "node")?),
        };
        if words.next().is_some() {
            return Err(ParseError::Trailing);
        }
        Ok(Self { cpu, apic, node })
    }
}

/// The entries of an `irq:cpus` file, one per non-blank line.
pub fn parse_cpus(text: &str) -> impl Iterator<Item = Result<CpuEntry, ParseError>> + '_ {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::parse)
}

/// The APIC id of logical CPU `cpu` in the text of an `irq:cpus` file, or `None` when the file
/// does not list it.
///
/// # Errors
///
/// [`ParseError`] for the first malformed line before `cpu`'s.
pub fn apic_of(text: &str, cpu: CpuId) -> Result<Option<ApicId>, ParseError> {
    for entry in parse_cpus(text) {
        let entry = entry?;
        if entry.cpu == cpu {
            return Ok(Some(entry.apic));
        }
    }
    Ok(None)
}

/// The CPUs to give a device's interrupt vectors to, in order: one vector per CPU before any
/// CPU gets a second.
///
/// The CPUs on `node` (the device's NUMA node, when known) come first, in the order `cpus` lists
/// them, then the others. After the last CPU the order starts again, so `spread(..).take(n)`
/// spreads `n` vectors over all CPUs round-robin, the device's node first. The iterator is empty
/// when `cpus` is.
///
/// # Examples
///
/// ```
/// use ferric_irq::{spread, ApicId, CpuEntry, CpuId};
///
/// let cpu = |cpu, node| CpuEntry { cpu: CpuId::new(cpu), apic: ApicId::new(cpu), node };
/// let cpus = [cpu(0, Some(0)), cpu(1, Some(1)), cpu(2, Some(0))];
/// let order: Vec<u32> = spread(&cpus, Some(1)).take(4).map(CpuId::get).collect();
/// assert_eq!(order, [1, 0, 2, 1]);
/// ```
pub fn spread(cpus: &[CpuEntry], node: Option<u32>) -> impl Iterator<Item = CpuId> + Clone + '_ {
    let near = move |entry: &&CpuEntry| node.is_some() && entry.node == node;
    let far = move |entry: &&CpuEntry| !near(entry);
    cpus.iter()
        .filter(near)
        .chain(cpus.iter().filter(far))
        .map(|entry| entry.cpu)
        .cycle()
}

/// The CPUs for the next `count` vectors of a device that has `given` already: [`spread`] over
/// `cpus` from where the first `given` left off, or all on [`CpuId::BSP`] when there is no CPU
/// map (no per-CPU delivery) or it is empty.
///
/// # Examples
///
/// ```
/// use ferric_irq::{spread_from, ApicId, CpuEntry, CpuId};
///
/// let cpu = |cpu| CpuEntry { cpu: CpuId::new(cpu), apic: ApicId::new(cpu), node: None };
/// let cpus = [cpu(0), cpu(1), cpu(2)];
/// let next: Vec<u32> = spread_from(Some(&cpus), None, 2, 2).map(CpuId::get).collect();
/// assert_eq!(next, [2, 0]);
/// let bsp: Vec<CpuId> = spread_from(Some(&[]), None, 0, 2).collect();
/// assert_eq!(bsp, [CpuId::BSP, CpuId::BSP]);
/// ```
pub fn spread_from<'a>(
    cpus: Option<&'a [CpuEntry]>,
    node: Option<u32>,
    given: usize,
    count: usize,
) -> impl Iterator<Item = CpuId> + 'a {
    // `spread` over a non-empty map never ends, so the BSP only comes in when the map is empty.
    spread(cpus.unwrap_or(&[]), node)
        .skip(given)
        .chain(core::iter::repeat(CpuId::BSP))
        .take(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{string::ToString, sync::Arc, thread, vec::Vec};

    fn irq(n: u8) -> Irq {
        Irq::new(n).expect("a valid IRQ")
    }

    fn entry(cpu: u32, node: Option<u32>) -> CpuEntry {
        CpuEntry {
            cpu: CpuId::new(cpu),
            apic: ApicId::new(cpu),
            node,
        }
    }

    fn order(cpus: &[CpuEntry], node: Option<u32>, n: usize) -> Vec<u32> {
        spread(cpus, node).take(n).map(CpuId::get).collect()
    }

    #[test]
    fn spread_gives_each_cpu_one_before_any_gets_two() {
        let cpus = [entry(0, None), entry(1, None), entry(2, None)];
        assert_eq!(order(&cpus, None, 2), [0, 1]);
        assert_eq!(order(&cpus, None, 7), [0, 1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn spread_prefers_the_device_node() {
        let cpus = [
            entry(0, Some(0)),
            entry(1, Some(1)),
            entry(2, Some(0)),
            entry(3, Some(1)),
        ];
        assert_eq!(order(&cpus, Some(1), 5), [1, 3, 0, 2, 1]);
        // An unknown node, or none, keeps the listed order.
        assert_eq!(order(&cpus, Some(7), 4), [0, 1, 2, 3]);
        assert_eq!(order(&cpus, None, 4), [0, 1, 2, 3]);
    }

    #[test]
    fn spread_continues_across_batches() {
        // `Msix::alloc_spread` skips the entries already programmed, so two batches of one
        // land on two CPUs, as one batch of two would.
        let cpus = [entry(0, None), entry(1, None)];
        let first: Vec<u32> = spread(&cpus, None).take(1).map(CpuId::get).collect();
        let second: Vec<u32> = spread(&cpus, None)
            .skip(1)
            .take(1)
            .map(CpuId::get)
            .collect();
        assert_eq!([first, second].concat(), order(&cpus, None, 2));
    }

    #[test]
    fn spread_from_falls_back_to_the_bsp_without_cpus() {
        let cpus = [entry(1, None), entry(2, None)];
        let next = |cpus, given, count| -> Vec<u32> {
            spread_from(cpus, None, given, count)
                .map(CpuId::get)
                .collect()
        };
        assert_eq!(next(Some(&cpus[..]), 1, 3), [2, 1, 2]);
        assert_eq!(next(Some(&cpus[..]), 0, 0), Vec::<u32>::new());
        // No map and an empty map are the same: every vector on the BSP.
        let bsp = CpuId::BSP.get();
        assert_eq!(next(None, 5, 2), [bsp, bsp]);
        assert_eq!(next(Some(&[]), 0, 3), [bsp, bsp, bsp]);
    }

    #[test]
    fn spread_over_no_cpus_is_empty() {
        assert_eq!(spread(&[], Some(0)).next(), None);
    }

    #[test]
    fn compatibility_destinations_stop_below_broadcast() {
        assert_eq!(ApicId::new(0).compatibility_destination(), Some(0));
        assert_eq!(ApicId::new(0xfe).compatibility_destination(), Some(0xfe));
        assert_eq!(ApicId::new(0xff).compatibility_destination(), None);
        assert_eq!(ApicId::new(0x100).compatibility_destination(), None);
    }

    #[test]
    fn apic_of_maps_sparse_ids() {
        let text = "cpu=00 apic=0 node=-\ncpu=01 apic=4 node=-\ncpu=04 apic=1 node=0\n";
        assert_eq!(apic_of(text, CpuId::new(1)), Ok(Some(ApicId::new(4))));
        assert_eq!(apic_of(text, CpuId::new(4)), Ok(Some(ApicId::new(1))));
        assert_eq!(apic_of(text, CpuId::new(2)), Ok(None));
        assert_eq!(
            apic_of("cpu=00 apic=0\n", CpuId::new(1)),
            Err(ParseError::Field("node"))
        );
    }

    #[test]
    fn irq_bounds_and_vectors() {
        assert_eq!(Irq::new(223).map(Irq::get), Some(223));
        assert_eq!(Irq::new(224), None);
        assert_eq!(Irq::from_vector(31), None);
        assert_eq!(Irq::from_vector(32), Some(irq(0)));
        assert_eq!(Irq::from_vector(255), Some(irq(223)));
    }

    #[test]
    fn counts_are_per_cpu() {
        let counts: Counts<4> = Counts::new();
        assert_eq!(counts.trigger(CpuId::new(1), irq(18)), Ok(1));
        assert_eq!(counts.trigger(CpuId::new(1), irq(18)), Ok(2));
        assert_eq!(counts.get(CpuId::new(1), irq(18)), Ok(2));
        assert_eq!(counts.get(CpuId::new(2), irq(18)), Ok(0));
        assert_eq!(counts.get(CpuId::new(1), irq(19)), Ok(0));
        counts.trigger(CpuId::new(3), irq(18)).expect("CPU 3");
        assert_eq!(counts.total(irq(18)), 3);
        assert_eq!(counts.total(irq(19)), 0);
    }

    #[test]
    fn counts_refuse_unknown_cpus() {
        let counts: Counts<2> = Counts::default();
        let none = NoSuchCpu { cpu: CpuId::new(2) };
        assert_eq!(counts.trigger(CpuId::new(2), irq(20)), Err(none));
        assert_eq!(counts.get(CpuId::new(2), irq(20)), Err(none));
        assert_eq!(none.to_string(), "no interrupt counts for CPU 2");
    }

    #[test]
    fn counts_wrap() {
        let counts: Counts<1> = Counts::new();
        let counter = counts.counter(CpuId::BSP, irq(30)).expect("CPU 0");
        counter.store(usize::MAX, Ordering::SeqCst);
        assert_eq!(counts.trigger(CpuId::BSP, irq(30)), Ok(0));
    }

    #[test]
    fn concurrent_cpus_count_their_own() {
        let counts: Arc<Counts<4>> = Arc::new(Counts::new());
        let threads: Vec<_> = (0..4)
            .map(|cpu| {
                let counts = Arc::clone(&counts);
                thread::spawn(move || {
                    for _ in 0..1000 * (cpu + 1) {
                        counts.trigger(CpuId::new(cpu), irq(40)).expect("a CPU");
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no panic");
        }
        for cpu in 0..4 {
            assert_eq!(
                counts.get(CpuId::new(cpu), irq(40)),
                Ok(1000 * (cpu as usize + 1))
            );
        }
    }

    #[test]
    fn cpu_entries_round_trip() {
        let entries = [
            CpuEntry {
                cpu: CpuId::BSP,
                apic: ApicId::new(0),
                node: Some(0),
            },
            CpuEntry {
                cpu: CpuId::new(0x1b),
                apic: ApicId::new(54),
                node: None,
            },
        ];
        let text: std::string::String = entries
            .iter()
            .map(|entry| entry.to_string() + "\n")
            .collect();
        assert_eq!(text, "cpu=00 apic=0 node=0\ncpu=1b apic=54 node=-\n");
        let parsed: Vec<_> = parse_cpus(&text).collect::<Result<_, _>>().expect("valid");
        assert_eq!(parsed, entries);
    }

    #[test]
    fn bad_lines_are_refused() {
        assert_eq!(
            "apic=0 node=-".parse::<CpuEntry>(),
            Err(ParseError::Field("cpu"))
        );
        assert_eq!(
            "cpu=00 node=- apic=0".parse::<CpuEntry>(),
            Err(ParseError::Field("apic"))
        );
        assert_eq!(
            "cpu=00 apic=0".parse::<CpuEntry>(),
            Err(ParseError::Field("node"))
        );
        assert!(matches!(
            "cpu=zz apic=0 node=-".parse::<CpuEntry>(),
            Err(ParseError::Number { field: "cpu", .. })
        ));
        assert!(matches!(
            "cpu=00 apic=0x2 node=-".parse::<CpuEntry>(),
            Err(ParseError::Number { field: "apic", .. })
        ));
        assert_eq!(
            "cpu=00 apic=0 node=- extra".parse::<CpuEntry>(),
            Err(ParseError::Trailing)
        );
        assert!(
            core::error::Error::source(&"cpu=00 apic=x node=-".parse::<CpuEntry>().unwrap_err())
                .is_some()
        );
    }

    #[test]
    fn blank_lines_are_skipped() {
        assert_eq!(parse_cpus("\n  \ncpu=02 apic=4 node=1\n").count(), 1);
    }
}
