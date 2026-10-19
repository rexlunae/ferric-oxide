//! `iommu-trace`: the kernel's `iommu.fault` tracepoint records the faults this program
//! causes (`kernel.trace_iommu`, roadmap Phase T).
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
//! - AMD-Vi: there is an I/O page fault event, decoded with `ferric-amdvi`. QEMU's AMD IOMMU
//!   leaves the address out of its events, so they cannot be matched to the announced
//!   addresses (the boot log check has the same limit).
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
use ferric_trace::kernel::{IOMMU_FAULT, IommuFormat};
use ferric_trace::record::Records;
use ferric_vtd::fault::{FaultRecord, RequestKind};

/// How long [`finish`] waits for the records of announced faults.
const WAIT: Duration = Duration::from_secs(3);

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

/// Why `iommu-trace` failed.
#[derive(Debug)]
pub enum Error {
    /// A file of the trace scheme could not be used.
    Io { path: String, source: io::Error },
    /// A listing line or record did not parse.
    Parse { path: String, what: String },
    /// An announced fault has no record.
    Missing { expected: String, records: usize },
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
        }
    }
}

/// The tracing state while the tests run.
struct Tracer {
    cpus: Vec<u32>,
    /// Whether [`start`] turned `iommu.fault` on, so [`finish`] turns it off again.
    enabled_here: bool,
    expected: Vec<Expected>,
    faults: Vec<Fault>,
}

/// The tracer, once [`start`] found the event.
static TRACER: Mutex<Option<Tracer>> = Mutex::new(None);

/// What [`start`] found.
pub enum Start {
    /// The `iommu` events are on and faults are collected.
    Watching,
    /// The kernel has no tracing, or no `iommu.fault`: why.
    Skip(&'static str),
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
    for line in listing.lines() {
        let event = EventLine::parse(line).map_err(|err| Error::Parse {
            path: events_path.clone(),
            what: format!("{line:?}: {err}"),
        })?;
        if event.name == IOMMU_FAULT.name() {
            on = Some(event.enabled);
        }
    }
    let Some(on) = on else {
        return Ok(Start::Skip("iommu.fault is not compiled in"));
    };
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
    if !on {
        command(&format!("enable {}", IOMMU_FAULT.name()))?;
    }
    let mut tracer = Tracer {
        cpus,
        enabled_here: !on,
        expected: Vec::new(),
        faults: Vec::new(),
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
    /// Drains every CPU's ring, keeping the `iommu.fault` records.
    fn drain(&mut self) -> Result<(), Error> {
        let mut buf = vec![0u8; READ_LEN];
        for cpu in &self.cpus {
            let path = path(&format!("{}/{cpu}", abi::CPU_DIR));
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
                    if record.event() != IOMMU_FAULT.id() {
                        continue;
                    }
                    let args: Vec<u64> = record.args().collect();
                    if let [_, format, lo, hi, _] = args[..] {
                        self.faults.push(Fault { format, lo, hi });
                    }
                }
            }
        }
        Ok(())
    }

    /// The first announced fault without a record.
    fn missing(&self) -> Option<Expected> {
        self.expected
            .iter()
            .copied()
            .find(|&expected| !self.faults.iter().any(|fault| fault.records(expected)))
    }
}

/// Notes a fault about to happen and makes room for its record. Draining errors are left
/// for [`finish`], which drains again.
fn expect(expected: Expected) {
    if let Some(tracer) = lock().as_mut() {
        let _ = tracer.drain();
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

/// Waits for the records of the announced faults, turns the events off again if [`start`]
/// turned them on, and returns what passed, or `None` when nothing was traced or expected.
///
/// # Errors
///
/// When the rings cannot be read or an announced fault has no record.
pub fn finish() -> Result<Option<String>, Error> {
    let Some(mut tracer) = lock().take() else {
        return Ok(None);
    };
    let deadline = Instant::now() + WAIT;
    let mut result = tracer.drain();
    while result.is_ok() && tracer.missing().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        result = tracer.drain();
    }
    if tracer.enabled_here {
        command(&format!("disable {}", IOMMU_FAULT.name()))?;
    }
    result?;
    if tracer.expected.is_empty() {
        return Ok(None);
    }
    if let Some(expected) = tracer.missing() {
        return Err(Error::Missing {
            expected: expected.to_string(),
            records: tracer.faults.len(),
        });
    }
    let amdvi = tracer.faults.iter().any(Fault::amdvi_dma);
    Ok(Some(format!(
        "{} announced fault(s) have iommu.fault records ({} records{})",
        tracer.expected.len(),
        tracer.faults.len(),
        if amdvi {
            "; AMD-Vi events matched by kind only"
        } else {
            ""
        }
    )))
}
