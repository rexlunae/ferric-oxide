//! The parts of tracing that exist with the `trace` feature: what is compiled in, turning events
//! on and off, the per-CPU rings and writing records.
//!
//! Each CPU's ring is in the shared layout of `ferric_trace::ring` (a header page, then the
//! data pages), in frames the kernel allocates once and keeps, so `kernel.trace` can map them
//! read-only into a reader. The kernel is the only writer of those words: the ring's size
//! lives in the `Ring` itself, and the counters it reads back from the header are ones it
//! stored, masked to the data before use. A reader that hands back a position goes through
//! `Ring::commit_to`, which takes only a record boundary between `tail` and `head`.

use alloc::{boxed::Box, vec::Vec};
use core::{fmt, sync::atomic::AtomicU64};

use ferric_trace::{
    abi::CPU_RING_WORDS,
    filter::{Filter, FilterError, Loaded, Slot},
    kernel::CATALOG,
    ring::{Ring, RingError, Shared, Words, SHARED_HEADER_WORDS},
    select, Event, EventSet, Level, SelectError, Subsystem,
};
use spin::{Mutex, Once};

pub use crate::arch::static_key::{interrupts_enabled, is_on, key as enabled, Mode};
use crate::{
    arch::static_key,
    cpu_set::{LogicalCpuId, MAX_CPU_COUNT},
    memory::{RaiiFrame, PAGE_SIZE},
};

#[cfg(not(target_arch = "x86_64"))]
compile_error!("kernel tracing (the `trace` feature) is only implemented on x86_64");

// The profiler takes one CPU out of the system (profiling.rs) and points all of its interrupt
// vectors at the wakeup handler, so it would never acknowledge the IPI live patching waits for.
#[cfg(feature = "profiling")]
compile_error!("the `trace` and `profiling` features cannot be combined yet");

/// The most verbose level compiled in, from the `trace_level_*` features.
pub const MAX_LEVEL: Level = if cfg!(feature = "trace_level_trace") {
    Level::Trace
} else if cfg!(feature = "trace_level_debug") {
    Level::Debug
} else if cfg!(feature = "trace_level_info") {
    Level::Info
} else if cfg!(feature = "trace_level_warn") {
    Level::Warn
} else {
    Level::Error
};

/// Whether `subsystem`'s tracepoints are compiled in (its `Subsystem::kernel_feature`).
const fn subsystem_compiled(subsystem: Subsystem) -> bool {
    (matches!(subsystem, Subsystem::Syscall) && cfg!(feature = "trace_syscall"))
        || (matches!(subsystem, Subsystem::Sched) && cfg!(feature = "trace_sched"))
        || (matches!(subsystem, Subsystem::Scheme) && cfg!(feature = "trace_scheme"))
}

/// Whether `event`'s tracepoints are compiled in: its subsystem is, and its level is within
/// [`MAX_LEVEL`] (the rule host tools apply with `Catalog::compiled_for`).
pub const fn compiled_in(event: &Event) -> bool {
    event.compiled_with(subsystem_compiled(event.subsystem()), MAX_LEVEL)
}

/// The events compiled into this kernel.
pub fn compiled() -> EventSet {
    let mut set = EventSet::new();
    for event in CATALOG.events().iter().filter(|event| compiled_in(event)) {
        set.insert(event.id());
    }
    set
}

/// Why a selection could not be turned on or off.
#[derive(Clone, Copy, Debug)]
pub enum ControlError<'a> {
    /// The selection names something that is not an event or subsystem.
    Select(SelectError<'a>),
    /// The selection names events, but none of them is compiled into this kernel.
    NotCompiled,
}

impl fmt::Display for ControlError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::Select(err) => fmt::Display::fmt(err, f),
            ControlError::NotCompiled => f.write_str("no selected event is compiled in"),
        }
    }
}

impl<'a> From<SelectError<'a>> for ControlError<'a> {
    fn from(err: SelectError<'a>) -> Self {
        ControlError::Select(err)
    }
}

/// Turns the compiled-in events that `spec` selects on or off, and returns how many changed.
///
/// [`Mode::Live`] waits for every other CPU to take an interrupt: call it from a system call,
/// not from an interrupt handler.
///
/// # Errors
///
/// [`ControlError::Select`] when `spec` does not parse, and [`ControlError::NotCompiled`] when it
/// selects events but none that this kernel has.
pub fn control(spec: &str, on: bool, mode: Mode) -> Result<usize, ControlError<'_>> {
    let selected = select(&CATALOG, spec)?;
    let events = selected.intersection(&compiled());
    if events.is_empty() && !selected.is_empty() {
        return Err(ControlError::NotCompiled);
    }
    if on && mode == Mode::Live {
        init_rings();
    }
    Ok(static_key::set(&events, on, mode))
}

/// The value of `key` in the boot environment (`KEY=VALUE` lines).
fn env_value<'a>(env: &'a [u8], key: &str) -> Option<&'a str> {
    env.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(key.as_bytes())?.strip_prefix(b"="))
        .and_then(|value| core::str::from_utf8(value).ok())
        .map(str::trim)
}

/// Turns on the events the build-time default (`trace.boot` in the kernel's `config.toml`) and
/// the boot environment's `FERRIC_TRACE` select. Runs on the BSP before the other CPUs start.
pub fn init_boot(env: &[u8]) {
    let sources = [
        ("trace.boot", option_env!("FERRIC_TRACE_BOOT").unwrap_or("")),
        ("FERRIC_TRACE", env_value(env, "FERRIC_TRACE").unwrap_or("")),
    ];
    for (source, spec) in sources {
        if spec.is_empty() {
            continue;
        }
        match control(spec, true, Mode::Boot) {
            Ok(_) => {
                for event in CATALOG.events().iter().filter(|event| is_on(event.id())) {
                    info!("trace: {} on from boot ({source})", event.name());
                }
            }
            Err(err) => {
                warn!("trace: {source}={spec:?} ignored: {err}");
            }
        }
    }
}

/// The words in a page.
const WORDS_PER_PAGE: usize = PAGE_SIZE / 8;

/// The pages of a CPU's ring: the header page, then the data.
const RING_PAGES: usize = (SHARED_HEADER_WORDS + CPU_RING_WORDS) / WORDS_PER_PAGE;

const _: () = assert!(SHARED_HEADER_WORDS.is_multiple_of(WORDS_PER_PAGE));
const _: () = assert!(CPU_RING_WORDS.is_multiple_of(WORDS_PER_PAGE));

/// A CPU ring's memory: zeroed frames allocated once and kept for the kernel's life, reached
/// through the linear map one page at a time.
pub struct FramePages {
    /// The frames, for mapping into a reader.
    frames: &'static [RaiiFrame],
    /// Each frame's words, looked up once so a push does not redo it.
    pages: Box<[&'static [AtomicU64; WORDS_PER_PAGE]]>,
}

/// Why a CPU's ring could not be made.
#[derive(Clone, Copy, Debug)]
pub enum RingInitError {
    /// No free frames.
    NoMemory,
    /// The layout constants do not make a ring.
    Layout(RingError),
}

impl fmt::Display for RingInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingInitError::NoMemory => f.write_str("no memory for the ring"),
            RingInitError::Layout(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl FramePages {
    /// [`RING_PAGES`] zeroed frames, kept for the kernel's life.
    fn allocate() -> Result<Self, RingInitError> {
        let frames = (0..RING_PAGES)
            .map(|_| RaiiFrame::allocate().map_err(|_| RingInitError::NoMemory))
            .collect::<Result<Vec<_>, _>>()?;
        // A ring lives as long as the kernel, so its frames are never freed.
        let frames: &'static [RaiiFrame] = Box::leak(frames.into_boxed_slice());
        let pages = frames.iter().map(RaiiFrame::words).collect();
        Ok(Self { frames, pages })
    }

    /// The frames, header first.
    pub fn frames(&self) -> &'static [RaiiFrame] {
        self.frames
    }
}

impl Words for FramePages {
    fn len(&self) -> usize {
        self.pages.len().saturating_mul(WORDS_PER_PAGE)
    }

    // `Words::word` may panic past the end, and a ring never asks for such a word: it masks
    // every position to the data first. The word within the page is always in bounds.
    #[expect(clippy::indexing_slicing)]
    fn word(&self, index: usize) -> &AtomicU64 {
        &self.pages[index / WORDS_PER_PAGE][index % WORDS_PER_PAGE]
    }
}

/// A CPU's ring.
pub type CpuRing = Ring<Shared<FramePages>>;

/// Each CPU's ring, made by [`init_rings`].
static RINGS: [Once<CpuRing>; MAX_CPU_COUNT as usize] =
    [const { Once::new() }; MAX_CPU_COUNT as usize];

/// Makes a ring for every CPU that is up and has none. Called from `kmain`, once the CPUs are
/// counted, and before an event is turned on at run time. A CPU whose ring cannot be made
/// stays without one, counting nothing, until the next call.
pub fn init_rings() {
    // The clock is picked before the first ring exists, so every record has its timestamps.
    super::clock::choice();
    for (cpu, ring) in RINGS.iter().enumerate().take(crate::cpu_count() as usize) {
        let made = ring
            .try_call_once(|| Ring::shared(FramePages::allocate()?).map_err(RingInitError::Layout));
        if let Err(err) = made {
            warn!("trace: no ring for CPU {cpu}: {err}");
        }
    }
}

/// CPU `cpu`'s ring, if it has one.
pub fn ring(cpu: LogicalCpuId) -> Option<&'static CpuRing> {
    RINGS.get(cpu.get() as usize)?.get()
}

/// Each event's argument filter (`ferric_trace::filter`), by event index.
static FILTERS: [Slot; CATALOG.len()] = [const { Slot::new() }; CATALOG.len()];

/// Serializes [`set_filter`]: a `Slot` takes one writer at a time.
static FILTER_WRITER: Mutex<()> = Mutex::new(());

/// Why a filter could not be set.
#[derive(Clone, Copy, Debug)]
pub enum FilterControlError<'a> {
    /// No event has this name.
    UnknownEvent(&'a str),
    /// The event is not compiled into this kernel.
    NotCompiled(&'static Event),
    /// The filter does not parse.
    Filter(FilterError),
}

impl fmt::Display for FilterControlError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterControlError::UnknownEvent(name) => write!(f, "no event named {name:?}"),
            FilterControlError::NotCompiled(event) => {
                write!(f, "{} is not compiled in", event.name())
            }
            FilterControlError::Filter(err) => fmt::Display::fmt(err, f),
        }
    }
}

/// Sets the argument filter of event `name` from `text` (`ferric_trace::filter::Filter::parse`,
/// or `none` to remove it), and returns the event and the filter now set.
///
/// # Errors
///
/// [`FilterControlError::UnknownEvent`], [`FilterControlError::NotCompiled`], or
/// [`FilterControlError::Filter`] when `text` does not parse.
pub fn set_filter<'a>(
    name: &'a str,
    text: &str,
) -> Result<(&'static Event, Option<Filter>), FilterControlError<'a>> {
    let event = CATALOG
        .find(name)
        .ok_or(FilterControlError::UnknownEvent(name))?;
    if !compiled_in(event) {
        return Err(FilterControlError::NotCompiled(event));
    }
    let filter = Filter::parse(event, text).map_err(FilterControlError::Filter)?;
    if let Some(slot) = FILTERS.get(event.id().index()) {
        let _writer = FILTER_WRITER.lock();
        slot.store(filter.as_ref());
    }
    Ok((event, filter))
}

/// The filters that are set, with their events: [`Loaded::Set`], or [`Loaded::Busy`] for one
/// that could not be read. The events without a filter are left out.
///
/// It holds [`FILTER_WRITER`] while reading, so no store overlaps a read and every set filter
/// is listed as it is.
pub fn filters() -> Vec<(&'static Event, Loaded)> {
    let _writer = FILTER_WRITER.lock();
    CATALOG
        .events()
        .iter()
        .zip(FILTERS.iter())
        .map(|(event, slot)| (event, slot.load()))
        .filter(|(_, loaded)| *loaded != Loaded::Empty)
        .collect()
}

/// Writes a record of `event` with `args` to this CPU's ring. The slow path of `trace_event!`.
#[cold]
#[inline(never)]
pub fn emit(event: &Event, args: &[u64]) {
    // Before `kmain` makes the rings there is nowhere to put the record.
    let Some(ring) = ring(crate::cpu_id()) else {
        return;
    };
    if interrupts_enabled() {
        // An interrupt could switch contexts mid-record and leave the ring with two producers.
        ring.count_lost();
        return;
    }
    // A filtered-out record is neither written nor lost: the reader asked not to see it.
    if !FILTERS
        .get(event.id().index())
        .is_none_or(|slot| slot.admits(args))
    {
        return;
    }
    // A full ring counts the record as lost itself.
    let _ = ring.push(event.id(), super::clock::now(), args);
}
