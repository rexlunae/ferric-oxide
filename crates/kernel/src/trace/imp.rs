//! The parts of tracing that exist with the `trace` feature: what is compiled in, turning events
//! on and off, the per-CPU rings and writing records.
//!
//! Each CPU's ring is in the shared layout of `ferric_trace::ring` (a header page, then the
//! data pages), in frames the kernel allocates once and keeps, so `kernel.trace` can map them
//! read-only into a reader, or, with host rings (`kernel.trace_host`, the `host` module), in
//! the region a host reads. The kernel is the only writer of those words: the ring's size
//! lives in the `Ring` itself, and the counters it reads back from the header are ones it
//! stored, masked to the data before use. A reader that hands back a position goes through
//! `Ring::commit_to`, which takes only a record boundary between `tail` and `head`.
//!
//! A full ring drops new records, unless the kernel keeps crash dumps (`kernel.trace_crash`)
//! or the ring is a host ring: then it overwrites its oldest ones
//! (`WhenFull::OverwriteOldest`), so a dump holds the last records before a panic even when
//! nobody reads the rings, and a host reader never holds the kernel up.
//!
//! The rings are made when the first event is turned on at run time, or by `kmain`. When
//! events are on from boot (or host rings are asked for), [`init_boot`] makes the boot
//! CPU's ring before it turns them on, and [`init_cpus`] the other CPUs' before they start,
//! so no record from boot is lost, provided the trace clock can be chosen that early (see
//! `clock::choice_early`).

use alloc::{boxed::Box, vec::Vec};
use core::{fmt, sync::atomic::AtomicU64};

use ferric_abi::boot_env::FERRIC_TRACE;
use ferric_trace::{
    abi::{UserRecord, CPU_RING_WORDS},
    filter::{Filter, FilterError, Loaded, Slot},
    kernel::CATALOG,
    ring::{Ring, RingError, Shared, WhenFull, Words, SHARED_HEADER_WORDS},
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
        || (matches!(subsystem, Subsystem::Mm) && cfg!(feature = "trace_mm"))
        || (matches!(subsystem, Subsystem::Irq) && cfg!(feature = "trace_irq"))
        || (matches!(subsystem, Subsystem::Iommu) && cfg!(feature = "trace_iommu"))
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
pub(super) fn env_value<'a>(env: &'a [u8], key: &str) -> Option<&'a str> {
    env.split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(key.as_bytes())?.strip_prefix(b"="))
        .and_then(|value| core::str::from_utf8(value).ok())
        .map(str::trim)
}

/// The selections of events to turn on at boot: the build-time default (`trace.boot` in the
/// kernel's `config.toml`) and the boot environment's `FERRIC_TRACE`, with their names.
fn boot_sources(env: &[u8]) -> [(&'static str, &str); 2] {
    [
        ("trace.boot", option_env!("FERRIC_TRACE_BOOT").unwrap_or("")),
        (FERRIC_TRACE, env_value(env, FERRIC_TRACE).unwrap_or("")),
    ]
}

/// Whether rings are wanted from boot: events are selected to turn on at boot, or host rings
/// were reserved.
pub fn rings_wanted_at_boot(env: &[u8]) -> bool {
    host_reserved() || boot_sources(env).iter().any(|(_, spec)| !spec.is_empty())
}

/// Whether host rings were reserved.
fn host_reserved() -> bool {
    #[cfg(feature = "trace_host")]
    return super::host::is_reserved();
    #[cfg(not(feature = "trace_host"))]
    false
}

/// Turns on the events the build-time default (`trace.boot` in the kernel's `config.toml`) and
/// the boot environment's `FERRIC_TRACE` select. Runs on the BSP before the other CPUs start.
///
/// When rings are wanted from boot ([`rings_wanted_at_boot`]), it first makes the boot CPU's
/// ring (and every host ring), if the clock can be chosen this early; otherwise records
/// before `kmain` makes the rings are dropped, as it logs. The trace-mode kernel
/// (`trace_mode`) first logs the line `trace: trace-mode kernel`.
pub fn init_boot(env: &[u8]) {
    // The harness's proof that the bootloader loaded the trace-mode kernel. Written without
    // `info!`'s module prefix, so the log line is exactly this text.
    #[cfg(feature = "trace_mode")]
    crate::println!("trace: trace-mode kernel");
    if rings_wanted_at_boot(env) {
        if super::clock::choice_early().is_some() {
            make_rings(1);
        } else {
            crate::info!(
                "trace: records before kmain are not kept: the trace clock waits for the HPET"
            );
        }
    }
    for (source, spec) in boot_sources(env) {
        if spec.is_empty() {
            continue;
        }
        match control(spec, true, Mode::Boot) {
            Ok(_) => {
                for event in CATALOG.events().iter().filter(|event| is_on(event.id())) {
                    crate::info!("trace: {} on from boot ({source})", event.name());
                }
            }
            Err(err) => {
                crate::warn!("trace: {source}={spec:?} ignored: {err}");
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

    /// A ring's pages in the host ring region: `words`, a whole number of pages. They are
    /// not frames a grant can hold, so it has no frames to map.
    #[cfg(feature = "trace_host")]
    fn region(words: &'static [AtomicU64]) -> Self {
        let (pages, _) = words.as_chunks::<WORDS_PER_PAGE>();
        Self {
            frames: &[],
            pages: pages.iter().collect(),
        }
    }

    /// The frames, header first; none for a host ring, which cannot be mapped into a reader.
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

/// What a full ring does: a crash dump wants the newest records, a reader that sees `lost`
/// counted wants the oldest kept until it reads them.
const WHEN_FULL: WhenFull = if cfg!(feature = "trace_crash") {
    WhenFull::OverwriteOldest
} else {
    WhenFull::DropNewest
};

/// Each CPU's ring, made by [`init_rings`].
static RINGS: [Once<CpuRing>; MAX_CPU_COUNT as usize] =
    [const { Once::new() }; MAX_CPU_COUNT as usize];

/// Makes a ring for every CPU that is up and has none. Called from `kmain`, once the CPUs are
/// counted, and before an event is turned on at run time. A CPU whose ring cannot be made
/// stays without one, counting nothing, until the next call.
pub fn init_rings() {
    // The clock is picked before the first ring exists, so every record has its timestamps.
    super::clock::choice();
    make_rings(crate::cpu_count() as usize);
}

/// Makes the rings of the first `count` CPUs, which the boot CPU is about to start, if
/// [`init_boot`] made the boot CPU's ring: then records from boot are kept, the other CPUs'
/// included. Called on the boot CPU before it starts the others.
pub fn init_cpus(count: usize) {
    if ring(LogicalCpuId::BSP).is_some() {
        make_rings(count);
    }
}

/// Makes a ring for each of the first `count` CPUs that has none, and for every CPU the host
/// ring region holds, then publishes the region's directory. The clock must be chosen.
fn make_rings(count: usize) {
    #[cfg(feature = "trace_host")]
    let count = count.max(super::host::cpus());
    for (cpu, ring) in RINGS.iter().enumerate().take(count) {
        let made = ring.try_call_once(|| {
            #[cfg(feature = "trace_host")]
            if let Some(words) = super::host::ring_words(cpu) {
                return host_ring(words);
            }
            frame_ring()
        });
        if let Err(err) = made {
            crate::warn!("trace: no ring for CPU {cpu}: {err}");
        }
    }
    #[cfg(feature = "trace_host")]
    super::host::publish();
}

/// A new ring in frames of its own.
fn frame_ring() -> Result<CpuRing, RingInitError> {
    Ring::shared(FramePages::allocate()?)
        .map(|ring| ring.when_full(WHEN_FULL))
        .map_err(RingInitError::Layout)
}

/// A new ring in `words` of the host ring region, which overwrites its oldest records when
/// full: a host reader never consumes, so the ring must not wait for one.
#[cfg(feature = "trace_host")]
fn host_ring(words: &'static [AtomicU64]) -> Result<CpuRing, RingInitError> {
    Ring::shared(FramePages::region(words))
        .map(|ring| ring.when_full(WhenFull::OverwriteOldest))
        .map_err(RingInitError::Layout)
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

/// Why a user-space record was refused (`kernel.trace/emit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserEmitError {
    /// The id names no event user space may write (`ferric_trace::kernel::USER_EVENTS`).
    NotUserEvent,
    /// The record does not have exactly the event's arguments.
    ArgCount,
    /// The event is not compiled into this kernel.
    NotCompiled(&'static Event),
}

impl fmt::Display for UserEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserEmitError::NotUserEvent => f.write_str("not an event user space may write"),
            UserEmitError::ArgCount => f.write_str("wrong number of arguments for the event"),
            UserEmitError::NotCompiled(event) => {
                write!(f, "{} is not compiled into this kernel", event.name())
            }
        }
    }
}

/// Stores a record user space wrote: one of `ferric_trace::kernel::USER_EVENTS`, with
/// exactly its arguments, stamped and written like a kernel record if the event is on.
/// Returns whether the event was on (a filter may still have dropped the record).
///
/// Call it where interrupts are off, as for `trace_event!` (a system call).
///
/// # Errors
///
/// [`UserEmitError`] for an event user space may not write, a wrong argument count, or an
/// event this kernel does not compile in.
pub fn emit_user(record: &UserRecord) -> Result<bool, UserEmitError> {
    let event = ferric_trace::kernel::user_event(record.id()).ok_or(UserEmitError::NotUserEvent)?;
    if record.args().len() != event.args().len() {
        return Err(UserEmitError::ArgCount);
    }
    if !compiled_in(event) {
        return Err(UserEmitError::NotCompiled(event));
    }
    if !is_on(event.id()) {
        return Ok(false);
    }
    emit(event, record.args());
    Ok(true)
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
    // A full ring counts the record (or the oldest it overwrote) as lost itself.
    let _ = ring.push(event.id(), super::clock::now(), args);
}
