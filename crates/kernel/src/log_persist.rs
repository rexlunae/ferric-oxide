//! Ferric Oxide: the persistent kernel log (`kernel.log_persist`; layout in
//! `ferric_trace::klog`).
//!
//! The kernel keeps `REGION_BYTES` of RAM at a fixed physical address (`log.persist_base` in
//! its `config.toml`, just above the crash region by default) out of the frame allocator
//! ([`crate::reserved`]), and copies everything it logs there as well as to its in-memory log,
//! so after a warm reboot the next boot finds the end of the previous boot's log:
//!
//! - [`reserve`], during memory setup, takes the region out of the free memory.
//! - [`init`], in `kmain`, copies the log the previous boot left to the heap (the `sys`
//!   scheme's `log_previous` file serves it), arms the region for this boot, and copies the
//!   in-memory log so far into it.
//! - [`append`], from `Log::write` under the in-memory log's lock, appends every logged byte,
//!   the panic handler's message included.

use alloc::{sync::Arc, vec};

use ferric_trace::klog::{Log, Previous, REGION_BYTES};
use spin::Once;

use crate::reserved::Region;

/// The region's physical address without `log.persist_base`: right after the crash region's
/// default (64 MiB, 64 KiB long).
const DEFAULT_BASE: usize = (64 << 20) + (64 << 10);

/// The region, once [`reserve`] took it out of the free memory.
static RESERVED: Once<Region> = Once::new();

/// This boot's log in the region, once [`init`] armed it.
static LOG: Once<Log<'static>> = Once::new();

/// The log the previous boot left, if any.
static PREVIOUS: Once<Arc<[u8]>> = Once::new();

/// The region's physical address: `log.persist_base` (the build script checked it is a
/// page-aligned number), or [`DEFAULT_BASE`].
fn base() -> usize {
    option_env!("FERRIC_LOG_PERSIST_BASE")
        .and_then(|base| base.parse().ok())
        .unwrap_or(DEFAULT_BASE)
}

/// Takes the region out of the free memory, if all of it is free RAM in the boot memory map
/// that no other area overlaps, as `is_free(start, end)` says. Called during memory setup,
/// after the bootloader's areas (and the crash region) are registered and before the free
/// areas are built from them.
pub fn reserve(is_free: impl Fn(usize, usize) -> bool) {
    let base = base();
    match Region::reserve(base, REGION_BYTES, is_free) {
        Ok(region) => {
            RESERVED.call_once(|| region);
        }
        Err(err) => {
            let end = base.saturating_add(REGION_BYTES);
            warn!("log: persistent log region {base:#x}..{end:#x} is {err}; no persistent log");
        }
    }
}

/// Saves the log the previous boot left, if there is one, arms the region for this boot and
/// copies this boot's log so far into it. Called from `kmain` once the heap exists.
pub fn init() {
    let Some(region) = RESERVED.get() else {
        return;
    };
    let words = region.words();
    let boot = match Previous::find(words) {
        Ok(previous) => {
            let mut text = vec![0; previous.kept()];
            previous.copy(words, &mut text);
            PREVIOUS.call_once(|| Arc::from(text));
            previous.boot().wrapping_add(1)
        }
        Err(err) => {
            debug!("log: no persistent log from the previous boot: {err}");
            0
        }
    };
    {
        // Under the in-memory log's lock, so no line is lost or written twice between the copy
        // and the first `append`.
        let log = crate::log::LOG.lock();
        let Ok(persistent) = Log::arm(words, boot) else {
            // `Region::reserve` checked the size, which holds a header and data.
            return;
        };
        if let Some(log) = &*log {
            let (older, newer) = log.read();
            persistent.append(older);
            persistent.append(newer);
        }
        LOG.call_once(|| persistent);
    }
    if let Some(previous) = PREVIOUS.get() {
        info!(
            "log: the previous boot's log ({} bytes) is in /scheme/sys/log_previous",
            previous.len()
        );
    }
    info!(
        "log: persistent log at {:#x} ({} KiB), boot {boot}",
        region.base(),
        region.bytes() / 1024
    );
}

/// Appends `bytes` to this boot's persistent log, once [`init`] armed it. Called by
/// `Log::write` with the in-memory log's lock held, which keeps to one writer at a time.
pub fn append(bytes: &[u8]) {
    if let Some(log) = LOG.get() {
        log.append(bytes);
    }
}

/// The log the previous boot left, if this boot found one.
pub fn previous() -> Option<Arc<[u8]>> {
    PREVIOUS.get().cloned()
}
