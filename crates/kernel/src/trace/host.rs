//! Ferric Oxide (`kernel.trace_host`): trace rings a host reads straight out of the guest's RAM.
//!
//! Booted with `FERRIC_TRACE_HOST=<KiB per CPU>`, the kernel takes one physically contiguous
//! region from the boot-time allocator ([`reserve`], before the frame allocator exists and
//! before the other CPUs start), laid out by `ferric_trace::host::RegionLayout`: a directory
//! page, then one ring per CPU in the shared layout. The rings are made in it instead of in
//! separately allocated frames, and overwrite their oldest records when full, so the kernel
//! never waits for a reader. Once every ring in it is made and the trace clock is chosen,
//! [`publish`] writes the directory and logs where it is:
//!
//! `trace: host rings at phys 0x<hex>, <n> CPUs x <KiB> KiB`
//!
//! A host that maps the guest's RAM (QEMU `memory-backend-file,share=on`) finds the region
//! by that address, or by scanning for the directory, and follows the rings without ever
//! writing guest memory (`ferric-trace follow`). The host is the host rings' only reader:
//! reading or committing them through `kernel.trace/cpu/<n>` fails with `EBUSY`
//! (`scheme::trace`), because a guest consumer would move the `tail` an overtaken host
//! observer relies on, and mapping them fails, because the region's pages are not frames the
//! frame allocator can hand to a grant.
//!
//! Since the kernel can refuse the request, the `kernel.trace/host` file ([`line()`]) is what
//! says whether there are host rings: tests and scripts in the guest decide from it.
//!
//! The CPU count comes from the MADT, as for the APs `acpi::madt` starts; the region is sized
//! for that many CPUs, and a CPU beyond it (none, normally) gets an ordinary ring.

use core::sync::atomic::AtomicU64;

use ferric_abi::boot_env::FERRIC_TRACE_HOST;
use ferric_trace::host::{self, HostRingsLine, Published, RegionLayout, RingSize, DIRECTORY_WORDS};
use rmm::BumpAllocator;
use spin::Once;

use crate::{cpu_set::MAX_CPU_COUNT, memory::RmmA, reserved::Region};

/// The region [`reserve`] took and its layout.
struct HostRegion {
    region: Region,
    layout: RegionLayout,
}

/// The region, once [`reserve`] took it.
static REGION: Once<HostRegion> = Once::new();

/// Whether [`publish`] wrote the directory.
static PUBLISHED: Once<()> = Once::new();

/// Takes the host-visible ring region from the boot-time allocator if the boot environment
/// asks for it (`FERRIC_TRACE_HOST`), sized for the CPUs the MADT lists. Called on the boot
/// CPU after the ACPI tables are mapped and before the frame allocator takes the rest of
/// memory. A value that is not a ring size, or a region that does not fit, is logged and
/// leaves host rings off.
pub fn reserve(env: &[u8], allocator: &mut BumpAllocator<RmmA>) {
    let Some(value) = super::env_value(env, FERRIC_TRACE_HOST) else {
        return;
    };
    let size = match RingSize::parse(value) {
        Ok(size) => size,
        Err(err) => {
            crate::warn!("trace: {FERRIC_TRACE_HOST}={value:?} ignored: {err}");
            return;
        }
    };
    let cpus = boot_cpu_count();
    let layout = match RegionLayout::new(cpus, size) {
        Ok(layout) => layout,
        Err(err) => {
            crate::warn!("trace: {FERRIC_TRACE_HOST}={value:?} ignored: {err}");
            return;
        }
    };
    match Region::from_boot_allocator(allocator, layout.bytes()) {
        Ok(region) => {
            REGION.call_once(|| HostRegion { region, layout });
        }
        Err(err) => {
            crate::warn!(
                "trace: {FERRIC_TRACE_HOST}={value:?} ignored: {cpus} CPUs x {} KiB: {err}",
                size.kib()
            );
        }
    }
}

/// The CPUs the boot CPU will start, by the MADT, within what the kernel and a directory hold.
fn boot_cpu_count() -> usize {
    let bsp_apic_id = crate::arch::cpuid::cpuid()
        .get_feature_info()
        .map_or(0, |info| u32::from(info.initial_local_apic_id()));
    let limit = host::MAX_RINGS.min(MAX_CPU_COUNT as usize);
    crate::acpi::madt::early_cpu_count(bsp_apic_id)
        .unwrap_or(1)
        .clamp(1, limit)
}

/// Whether [`reserve`] took a region.
pub fn is_reserved() -> bool {
    REGION.is_completed()
}

/// Whether CPU `cpu`'s ring is a host ring, in the region: one the host follows and nothing
/// in the guest may consume. A CPU past the region's rings has an ordinary ring.
pub fn is_host_ring(cpu: usize) -> bool {
    REGION
        .get()
        .is_some_and(|host| host.layout.entry(cpu).is_some())
}

/// The `kernel.trace/host` line for the region: where it is, its rings and their size, or
/// `None` when [`reserve`] took none (no `FERRIC_TRACE_HOST`, or a refused one).
pub fn line() -> Option<HostRingsLine> {
    REGION
        .get()
        .map(|host| HostRingsLine::new(host.region.base() as u64, &host.layout))
}

/// The number of rings in the region, 0 without one.
pub fn cpus() -> usize {
    REGION.get().map_or(0, |host| host.layout.cpus())
}

/// CPU `cpu`'s ring words in the region, header page first, or `None` when the region has no
/// ring for it.
pub fn ring_words(cpu: usize) -> Option<&'static [AtomicU64]> {
    let host = REGION.get()?;
    let entry = host.layout.entry(cpu)?;
    let words = host.region.words();
    let in_words = |bytes: u64| {
        usize::try_from(bytes)
            .ok()?
            .checked_div(size_of::<AtomicU64>())
    };
    let start = in_words(entry.offset)?;
    words.get(start..start.checked_add(in_words(entry.bytes)?)?)
}

/// Writes the directory and logs where the region is, once, after every ring in it was made
/// (their headers written) and the trace clock chosen. Does nothing without a region.
pub fn publish() {
    let Some(host) = REGION.get() else {
        return;
    };
    PUBLISHED.call_once(|| {
        let words = host.region.words();
        let Some(directory) = words.get(..DIRECTORY_WORDS) else {
            return;
        };
        let published = Published {
            self_phys: host.region.base() as u64,
            generation: generation(),
            clock: super::clock::current(),
        };
        match host::publish(directory, &published, host.layout.entries()) {
            Ok(()) => {
                crate::info!(
                    "trace: host rings at phys {:#x}, {} CPUs x {} KiB",
                    host.region.base(),
                    host.layout.cpus(),
                    host.layout.ring_size().kib()
                );
            }
            Err(err) => {
                crate::warn!("trace: host ring directory not written: {err}");
            }
        }
    });
}

/// A value that differs from one boot to the next, for the directory: a hardware random
/// number, or the TSC without one.
fn generation() -> u64 {
    ferric_hwrand::u64().unwrap_or_else(crate::arch::misc::rdtsc)
}
