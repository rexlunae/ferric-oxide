//! `iommu-trace` and `trace-irq`: the kernel's `iommu.fault` tracepoint records the faults this
//! program causes (`kernel.trace_iommu`), and its `irq.deliver` and `irq.ack` tracepoints the
//! interrupt `iommu-msi` raises and acknowledges (`kernel.trace_irq`; roadmap Phase T).
//!
//! When the kernel has the `kernel.trace` scheme and the `iommu.fault` event, [`start`] turns
//! the `iommu` events on before the first test. Each fault the program announces
//! ([`expect_dma_fault`], [`expect_irq_fault`]) first drains every CPU's ring, keeping the
//! `iommu.fault` records: `sched.switch` is on from boot and fills the rings, and a full ring
//! drops new records. [`finish`] drains until every announced fault has its record, or a
//! timeout, turns the events off again and checks:
//!
//! - VT-d: each announced DMA fault has a fault record, decoded with `ferric-vtd`, for that page
//!   and access, and the forged interrupt a record of an interrupt remapping fault for its
//!   index;
//! - AMD-Vi: an I/O page fault event, decoded with `ferric-amdvi`. QEMU's AMD IOMMU leaves the
//!   address out of its events, so they cannot be matched to the announced addresses (the
//!   boot log check has the same limit).
//!
//! Each announced fault takes a record of its own: one record never stands for two
//! announcements. On AMD-Vi the events of two faults look the same, and the kernel merges
//! identical events it drains in one pass into one record, so before announcing the next
//! fault [`expect`] waits until the faults announced so far have their records, read back from
//! the rings. The kernel drains the event log on every event interrupt (and a call that finds
//! it busy makes the drainer go round again), so a fault's events are drained, and their
//! record written, before the test moves on; the next fault's events then start a new
//! record. A fault's events can still be split over several records, which only adds records.
//!
//! The `irq` events are on only around `iommu-msi`'s interrupt ([`watch_interrupt`],
//! [`interrupt_handled`]): the test reserves a vector, has the device raise its MSI, and
//! acknowledges the count it reads, as a driver does. [`finish`] then needs an `irq.deliver`
//! that woke a handle and the `irq.ack` of the same IRQ, CPU and count. The test does the
//! acknowledging itself because no driver of the smoke image reliably does while a test
//! watches: `nvmed` takes MSI-X interrupts without acknowledging them on the handle, and
//! `e1000d` acknowledges only received packets.
//!
//! The program runs before `pcid-spawner` starts the next driver, long before the smoke script
//! reads the rings, so draining them here takes no records from another test.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ferric_amdvi::event::Event as AmdViEvent;
use ferric_trace::abi::{self, EventLine, StatsLine};
use ferric_trace::kernel::{IOMMU_FAULT, IRQ_ACK, IRQ_DELIVER, IommuFormat, IrqSource};
use ferric_trace::record::Records;
use ferric_vtd::fault::{FaultRecord, RequestKind};

/// How long [`finish`] waits for the records of announced faults.
const WAIT: Duration = Duration::from_secs(3);

/// How long [`expect`] waits for the records of the faults announced before. They normally
/// come within milliseconds; the bound only keeps a missing record from hanging the test,
/// which [`finish`] then reports.
const PREVIOUS_WAIT: Duration = Duration::from_secs(5);

/// How much one read of a CPU's ring asks for (the scheme's maximum).
const READ_LEN: usize = 64 * 1024;

/// A fault this program announced.
#[derive(Clone, Copy, Debug)]
enum Expected {
    /// A DMA write (or read) the IOMMU must block, at this address.
    Dma { write: bool, address: u64 },
    /// An interrupt request naming this remapping entry, which the IOMMU must block.
    Interrupt { index: u16 },
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dma { write, address } => {
                let kind = if *write { "write" } else { "read" };
                write!(f, "DMA {kind} at {address:#x}")
            }
            Self::Interrupt { index } => write!(f, "interrupt index {index}"),
        }
    }
}

/// One `iommu.fault` record: its format and the hardware's two words.
#[derive(Clone, Copy, Debug)]
struct Fault {
    format: u64,
    lo: u64,
    hi: u64,
}

impl Fault {
    /// The VT-d fault record, if it is one.
    fn vtd(&self) -> Option<FaultRecord> {
        (self.format == IommuFormat::VtdFaultRecord.raw())
            .then(|| FaultRecord::decode(self.lo, self.hi))
            .flatten()
    }

    /// Whether it is an AMD-Vi event about a device's DMA.
    fn amdvi_dma(&self) -> bool {
        self.format == IommuFormat::AmdViEvent.raw()
            && AmdViEvent([self.lo, self.hi]).is_dma_fault()
    }

    /// Whether it records `expected`.
    fn records(&self, expected: Expected) -> bool {
        match (expected, self.vtd()) {
            (Expected::Dma { write, address }, Some(record)) => {
                let kind = if write {
                    RequestKind::Write
                } else {
                    RequestKind::Read
                };
                !record.reason.is_interrupt_remapping()
                    && record.kind == kind
                    && record.address == address & !0xfff
            }
            (Expected::Interrupt { index }, Some(record)) => {
                record.interrupt_index() == Some(index)
            }
            (Expected::Dma { .. }, None) => self.amdvi_dma(),
            (Expected::Interrupt { .. }, None) => false,
        }
    }
}

/// One `irq.deliver` record.
#[derive(Clone, Copy, Debug)]
struct Delivery {
    irq: u64,
    cpu: u64,
    source: u64,
    count: u64,
    handles: u64,
    timestamp: u64,
}

/// One `irq.ack` record, with the CPU whose ring had it.
#[derive(Clone, Copy, Debug)]
struct Ack {
    irq: u64,
    cpu: u64,
    count: u64,
    timestamp: u64,
    ring: u32,
}

/// The `irq` events while `iommu-msi` runs.
#[derive(Debug, Default)]
struct Interrupts {
    /// The `irq` events that were off, which [`watch_interrupt`] turned on and
    /// [`interrupt_handled`] turns off again.
    enabled_here: Vec<&'static str>,
    /// Whether an interrupt was raised and acknowledged while the events were on.
    expected: bool,
    /// The CPU and IRQ number of the vector `iommu-msi` reserved: only its delivery and
    /// acknowledgement count, not another driver's.
    target: Option<(u64, u64)>,
    deliveries: Vec<Delivery>,
    acks: Vec<Ack>,
}

impl Interrupts {
    /// The first delivery of the reserved vector that woke a handle and has the
    /// acknowledgement of the same IRQ, CPU and count.
    fn handled(&self) -> Option<(Delivery, Ack)> {
        let (cpu, irq) = self.target?;
        self.deliveries
            .iter()
            .filter(|d| d.cpu == cpu && d.irq == irq && d.handles > 0)
            .find_map(|d| {
                self.acks
                    .iter()
                    .find(|a| a.irq == d.irq && a.cpu == d.cpu && a.count == d.count)
                    .map(|a| (*d, *a))
            })
    }
}

/// Why `iommu-trace` or `trace-irq` failed.
#[derive(Debug)]
pub enum Error {
    /// A file of the trace scheme could not be used.
    Io { path: String, source: io::Error },
    /// A listing line or record did not parse.
    Parse { path: String, what: String },
    /// An announced fault has no record.
    Missing { expected: String, records: usize },
    /// The reserved vector's interrupt has no matching `irq.deliver` and `irq.ack`.
    Unacknowledged { deliveries: usize, acks: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{path}: {source}"),
            Self::Parse { path, what } => write!(f, "{path}: {what}"),
            Self::Missing { expected, records } => write!(
                f,
                "no iommu.fault record for the {expected} ({records} iommu.fault records)"
            ),
            Self::Unacknowledged { deliveries, acks } => write!(
                f,
                "no irq.deliver of the reserved vector that woke a handle has an irq.ack of the same IRQ, CPU and count ({deliveries} irq.deliver, {acks} irq.ack records)"
            ),
        }
    }
}

/// The tracing state while the tests run.
struct Tracer {
    cpus: Vec<u32>,
    /// Whether the kernel has `iommu.fault`.
    iommu: bool,
    /// Whether [`start`] turned `iommu.fault` on, so [`finish`] turns it off again.
    enabled_here: bool,
    expected: Vec<Expected>,
    faults: Vec<Fault>,
    /// The `irq` events, when the kernel has both.
    interrupts: Option<Interrupts>,
}

/// The tracer, once [`start`] found the event.
static TRACER: Mutex<Option<Tracer>> = Mutex::new(None);

/// What [`start`] found.
pub enum Start {
    /// The kernel has `iommu.fault` (now on) or the `irq` events, and their records are
    /// collected.
    Watching,
    /// The kernel has no tracing, or none of these events: why.
    Skip(&'static str),
}

/// The result of one of the two steps.
pub enum Step {
    /// What passed.
    Pass(String),
    /// Why the step did not run.
    Skip(&'static str),
    /// Why it failed.
    Fail(Error),
}

/// The results of `iommu-trace` and `trace-irq`.
pub struct Report {
    /// `iommu-trace`.
    pub iommu: Step,
    /// `trace-irq`.
    pub irq: Step,
}

/// The path of `file` in the trace scheme.
fn path(file: &str) -> String {
    format!("/scheme/{}/{file}", abi::SCHEME_NAME)
}

fn io_error(path: &str) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// Reads a whole text file of the scheme.
fn read_text(file: &str) -> Result<String, Error> {
    let path = path(file);
    let mut text = String::new();
    File::open(&path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(io_error(&path))?;
    Ok(text)
}

/// Writes a command to the event listing.
fn command(text: &str) -> Result<(), Error> {
    let path = path(abi::EVENTS_PATH);
    OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(io_error(&path))
}

/// Turns the `iommu` events on, if the kernel has them.
///
/// # Errors
///
/// When the scheme is listed but its files cannot be used or do not parse.
pub fn start() -> Result<Start, Error> {
    let listed = std::fs::read_dir("/scheme").map(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == abi::SCHEME_NAME)
    });
    if !listed.unwrap_or(false) {
        return Ok(Start::Skip("the kernel has no kernel.trace scheme"));
    }
    let events_path = path(abi::EVENTS_PATH);
    let listing = read_text(abi::EVENTS_PATH)?;
    let mut on = None;
    let mut irq_events = 0;
    for line in listing.lines() {
        let event = EventLine::parse(line).map_err(|err| Error::Parse {
            path: events_path.clone(),
            what: format!("{line:?}: {err}"),
        })?;
        if event.name == IOMMU_FAULT.name() {
            on = Some(event.enabled);
        } else if event.name == IRQ_DELIVER.name() || event.name == IRQ_ACK.name() {
            irq_events += 1;
        }
    }
    let interrupts = (irq_events == 2).then(Interrupts::default);
    if on.is_none() && interrupts.is_none() {
        return Ok(Start::Skip(
            "iommu.fault and the irq events are not compiled in",
        ));
    }
    let stats_path = path(abi::STATS_PATH);
    let cpus = read_text(abi::STATS_PATH)?
        .lines()
        .map(|line| {
            StatsLine::parse(line)
                .map(|s| s.cpu)
                .map_err(|err| Error::Parse {
                    path: stats_path.clone(),
                    what: format!("{line:?}: {err}"),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if on == Some(false) {
        command(&format!("enable {}", IOMMU_FAULT.name()))?;
    }
    let mut tracer = Tracer {
        cpus,
        iommu: on.is_some(),
        enabled_here: on == Some(false),
        expected: Vec::new(),
        faults: Vec::new(),
        interrupts,
    };
    tracer.drain()?;
    *lock() = Some(tracer);
    Ok(Start::Watching)
}

/// The tracer's lock; a panic while holding it leaves the state usable.
fn lock() -> std::sync::MutexGuard<'static, Option<Tracer>> {
    TRACER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Tracer {
    /// Drains every CPU's ring, keeping the `iommu.fault` and `irq` records.
    fn drain(&mut self) -> Result<(), Error> {
        let mut buf = vec![0u8; READ_LEN];
        for cpu_ring in &self.cpus {
            let path = path(&format!("{}/{cpu_ring}", abi::CPU_DIR));
            let mut file = File::open(&path).map_err(io_error(&path))?;
            loop {
                let len = file.read(&mut buf).map_err(io_error(&path))?;
                if len == 0 {
                    break;
                }
                for record in Records::new(buf.get(..len).unwrap_or_default()) {
                    let record = record.map_err(|err| Error::Parse {
                        path: path.clone(),
                        what: err.to_string(),
                    })?;
                    let args: Vec<u64> = record.args().collect();
                    let timestamp = record.timestamp();
                    let event = record.event();
                    match (&args[..], self.interrupts.as_mut()) {
                        (&[_, format, lo, hi, _], _) if event == IOMMU_FAULT.id() => {
                            self.faults.push(Fault { format, lo, hi });
                        }
                        (&[irq, cpu, source, count, handles], Some(interrupts))
                            if event == IRQ_DELIVER.id() =>
                        {
                            interrupts.deliveries.push(Delivery {
                                irq,
                                cpu,
                                source,
                                count,
                                handles,
                                timestamp,
                            });
                        }
                        (&[irq, cpu, count], Some(interrupts)) if event == IRQ_ACK.id() => {
                            interrupts.acks.push(Ack {
                                irq,
                                cpu,
                                count,
                                timestamp,
                                ring: *cpu_ring,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether [`finish`] still waits for records: of an announced fault, or of the
    /// acknowledged interrupt.
    fn waiting(&self) -> bool {
        self.missing().is_some()
            || self
                .interrupts
                .as_ref()
                .is_some_and(|i| i.expected && i.handled().is_none())
    }

    /// The first announced fault without a record of its own: each announcement, in order,
    /// takes the first record that matches it and no earlier announcement took.
    fn missing(&self) -> Option<Expected> {
        let mut taken = vec![false; self.faults.len()];
        self.expected.iter().copied().find(|&expected| {
            let free = self
                .faults
                .iter()
                .zip(taken.iter_mut())
                .find(|(fault, taken)| !**taken && fault.records(expected));
            match free {
                Some((_, taken)) => {
                    *taken = true;
                    false
                }
                None => true,
            }
        })
    }
}

/// Notes a fault about to happen and makes room for its record. It first waits (up to
/// [`PREVIOUS_WAIT`]) until the faults announced before have their records, so that this one
/// does not share a record with them. Draining errors are left for [`finish`], which drains
/// again, as is a fault whose record does not come.
fn expect(expected: Expected) {
    if let Some(tracer) = lock().as_mut() {
        let deadline = Instant::now() + PREVIOUS_WAIT;
        while tracer.drain().is_ok() && tracer.missing().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        tracer.expected.push(expected);
    }
}

/// Announces a DMA fault the device is about to cause: prints the line the boot check reads
/// and, while tracing, expects its record.
pub fn expect_dma_fault(write: bool, address: u64) {
    let kind = if write { "write" } else { "read" };
    println!("FERRIC_IOMMU_EXPECT_FAULT {kind} {address:#x}");
    expect(Expected::Dma { write, address });
}

/// Announces an interrupt remapping fault for entry `index`, like [`expect_dma_fault`].
pub fn expect_irq_fault(index: u16) {
    println!("FERRIC_IOMMU_EXPECT_IRQ_FAULT {index}");
    expect(Expected::Interrupt { index });
}

/// Turns the `irq` events on for the interrupt `iommu-msi` is about to raise on IRQ `irq` of
/// logical CPU `cpu` (the `irq:cpu-XX/N` numbering), after making room in the rings. Errors are
/// left for [`finish`], which drains again.
pub fn watch_interrupt(cpu: u32, irq: u8) {
    let mut guard = lock();
    let Some(tracer) = guard.as_mut() else {
        return;
    };
    let _ = tracer.drain();
    let Some(interrupts) = tracer.interrupts.as_mut() else {
        return;
    };
    interrupts.target = Some((u64::from(cpu), u64::from(irq)));
    let on = read_text(abi::EVENTS_PATH).unwrap_or_default();
    for name in [IRQ_DELIVER.name(), IRQ_ACK.name()] {
        let enabled = on
            .lines()
            .filter_map(|line| EventLine::parse(line).ok())
            .any(|event| event.name == name && event.enabled);
        if !enabled && command(&format!("enable {name}")).is_ok() {
            interrupts.enabled_here.push(name);
        }
    }
}

/// Called once `iommu-msi`'s interrupt was handled: `acknowledged` says whether the test read
/// and acknowledged a count. Collects the records and turns off the `irq` events that
/// [`watch_interrupt`] turned on. Errors are left for [`finish`].
pub fn interrupt_handled(acknowledged: bool) {
    let mut guard = lock();
    let Some(tracer) = guard.as_mut() else {
        return;
    };
    let _ = tracer.drain();
    if let Some(interrupts) = tracer.interrupts.as_mut() {
        for name in core::mem::take(&mut interrupts.enabled_here) {
            let _ = command(&format!("disable {name}"));
        }
        interrupts.expected |= acknowledged;
    }
}

/// Waits for the records of the announced faults and of the acknowledged interrupt, turns the
/// events off again if this program turned them on, and returns both steps' results.
///
/// # Errors
///
/// When the rings cannot be read; then neither step can say anything.
pub fn finish() -> Result<Report, Error> {
    let Some(mut tracer) = lock().take() else {
        return Ok(Report {
            iommu: Step::Skip("tracing did not start"),
            irq: Step::Skip("tracing did not start"),
        });
    };
    let deadline = Instant::now() + WAIT;
    let mut result = tracer.drain();
    while result.is_ok() && tracer.waiting() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        result = tracer.drain();
    }
    if tracer.enabled_here {
        command(&format!("disable {}", IOMMU_FAULT.name()))?;
    }
    if let Some(interrupts) = tracer.interrupts.as_mut() {
        for name in core::mem::take(&mut interrupts.enabled_here) {
            command(&format!("disable {name}"))?;
        }
    }
    result?;
    Ok(Report {
        iommu: iommu_step(&tracer),
        irq: irq_step(tracer.interrupts.as_ref()),
    })
}

/// `iommu-trace`'s result from the collected records.
fn iommu_step(tracer: &Tracer) -> Step {
    if !tracer.iommu {
        return Step::Skip("iommu.fault is not compiled in");
    }
    if tracer.expected.is_empty() {
        return Step::Skip("no IOMMU fault was announced (no domain, no interrupt remapping)");
    }
    if let Some(expected) = tracer.missing() {
        return Step::Fail(Error::Missing {
            expected: expected.to_string(),
            records: tracer.faults.len(),
        });
    }
    let amdvi = tracer.faults.iter().any(Fault::amdvi_dma);
    Step::Pass(format!(
        "{} announced fault(s) have iommu.fault records ({} records{})",
        tracer.expected.len(),
        tracer.faults.len(),
        if amdvi {
            "; AMD-Vi events matched by kind only"
        } else {
            ""
        }
    ))
}

/// `trace-irq`'s result from the collected records.
fn irq_step(interrupts: Option<&Interrupts>) -> Step {
    let Some(interrupts) = interrupts else {
        return Step::Skip("irq.deliver and irq.ack are not compiled in");
    };
    if !interrupts.expected {
        return Step::Skip("iommu-msi acknowledged no interrupt");
    }
    let Some((deliver, ack)) = interrupts.handled() else {
        return Step::Fail(Error::Unacknowledged {
            deliveries: interrupts.deliveries.len(),
            acks: interrupts.acks.len(),
        });
    };
    let source = IrqSource::from_raw(deliver.source)
        .map_or_else(|| deliver.source.to_string(), |s| s.to_string());
    Step::Pass(format!(
        "irq {} on cpu {} count {} ({source}), {} deliveries; acknowledged on cpu {}, {} TSC cycles later",
        deliver.irq,
        deliver.cpu,
        deliver.count,
        interrupts.deliveries.len(),
        ack.ring,
        ack.timestamp.saturating_sub(deliver.timestamp),
    ))
}
