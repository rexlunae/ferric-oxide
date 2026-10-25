//! Ferric Oxide: the crash dump (`kernel.trace_crash`; layout in `ferric_trace::crash`).
//!
//! The kernel keeps `REGION_BYTES` of RAM at a fixed physical address (`trace.crash_base` in its
//! `config.toml`, 64 MiB by default) out of the frame allocator, so the same memory is found
//! again after a warm reboot, where firmware leaves RAM as it was:
//!
//! - [`reserve`], during memory setup, takes the region out of the free memory
//!   ([`crate::reserved`]), provided it is wholly free RAM in the boot memory map.
//! - [`init`], in `kmain`, copies a complete dump the previous boot left there to the heap (the
//!   scheme's `crash` file serves it), logs its panic message, and arms the region again.
//! - [`dump`], from the panic handler, writes the panic message, the timestamp clock and the
//!   newest records of every CPU's ring there. Other CPUs keep running meanwhile; their records
//!   are copied with atomic loads, and a torn one is rejected by the reader.
//!
//! With `kernel.trace_crash_test`, writing `panic` to the scheme's `crash` file calls
//! [`test_panic`], after which the panic handler resets the machine instead of halting, so CI
//! can read the dump back on the next boot.

use alloc::{sync::Arc, vec::Vec};
use core::{
    fmt::Write,
    panic::PanicInfo,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use ferric_trace::crash::{Dump, MessageBuf, Writer, REGION_BYTES};
use spin::Once;

use crate::{cpu_set::LogicalCpuId, reserved::Region};

/// The region's physical address without `trace.crash_base`.
const DEFAULT_BASE: usize = 64 << 20;

/// The region, once [`reserve`] took it out of the free memory.
static RESERVED: Once<Region> = Once::new();

/// The region's words, once [`init`] armed it.
static REGION: Once<&'static [AtomicU64]> = Once::new();

/// The dump the previous boot left, if any.
static SAVED: Once<Arc<[u8]>> = Once::new();

/// Set by the first panic to dump, so a panic during the dump does not start another.
static DUMPING: AtomicBool = AtomicBool::new(false);

/// Set by [`test_panic`]: reset the machine once the dump is written.
static RESET: AtomicBool = AtomicBool::new(false);

/// The region's physical address: `trace.crash_base` (the build script checked it is a
/// page-aligned number), or [`DEFAULT_BASE`].
fn base() -> usize {
    option_env!("FERRIC_TRACE_CRASH_BASE")
        .and_then(|base| base.parse().ok())
        .unwrap_or(DEFAULT_BASE)
}

/// Takes the region out of the free memory, if all of it is free RAM in the boot memory map that
/// no other area overlaps, as `is_free(start, end)` says
/// ([`ferric_trace::crash::region_is_free`]). Called during memory setup, after the bootloader's areas are
/// registered and before the free areas are built from them.
pub fn reserve(is_free: impl Fn(usize, usize) -> bool) {
    let base = base();
    match Region::reserve(base, REGION_BYTES, is_free) {
        Ok(region) => {
            RESERVED.call_once(|| region);
        }
        Err(err) => {
            let end = base.saturating_add(REGION_BYTES);
            crate::warn!("trace: crash region {base:#x}..{end:#x} is {err}; no crash dumps");
        }
    }
}

/// Saves the dump the previous boot left, if there is one, and arms the region for this boot.
/// Called from `kmain` once the heap exists.
pub fn init() {
    let Some(region) = RESERVED.get() else {
        return;
    };
    let words = region.words();
    match Dump::len_in(words) {
        Ok(len) => save(words, len),
        Err(err) => crate::debug!("trace: no crash dump from the previous boot: {err}"),
    }
    if let Err(err) = Writer::arm(words) {
        crate::warn!("trace: crash region not armed: {err}");
        return;
    }
    REGION.call_once(|| words);
    crate::info!(
        "trace: crash region at {:#x} ({} KiB)",
        region.base(),
        region.bytes() / 1024
    );
}

/// Copies the `len` bytes of the dump in `words` to the heap and logs its message.
fn save(words: &[AtomicU64], len: usize) {
    let bytes: Vec<u8> = words
        .iter()
        .flat_map(|word| word.load(Ordering::Relaxed).to_le_bytes())
        .take(len)
        .collect();
    let Ok(dump) = Dump::parse(&bytes) else {
        // The region changed under the copy, which nothing at this point of boot does.
        crate::warn!("trace: the crash dump from the previous boot changed while it was read");
        return;
    };
    if let Some((cpu, message)) = dump.message() {
        let message = core::str::from_utf8(message).unwrap_or("(not UTF-8)");
        crate::warn!("trace: the previous boot panicked on CPU {cpu}: {message}");
    }
    crate::info!("trace: its crash dump ({len} bytes) is in /scheme/kernel.trace/crash");
    SAVED.call_once(|| Arc::from(bytes));
}

/// The dump the previous boot left, if any.
pub fn saved() -> Option<Arc<[u8]>> {
    SAVED.get().cloned()
}

/// Writes the panic message `info` and the newest records of every CPU's ring to the region.
/// Called first thing by the panic handler; does nothing before [`init`] or in a panic during
/// the dump.
pub fn dump(info: &PanicInfo) {
    let Some(words) = REGION.get() else {
        return;
    };
    if DUMPING.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut message = MessageBuf::new();
    let _ = write!(message, "{info}");
    let Ok(mut writer) = Writer::begin(words) else {
        return;
    };
    writer.message(crate::cpu_id().get(), message.as_bytes());
    writer.clock(super::clock::current());
    let count = crate::cpu_count();
    for cpu in 0..count {
        let Some(ring) = super::ring(LogicalCpuId::new(cpu)) else {
            continue;
        };
        // An equal share of what is left for each CPU still to go.
        let share = writer
            .remaining()
            .checked_div(count.saturating_sub(cpu) as usize)
            .unwrap_or(0);
        writer.cpu(cpu, ring, share);
    }
    writer.finish();
    crate::println!("trace: crash dump written");
}

/// Whether the panic handler should reset the machine after the dump, as it does after
/// [`test_panic`].
pub fn reset_after_panic() -> bool {
    RESET.load(Ordering::Acquire)
}

/// Panics on purpose, for testing the dump across a reboot (`kernel.trace_crash_test`): the
/// panic handler writes the dump and resets the machine.
#[cfg(feature = "trace_crash_test")]
pub fn test_panic() -> ! {
    RESET.store(true, Ordering::Release);
    panic!("ferric-trace crash test (kernel.trace_crash_test)");
}
