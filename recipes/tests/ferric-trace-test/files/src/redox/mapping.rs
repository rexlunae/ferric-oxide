//! Mapping-operation results: target address spaces, refusals, peer contexts and live disable.

use std::fs::OpenOptions;
use std::io::Write;

use ferric_trace::abi;
use ferric_trace::kernel::{MM_MAP, MM_PROTECT, MM_UNMAP};
use syscall::{EACCES, EEXIST, EINVAL, Map, MapFlags};

use super::{
    Entry, Failure, PAGE, Why, check, drain, errno_result, expect_errno, has_event, io_failure,
    map_cpus, path,
};

const PAGES: usize = 2;

fn succeeded(call: &'static str, result: syscall::Result<usize>) -> Result<usize, Failure> {
    result.map_err(|err| Failure {
        step: "trace-mapping",
        why: Why::Call {
            call,
            result: Err(err),
        },
    })
}

/// Makes fresh anonymous memory; no fixed address or other live mapping is replaced.
fn anonymous() -> Result<usize, Failure> {
    let map = Map {
        offset: 0,
        size: PAGES * PAGE,
        flags: MapFlags::MAP_PRIVATE | MapFlags::PROT_READ | MapFlags::PROT_WRITE,
        address: 0,
    };
    // SAFETY: the kernel selects unused memory and this call creates no Rust references.
    succeeded("anonymous mmap", unsafe { syscall::fmap(!0, &map) })
}

/// Matches an exact result and preserves the sequence of this test's operations across CPUs.
fn ordered<'a>(
    trace: &'a [Vec<Entry>],
    event: &ferric_trace::Event,
    args: [u64; 5],
    after: u64,
) -> Result<&'a Entry, Failure> {
    let mut matches = trace
        .iter()
        .flatten()
        .filter(|e| e.is(event) && e.args == args);
    let entry = matches
        .next()
        .ok_or_else(|| check("trace-mapping", format!("no {} {args:x?}", event.name())))?;
    if matches.next().is_some() || entry.timestamp < after {
        return Err(check(
            "trace-mapping",
            format!("duplicate or unordered {} {args:x?}", event.name()),
        ));
    }
    Ok(entry)
}

/// Runs the mapping requests and checks their system-call results after cleanup.
fn operations(addr: usize, ring_addr: usize) -> Result<(), Failure> {
    let step = "trace-mapping";
    let rw = MapFlags::PROT_READ | MapFlags::PROT_WRITE;
    let fixed = Map {
        offset: 0,
        size: PAGES * PAGE,
        flags: rw | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE,
        address: addr,
    };
    // SAFETY: the requested range is exactly the private allocation above. NOREPLACE must
    // refuse overlap, and even an unexpected successful replacement affects no Rust reference.
    let refused_map = unsafe { syscall::fmap(!0, &fixed) };
    // SAFETY: this only narrows access to our private first page; nothing refers to it.
    let protected = unsafe { syscall::mprotect(addr, PAGE, MapFlags::PROT_READ) };
    let peer = std::thread::spawn(move || {
        // SAFETY: the other page belongs to the same still-live allocation, with no references.
        unsafe { syscall::mprotect(addr + PAGE, PAGE, MapFlags::PROT_READ) }
    })
    .join()
    .map_err(|_| check(step, "protection peer panicked"))?;
    // SAFETY: restores the original permissions on the same private allocation, still unused.
    let restored = unsafe { syscall::mprotect(addr, PAGES * PAGE, rw) };
    // SAFETY: the live ring mapping has no mutable references, and the kernel must refuse widening.
    let refused_protect =
        unsafe { syscall::mprotect(ring_addr, ferric_trace::abi::CPU_MAP_BYTES, rw) };
    let invalid = Map {
        address: addr + 1,
        ..fixed
    };
    // SAFETY: the kernel must reject this unaligned address before changing any mapping; it
    // lies inside our otherwise unused allocation and no Rust references point there.
    let (invalid_map, invalid_protect, invalid_unmap) = unsafe {
        (
            syscall::fmap(!0, &invalid),
            syscall::mprotect(addr + 1, PAGE, rw),
            syscall::funmap(addr + 1, PAGE),
        )
    };
    // SAFETY: drops the private allocation after the peer joined; no references point into it.
    let unmapped = unsafe { syscall::funmap(addr, PAGES * PAGE) };
    expect_errno(step, "overlapping fixed mmap", refused_map, EEXIST)?;
    succeeded("mprotect", protected)?;
    succeeded("peer mprotect", peer)?;
    succeeded("restore mprotect", restored)?;
    expect_errno(step, "writable ring mprotect", refused_protect, EACCES)?;
    expect_errno(step, "unaligned mmap", invalid_map, EINVAL)?;
    expect_errno(step, "unaligned mprotect", invalid_protect, EINVAL)?;
    expect_errno(step, "unaligned munmap", invalid_unmap, EINVAL)?;
    succeeded("munmap", unmapped)?;
    Ok(())
}

/// Verifies exact results, shared space identity, timestamps and early-validation exclusion.
fn verify(trace: &[Vec<Entry>], addr: usize, ring_addr: usize) -> Result<(), Failure> {
    let step = "trace-mapping";
    let rw = MapFlags::PROT_READ | MapFlags::PROT_WRITE;
    let created = trace
        .iter()
        .flatten()
        .find(|e| e.is(&MM_MAP) && e.arg(1) == Some(0) && e.arg(4) == Some(addr as u64))
        .ok_or_else(|| check(step, "no successful anonymous map"))?;
    let space = created
        .arg(0)
        .ok_or_else(|| check(step, "map has no address space"))?;
    if space == 0 || space % PAGE as u64 != 0 {
        return Err(check(step, "invalid page-table root"));
    }
    let mut after = ordered(
        trace,
        &MM_MAP,
        [
            space,
            0,
            PAGES as u64,
            (rw | MapFlags::MAP_PRIVATE).bits() as u64,
            addr as u64,
        ],
        0,
    )?
    .timestamp;
    for (event, args) in [
        (
            &MM_MAP,
            [
                space,
                addr as u64,
                PAGES as u64,
                (rw | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED_NOREPLACE).bits() as u64,
                errno_result(EEXIST),
            ],
        ),
        (
            &MM_PROTECT,
            [space, addr as u64, 1, MapFlags::PROT_READ.bits() as u64, 0],
        ),
        (
            &MM_PROTECT,
            [
                space,
                (addr + PAGE) as u64,
                1,
                MapFlags::PROT_READ.bits() as u64,
                0,
            ],
        ),
        (
            &MM_PROTECT,
            [space, addr as u64, PAGES as u64, rw.bits() as u64, 0],
        ),
        (
            &MM_PROTECT,
            [
                space,
                ring_addr as u64,
                (ferric_trace::abi::CPU_MAP_BYTES / PAGE) as u64,
                rw.bits() as u64,
                errno_result(EACCES),
            ],
        ),
        (&MM_UNMAP, [space, addr as u64, PAGES as u64, 0, 0]),
    ] {
        after = ordered(trace, event, args, after)?.timestamp;
    }
    if trace.iter().flatten().any(|e| {
        [&MM_MAP, &MM_PROTECT, &MM_UNMAP]
            .iter()
            .any(|event| e.is(event))
            && e.arg(1) == Some((addr + 1) as u64)
    }) {
        return Err(check(
            step,
            "pre-validation refusal reached an address-space operation",
        ));
    }
    Ok(())
}

/// No result event may remain after the three static keys have been disabled.
fn disabled(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-mapping";
    let disabled = anonymous()?;
    // SAFETY: the fresh disabled-phase allocation has no references or other users.
    succeeded("disabled munmap", unsafe {
        syscall::funmap(disabled, PAGES * PAGE)
    })?;
    if drain(step, cpus)?.iter().flatten().any(|e| {
        [&MM_MAP, &MM_PROTECT, &MM_UNMAP]
            .iter()
            .any(|event| e.is(event))
    }) {
        return Err(check(step, "mapping event after disable"));
    }
    Ok(())
}

/// Checks accepted, refused and pre-validation operations, a peer's target space and disable.
///
/// # Errors
/// Returns a failed system call, control operation or record assertion.
pub(super) fn check_mapping(cpus: &[u32]) -> Result<(), Failure> {
    let step = "trace-mapping";
    for event in [&MM_MAP, &MM_PROTECT, &MM_UNMAP] {
        if !has_event(step, event)? {
            return Ok(());
        }
    }
    // Map the read-only rings before enabling the events, so their setup stays outside the capture.
    let rings = map_cpus(step, cpus)?;
    let ring_addr = rings
        .first()
        .ok_or_else(|| check(step, "no CPU ring"))?
        .map
        .addr();
    // Open control before the capture, and disable with static bytes after unmap, so
    // control-path allocations cannot reuse the target range before capture ends.
    let control_path = path(abi::EVENTS_PATH);
    let mut control = OpenOptions::new()
        .write(true)
        .open(&control_path)
        .map_err(io_failure(step, &control_path))?;
    drain(step, cpus)?;
    control
        .write_all(b"enable mm.map mm.protect mm.unmap")
        .map_err(io_failure(step, &control_path))?;
    let addr = anonymous()?;
    operations(addr, ring_addr)?;
    control
        .write_all(b"disable mm.map mm.protect mm.unmap")
        .map_err(io_failure(step, &control_path))?;
    verify(&drain(step, cpus)?, addr, ring_addr)?;
    disabled(cpus)?;
    println!(
        "FERRIC_TEST_PASS trace-mapping (space identity across contexts, map/protect/unmap results, refusals, validation and disable)"
    );
    Ok(())
}
