//! Mapping a large file: map it, touch every page, unmap it.
//!
//! The file is written once (`--mmap-mib`, 32 MiB by default) and each sample maps the whole
//! of it read-only and private (`PROT_READ`, `MAP_PRIVATE`), as `ld.so` maps the objects it
//! loads, reads one byte of every 4 KiB page, and unmaps it, timing the three phases apart:
//!
//! - `mmap-map`: the `mmap` call.
//! - `mmap-touch`: the first read of every page of the new mapping.
//! - `mmap-unmap`: the `munmap` call.
//!
//! Where the work falls differs between systems, so compare the sum of the three across them:
//!
//! - On Redox, the kernel asks RedoxFS for the range, and RedoxFS reads it into a buffer of
//!   its own the first time and keeps that buffer, unmapped, while the file does not change.
//!   The kernel then maps every page of the range copy-on-write from that buffer before
//!   `mmap` returns. So the untimed warm-up mapping reads the file; each timed `mmap-map` is a
//!   request to RedoxFS and the kernel mapping 8192 pages (32 MiB), `mmap-touch` takes no
//!   page faults (it is the TLB misses of reading mapped memory), and `mmap-unmap` removes
//!   the 8192 pages and tells RedoxFS.
//! - On Linux, `mmap` only reserves the range and `mmap-touch` takes a page fault per page
//!   (served from the page cache).
//!
//! The sum of the bytes read is checked against what was written.

use std::fs::File;
use std::io::Write;
use std::time::Duration;

use super::file::Scratch;
use super::{BenchError, Config, Measurement, Op, file_error};
use crate::measure::{Monotonic, Plan, measure};
use crate::sys::Mapping;

/// The plan for mapping the file.
const PLAN: Plan = Plan {
    warmup: 1,
    iters: 20,
    min_iters: 3,
    budget: Duration::from_secs(3),
};

/// The distance between the bytes `mmap-touch` reads: the x86-64 page size.
const PAGE: usize = 4096;

/// The byte the file is filled with; not zero, so no page of it can pass for a hole.
const FILL: u8 = 0x5a;

/// The `mmap` group.
pub fn run(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    let scratch = Scratch::new(&config.dir, "mmap")?;
    let path = scratch.path().join("large");
    let mib = usize::try_from(config.mmap_mib).unwrap_or(usize::MAX);
    let len = mib.saturating_mul(1 << 20);
    {
        let mut file = File::create_new(&path).map_err(file_error(Op::Create, &path))?;
        let chunk = vec![FILL; 1 << 20];
        for _ in 0..mib {
            file.write_all(&chunk)
                .map_err(file_error(Op::Write, &path))?;
        }
    }
    let file = File::open(&path).map_err(file_error(Op::Open, &path))?;
    let expected = u64::from(FILL) * len.div_ceil(PAGE) as u64;
    let [map, touch, unmap] = measure(&config.plan(PLAN), &mut Monotonic::new(), |laps| {
        let mapping = Mapping::new(&file, len).map_err(file_error(Op::Map, &path))?;
        laps.lap();
        let got = mapping.touch(PAGE);
        laps.lap();
        mapping.unmap().map_err(file_error(Op::Unmap, &path))?;
        laps.lap();
        if got == expected {
            Ok(())
        } else {
            Err(BenchError::MappedContent {
                path: path.clone(),
                expected,
                got,
            })
        }
    })?
    .phases;
    drop(file);
    scratch.remove()?;
    let bytes = Some(len as u64);
    Ok(vec![
        Measurement {
            name: "mmap-map",
            latency: map,
            bytes,
            extra: Vec::new(),
        },
        Measurement {
            name: "mmap-touch",
            latency: touch,
            bytes,
            extra: Vec::new(),
        },
        Measurement {
            name: "mmap-unmap",
            latency: unmap,
            bytes: None,
            extra: Vec::new(),
        },
    ])
}
