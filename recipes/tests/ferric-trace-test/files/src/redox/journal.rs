//! The mapping journal: fixed replacement, `mremap` moves, a fork's clone with the grant it
//! leaves out, and the copy's teardown.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use ferric_trace::Event;
use ferric_trace::kernel::{MM_CLONE, MM_CLONE_SKIP, MM_DESTROY, MM_MAP, MM_MOVE, MM_REPLACE};
use syscall::{Map, MapFlags, MremapFlags, SYS_MREMAP};

use super::lifecycle::spawn_child;
use super::mapping::{anonymous, ordered, succeeded};
use super::{Entry, Failure, PAGE, check, command, drain, has_event, io_failure};

const STEP: &str = "trace-journal";
const EVENTS: &str = "mm.map mm.replace mm.move mm.clone mm.clone_skip mm.destroy";
const JOURNAL: [&Event; 6] = [
    &MM_MAP,
    &MM_REPLACE,
    &MM_MOVE,
    &MM_CLONE,
    &MM_CLONE_SKIP,
    &MM_DESTROY,
];

/// Physically contiguous memory, which a fork does not copy (`mm.clone_skip`); the test runs
/// as root, which the memory scheme requires.
const CONTIGUOUS: &str = "/scheme/memory/zeroed@wb?phys_contiguous";

/// The size of each mapping, in pages: a count the allocator's own mappings are unlikely to
/// share, so an exact record cannot match one of them.
const PAGES: usize = 3;

/// How long the forked child's address space may take to be torn down after its `wait`.
const TEARDOWN: Duration = Duration::from_secs(5);

/// The addresses the operations used, which the records must carry.
struct Addresses {
    /// The first anonymous mapping, replaced in place and then moved.
    first: usize,
    /// Where the kernel moved it, at an address it picked.
    moved: usize,
    /// The second anonymous mapping, which the final fixed move replaced.
    target: usize,
    /// The page of contiguous memory mapped across the fork.
    contiguous: usize,
    /// The forked child's process id.
    child: u32,
}

fn rw() -> MapFlags {
    MapFlags::PROT_READ | MapFlags::PROT_WRITE
}

/// `mremap` of the `PAGES` at `old` to `new`, with `flags` and read-write access.
fn mremap(old: usize, new: usize, flags: MremapFlags) -> syscall::Result<usize> {
    let size = PAGES * PAGE;
    // SAFETY: moves only this test's own anonymous memory, which no Rust reference points into.
    unsafe { syscall::syscall5(SYS_MREMAP, old, size, new, size, flags.bits() | rw().bits()) }
}

/// Replaces a mapping in place and moves it twice, the mapping operations before the fork.
fn mapping_operations() -> Result<(usize, usize, usize), Failure> {
    let first = anonymous(STEP, PAGES)?;
    let fixed = Map {
        offset: 0,
        size: PAGES * PAGE,
        flags: rw() | MapFlags::MAP_PRIVATE | MapFlags::MAP_FIXED,
        address: first,
    };
    // SAFETY: replaces exactly the private allocation above, which nothing refers to.
    let replaced = succeeded(STEP, "fixed mmap", unsafe { syscall::fmap(!0, &fixed) })?;
    if replaced != first {
        return Err(check(STEP, format!("fixed mmap moved to {replaced:#x}")));
    }
    // Made while `first` is still mapped, so it cannot reuse that address.
    let target = anonymous(STEP, PAGES)?;
    let moved = succeeded(STEP, "mremap", mremap(first, 0, MremapFlags::empty()))?;
    let landed = succeeded(
        STEP,
        "fixed mremap",
        mremap(moved, target, MremapFlags::FIXED_REPLACE),
    )?;
    if landed != target {
        return Err(check(STEP, format!("fixed mremap moved to {landed:#x}")));
    }
    // SAFETY: drops the moved allocation, the last of this test's memory; nothing refers to it.
    succeeded(STEP, "munmap", unsafe {
        syscall::funmap(target, PAGES * PAGE)
    })?;
    Ok((first, moved, target))
}

/// Forks a child while a page of contiguous memory is mapped, then unmaps that page; returns
/// its address and the child's process id.
fn fork_with_contiguous() -> Result<(usize, u32), Failure> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(CONTIGUOUS)
        .map_err(io_failure(STEP, CONTIGUOUS))?;
    let map = Map {
        offset: 0,
        size: PAGE,
        flags: rw() | MapFlags::MAP_PRIVATE,
        address: 0,
    };
    let fd = usize::try_from(file.as_raw_fd()).map_err(|_| check(STEP, "negative descriptor"))?;
    // SAFETY: the kernel picks unused memory for this mapping, and no Rust reference points
    // into it.
    let contiguous = succeeded(STEP, "contiguous mmap", unsafe { syscall::fmap(fd, &map) })?;
    let child = spawn_child(STEP);
    // SAFETY: drops the contiguous page mapped above, which nothing refers to.
    let unmapped = unsafe { syscall::funmap(contiguous, PAGE) };
    let child = child?;
    succeeded(STEP, "contiguous munmap", unmapped)?;
    Ok((contiguous, child))
}

/// The `mm.clone` records of this process's space, from `space`.
fn clones<'a>(trace: &'a [Vec<Entry>], space: u64) -> impl Iterator<Item = &'a Entry> + 'a {
    trace
        .iter()
        .flatten()
        .filter(move |e| e.is(&MM_CLONE) && e.arg(0) == Some(space))
}

/// The new address space of this process's one successful `mm.clone`, from `space`.
///
/// # Errors
/// `no mm.clone of this address space` when the trace has none, `more than one mm.clone
/// for one fork` when it has several.
fn cloned(trace: &[Vec<Entry>], space: u64) -> Result<&Entry, Failure> {
    let mut clones = clones(trace, space);
    let clone = clones
        .next()
        .ok_or_else(|| check(STEP, "no mm.clone of this address space"))?;
    if clones.next().is_some() {
        return Err(check(STEP, "more than one mm.clone for one fork"));
    }
    let new = clone.arg(1).unwrap_or(0);
    let copied = clone.arg(2).unwrap_or(0) > 0 && clone.arg(3).unwrap_or(0) > 0;
    if clone.arg(4) != Some(0) || new == 0 || new == space || !copied {
        return Err(check(STEP, format!("bad mm.clone {:x?}", clone.args)));
    }
    Ok(clone)
}

/// Whether the forked copy has been torn down after it was made.
fn destroyed(trace: &[Vec<Entry>], clone: &Entry) -> bool {
    trace
        .iter()
        .flatten()
        .any(|e| e.is(&MM_DESTROY) && e.arg(0) == clone.arg(1) && e.timestamp >= clone.timestamp)
}

/// Drains until the fork's copy has been torn down: the child's exec replaces it, and the
/// kernel can finish that after `wait` returns. The fork's records may also arrive in a
/// later drain than the first: they are written into whatever CPU ring the fork ran on, and
/// a drain walks the rings one by one, so a record can land behind a drain that already
/// passed its ring (a full ring can drop it too; the step's earlier drain before the fork
/// makes that unlikely, and a rerun shows a drop). So absence of the clone alone is not a
/// failure yet; only the limit ending without the pair is. A record that appears twice
/// still fails at once; `verify` checks the arguments once the records are all in.
fn collect(
    cpus: &[u32],
    space: u64,
    mut trace: Vec<Vec<Entry>>,
) -> Result<Vec<Vec<Entry>>, Failure> {
    let limit = Instant::now() + TEARDOWN;
    loop {
        // The clone's facts, copied out: nothing may borrow `trace` past the drain below.
        let (mut new_space, mut clone_at, mut duplicates, mut torn) = (None, None, false, false);
        for clone in clones(&trace, space) {
            if new_space.is_none() {
                new_space = clone.arg(1);
                clone_at = Some(clone.timestamp);
            } else {
                duplicates = true;
            }
        }
        if duplicates {
            return Err(check(STEP, "more than one mm.clone for one fork"));
        }
        if let (Some(new), Some(at)) = (new_space, clone_at) {
            torn = trace.iter().flatten().any(|e| {
                e.is(&MM_DESTROY) && e.arg(0) == Some(new) && e.timestamp >= at
            });
            if torn {
                return Ok(trace);
            }
        }
        if Instant::now() >= limit {
            return Err(check(
                STEP,
                if new_space.is_some() {
                    "the forked address space was not destroyed"
                } else {
                    "no mm.clone of this address space"
                },
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
        trace.extend(drain(STEP, cpus)?);
    }
}

/// This process's page-table root, from the anonymous mapping that returned `first`.
fn space(trace: &[Vec<Entry>], first: usize) -> Result<u64, Failure> {
    trace
        .iter()
        .flatten()
        .find(|e| e.is(&MM_MAP) && e.arg(1) == Some(0) && e.arg(4) == Some(first as u64))
        .and_then(|e| e.arg(0))
        .filter(|space| *space != 0 && space % PAGE as u64 == 0)
        .ok_or_else(|| check(STEP, "no anonymous mm.map with a page-table root"))
}

/// Checks the exact replace, map and move records in order, then the clone and its teardown.
fn verify(trace: &[Vec<Entry>], at: &Addresses) -> Result<(), Failure> {
    let space = space(trace, at.first)?;
    let pages = PAGES as u64;
    let private = rw() | MapFlags::MAP_PRIVATE;
    let (first, moved, target) = (at.first as u64, at.moved as u64, at.target as u64);
    let after = ordered(
        STEP,
        trace,
        &MM_MAP,
        [space, 0, pages, private.bits() as u64, first],
        0,
    )?
    .timestamp;
    let after = ordered(STEP, trace, &MM_REPLACE, [space, first, pages, 0], after)?.timestamp;
    let fixed = private | MapFlags::MAP_FIXED;
    let after = ordered(
        STEP,
        trace,
        &MM_MAP,
        [space, first, pages, fixed.bits() as u64, first],
        after,
    )?
    .timestamp;
    let after = ordered(
        STEP,
        trace,
        &MM_MAP,
        [space, 0, pages, private.bits() as u64, target],
        after,
    )?
    .timestamp;
    let any = [
        space,
        space,
        first,
        pages,
        0,
        pages,
        rw().bits() as u64,
        moved,
    ];
    let after = ordered(STEP, trace, &MM_MOVE, any, after)?.timestamp;
    let after = ordered(STEP, trace, &MM_REPLACE, [space, target, pages, 0], after)?.timestamp;
    let flags = (rw() | MapFlags::MAP_FIXED).bits() as u64;
    let to = [space, space, moved, pages, target, pages, flags, target];
    let after = ordered(STEP, trace, &MM_MOVE, to, after)?.timestamp;
    let clone = cloned(trace, space)?;
    if clone.timestamp < after || !destroyed(trace, clone) {
        return Err(check(STEP, "mm.clone out of order or not destroyed"));
    }
    let new = clone.arg(1).unwrap_or(0);
    let left_out = [space, new, at.contiguous as u64, 1];
    let skip = ordered(STEP, trace, &MM_CLONE_SKIP, left_out, after)?;
    if skip.timestamp > clone.timestamp {
        return Err(check(STEP, "mm.clone_skip after its mm.clone"));
    }
    Ok(())
}

/// No journal record may appear once the events are off, for the same operations.
fn disabled(cpus: &[u32]) -> Result<(), Failure> {
    drain(STEP, cpus)?;
    mapping_operations()?;
    if drain(STEP, cpus)?
        .iter()
        .flatten()
        .any(|e| JOURNAL.iter().any(|event| e.is(event)))
    {
        return Err(check(STEP, "mapping journal record after disable"));
    }
    Ok(())
}

/// `trace-journal`: fixed replacement, kernel-placed and fixed `mremap`, a fork's clone and
/// the copy's teardown after the child's exec, each recorded exactly; disable stops them.
///
/// # Errors
/// Returns a failed system call, control operation or record assertion.
pub(super) fn check_journal(cpus: &[u32]) -> Result<(), Failure> {
    for event in JOURNAL {
        if !has_event(STEP, event)? {
            return Ok(());
        }
    }
    command(STEP, &format!("disable {EVENTS}"))?;
    drain(STEP, cpus)?;
    command(STEP, &format!("enable {EVENTS}"))?;
    let result = (|| {
        let (first, moved, target) = mapping_operations()?;
        // While the journal events are on, every mapping operation of every process is
        // recorded, and on a busy boot the rings drop records from the moment they fill
        // (`stats` counts the drops). Drain before the fork, keeping the mapping records,
        // so the `mm.clone` and `mm.clone_skip` the fork is about to write land in rings
        // with a whole ring of room until the next drain.
        let mut trace = drain(STEP, cpus)?;
        let (contiguous, child) = fork_with_contiguous()?;
        let at = Addresses {
            first,
            moved,
            target,
            contiguous,
            child,
        };
        let space = space(&trace, at.first)?;
        collect(cpus, space, trace).map(|trace| (trace, at))
    })();
    // Off again even when a step failed, so later steps are not traced.
    command(STEP, &format!("disable {EVENTS}"))?;
    let (mut trace, at) = result?;
    trace.extend(drain(STEP, cpus)?);
    verify(&trace, &at)?;
    disabled(cpus)?;
    println!(
        "FERRIC_TEST_PASS trace-journal (fixed replacement, kernel-placed and fixed mremap, fork clone of child {} and its teardown, disable)",
        at.child
    );
    Ok(())
}
