//! The steps on RedoxFS itself, which serves reads of files opened read-only from the page
//! cache when the kernel has one: a reader sees writes and truncations made through another
//! description, a writable mapping keeps a file out of the cache exactly while it lasts, and
//! the same-boot benchmark of cached reads against reads RedoxFS serves.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::time::Instant;

use ferric_blkqueue::ab::{self, Comparison, Verdict};

use crate::redox::{Failure, check};

/// The file the steps use, on the root file system.
const PATH: &str = "/tmp/ferric-pcache-test";
/// Its first contents: 8 pages and 10 bytes.
const LEN: usize = 8 * 4096 + 10;

/// The byte at `offset` of contents `generation`.
fn byte(generation: u8, offset: usize) -> u8 {
    (offset % 253) as u8 ^ generation.wrapping_mul(0x5b)
}

/// `len` bytes of contents `generation` from `offset`.
fn bytes(generation: u8, offset: usize, len: usize) -> Vec<u8> {
    (offset..offset + len)
        .map(|at| byte(generation, at))
        .collect()
}

fn io<T>(what: &'static str, result: std::io::Result<T>) -> Result<T, Failure> {
    result.map_err(|err| Failure::Errno {
        what,
        errno: err.raw_os_error().unwrap_or(-1),
    })
}

/// Reads all of `file` (up to `max` bytes) with `pread`s.
fn read_all(file: &File, max: usize) -> Result<Vec<u8>, Failure> {
    let mut buf = vec![0; max];
    let mut done = 0;
    while done < max {
        match io("pread", file.read_at(&mut buf[done..], done as u64))? {
            0 => break,
            n => done += n,
        }
    }
    buf.truncate(done);
    Ok(buf)
}

/// An access time long past: RedoxFS moves a file's access time forward when it reads the
/// file and the time is over an hour old.
const OLD_ATIME: i64 = 1000;

/// Whether a read of page `page` through `file` was served from the page cache. RedoxFS
/// updates the file's access time when it reads the file (to serve a read or to fill a
/// page), and the cache does not, so: set the time far back, read, and see whether it moved.
/// The page must be resident for a cached description (read it first), and `page` must not
/// follow the description's last read, so the read starts no readahead.
fn served_from_cache(file: &File, page: u64) -> Result<bool, Failure> {
    let old = libc::timespec {
        tv_sec: OLD_ATIME,
        tv_nsec: 0,
    };
    // RedoxFS takes the access and the modification time together.
    let times = [old, old];
    // SAFETY: `times` is two timespecs, as futimens reads, and the descriptor is open.
    let set = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
    if set != 0 {
        return io("futimens", Err(std::io::Error::last_os_error()));
    }
    let mut byte = [0];
    io("pread", file.read_exact_at(&mut byte, page * 4096))?;
    let atime = io("fstat", file.metadata())?.atime();
    Ok(atime == OLD_ATIME)
}

/// Checks `reader` reads exactly `want`.
fn expect(reader: &File, want: &[u8], after: &str) -> Result<(), Failure> {
    let got = read_all(reader, want.len() + 2 * 4096)?;
    let first_bad = got.iter().zip(want).position(|(g, w)| g != w);
    check(got.len() == want.len() && first_bad.is_none(), || {
        format!(
            "after {after} a reader got {} bytes, first wrong at {first_bad:?}, not the {} written",
            got.len(),
            want.len()
        )
    })
}

/// `pcache-redoxfs`: a description opened read-only follows writes, an append, a shrinking
/// and a growing truncation made through another one.
pub fn coherence() -> Result<(), Failure> {
    let mut want = bytes(1, 0, LEN);
    io("create the test file", fs::write(PATH, &want))?;
    let reader = io("open the test file", File::open(PATH))?;
    // Twice: the second read is served from the pages the first filled.
    expect(&reader, &want, "creating the file")?;
    expect(&reader, &want, "reading it again")?;
    let writer = io(
        "open the test file to write",
        OpenOptions::new().write(true).open(PATH),
    )?;
    // Across the boundary of pages 1 and 2.
    let at = 2 * 4096 - 100;
    let patch = bytes(2, at, 300);
    io("write", writer.write_all_at(&patch, at as u64))?;
    want[at..at + patch.len()].copy_from_slice(&patch);
    expect(&reader, &want, "a write")?;
    // Past the end: the file grows, and the reader sees the new end.
    let tail = bytes(3, LEN, 5000);
    io("extend", writer.write_all_at(&tail, LEN as u64))?;
    want.extend_from_slice(&tail);
    expect(&reader, &want, "a write past the end")?;
    // Cut inside page 3, then grown back: the bytes past the cut read as zero.
    let cut = 3 * 4096 + 5;
    io("truncate", writer.set_len(cut as u64))?;
    want.truncate(cut);
    expect(&reader, &want, "a truncation")?;
    io("grow", writer.set_len(5 * 4096))?;
    want.resize(5 * 4096, 0);
    expect(&reader, &want, "growing the file")?;
    // An append through a description at offset 0 writes at the end, and drops only the
    // pages it changed: page 1 stays cached.
    let appender = io(
        "open the test file to append",
        OpenOptions::new().append(true).open(PATH),
    )?;
    check(served_from_cache(&reader, 1)?, || {
        "a read-only description of a RedoxFS file was not served from the cache".into()
    })?;
    let more = bytes(5, want.len(), 100);
    // Its offset is 0; O_APPEND puts the bytes at the end.
    io("append", (&appender).write_all(&more))?;
    want.extend_from_slice(&more);
    check(served_from_cache(&reader, 1)?, || {
        "an append at the end dropped page 1 from the cache".into()
    })?;
    expect(&reader, &want, "an append")?;
    drop((reader, writer, appender));
    io("remove the test file", fs::remove_file(PATH))
}

/// Maps the first page of `file` writable, `MAP_SHARED` or `MAP_PRIVATE`.
fn map_page(file: &File, shared: bool) -> Result<*mut u8, Failure> {
    let kind = if shared {
        libc::MAP_SHARED
    } else {
        libc::MAP_PRIVATE
    };
    // SAFETY: a new mapping of a page of an open file, at an address the kernel picks.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            kind,
            file.as_raw_fd(),
            0,
        )
    };
    if page == libc::MAP_FAILED {
        return io("mmap", Err(std::io::Error::last_os_error()));
    }
    Ok(page.cast())
}

/// Unmaps a page [`map_page`] mapped.
fn unmap_page(page: *mut u8) -> Result<(), Failure> {
    // SAFETY: `page` is a whole page mapping `map_page` made, used no more.
    if unsafe { libc::munmap(page.cast(), 4096) } != 0 {
        return io("munmap", Err(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Opens the test file read-only, reads it all (filling the cache) and says whether it is
/// served from the cache.
fn open_and_check_cached(want: &[u8]) -> Result<(File, bool), Failure> {
    let reader = io("open the test file", File::open(PATH))?;
    expect(&reader, want, "opening it")?;
    let cached = served_from_cache(&reader, 2)?;
    Ok((reader, cached))
}

/// `pcache-redoxfs-mmap`: a file with a writable mapping, private or shared, is not cached
/// while the mapping lasts, whatever happens to the descriptor it was made through, and is
/// cached again once it is unmapped. A reader cached before the mapping sees what the
/// mapping writes back.
pub fn mappings() -> Result<(), Failure> {
    let mut want = bytes(6, 0, LEN);
    io("create the test file", fs::write(PATH, &want))?;
    let writer = io(
        "open the test file read-write",
        OpenOptions::new().read(true).write(true).open(PATH),
    )?;
    // The check itself: RedoxFS serves the read-write description.
    expect(&writer, &want, "creating the file")?;
    check(!served_from_cache(&writer, 2)?, || {
        "the access time did not move on a read RedoxFS served".into()
    })?;
    let (reader, cached) = open_and_check_cached(&want)?;
    check(cached, || "a read-only description was not cached".into())?;

    // A private writable mapping: RedoxFS writes it back on fsync, so the file leaves the
    // cache while it lasts, and the reader reads what RedoxFS has.
    let private = map_page(&writer, false)?;
    // SAFETY: `private` maps a whole page, writable, and nothing else uses it.
    unsafe { private.add(7).write(!want[7]) };
    io("fsync", writer.sync_all())?;
    let on_disk = read_all(&writer, want.len())?;
    expect(&reader, &on_disk, "a private writable mapping and fsync")?;
    let (_, cached) = open_and_check_cached(&on_disk)?;
    check(!cached, || {
        "a file with a private writable mapping was still cached".into()
    })?;
    unmap_page(private)?;
    want = read_all(&writer, want.len())?;
    let (_, cached) = open_and_check_cached(&want)?;
    check(cached, || "a file was not cached again after munmap".into())?;

    // A shared writable mapping whose descriptor closes first: the store reaches the file,
    // the reader cached before it sees it, and the file is cached again only after munmap.
    let (reader, cached) = open_and_check_cached(&want)?;
    check(cached, || "a read-only description was not cached".into())?;
    let shared = map_page(&writer, true)?;
    drop(writer);
    want[9] = !want[9];
    // SAFETY: `shared` maps a whole page, writable, and nothing else uses it.
    unsafe { shared.add(9).write(want[9]) };
    let syncer = io(
        "open the test file to sync",
        OpenOptions::new().write(true).open(PATH),
    )?;
    io("fsync", syncer.sync_all())?;
    drop(syncer);
    expect(&reader, &want, "a store through a shared mapping and fsync")?;
    let (_, cached) = open_and_check_cached(&want)?;
    check(!cached, || {
        "a file mapped writable, its descriptor closed, was still cached".into()
    })?;
    unmap_page(shared)?;
    let (again, cached) = open_and_check_cached(&want)?;
    check(cached, || {
        "a file unmapped after its descriptor closed was not cached again".into()
    })?;
    drop((reader, again));
    io("remove the test file", fs::remove_file(PATH))
}

/// `pread`s of 4 KiB timed per round, each way.
const PREADS: u32 = 2000;
/// Sequential 1 MiB reads of the benchmark file per round, each way.
const SEQUENTIAL: u32 = 32;
/// The benchmark file's size.
const BENCH_LEN: usize = 1 << 20;

/// One way of reading the benchmark file, and its rounds.
struct Way {
    file: File,
    pread4k: [u32; ab::MAX_ROUNDS],
    seq1m: [u32; ab::MAX_ROUNDS],
}

impl Way {
    fn new(file: File) -> Self {
        Self {
            file,
            pread4k: [0; ab::MAX_ROUNDS],
            seq1m: [0; ab::MAX_ROUNDS],
        }
    }

    /// Runs round `round`: 4 KiB `pread`s per second, then 1 MiB sequential reads per second.
    fn round(&mut self, round: usize) -> Result<(), Failure> {
        let mut page = vec![0; 4096];
        let start = Instant::now();
        for _ in 0..PREADS {
            io("pread", self.file.read_exact_at(&mut page, 0))?;
        }
        self.pread4k[round] = per_second(PREADS, start);
        let mut whole = vec![0; BENCH_LEN];
        let start = Instant::now();
        for _ in 0..SEQUENTIAL {
            io("read", self.file.read_exact_at(&mut whole, 0))?;
        }
        self.seq1m[round] = per_second(SEQUENTIAL, start);
        Ok(())
    }
}

fn per_second(count: u32, start: Instant) -> u32 {
    let ns = start.elapsed().as_nanos().max(1);
    u32::try_from(u128::from(count) * 1_000_000_000 / ns).unwrap_or(u32::MAX)
}

/// `pcache-redoxfs-bench`: warm 4 KiB `pread`s and 1 MiB sequential reads of one RedoxFS
/// file through a read-only description (served from the cache) against a read-write one
/// (served by RedoxFS, as with the option off), in the same boot, with the paired A/B rule
/// of `ferric_blkqueue::ab`. Fails when the cache is slower.
pub fn bench() -> Result<(), Failure> {
    io(
        "create the bench file",
        fs::write(PATH, bytes(4, 0, BENCH_LEN)),
    )?;
    let scheme = io(
        "open the bench file read-write",
        OpenOptions::new().read(true).write(true).open(PATH),
    )?;
    let cached = io("open the bench file", File::open(PATH))?;
    let (mut baseline, mut candidate) = (Way::new(scheme), Way::new(cached));
    // Warm both: the cache's pages, and RedoxFS's own buffers.
    baseline.round(0)?;
    candidate.round(0)?;
    let (mut rounds, mut batch) = (0, ab::ROUNDS);
    let (pread, seq) = loop {
        for round in rounds..rounds + batch {
            // Alternate the order, so drift during the run favours neither.
            if round % 2 == 0 {
                baseline.round(round)?;
                candidate.round(round)?;
            } else {
                candidate.round(round)?;
                baseline.round(round)?;
            }
        }
        rounds += batch;
        let pread = Comparison::new(&baseline.pread4k[..rounds], &candidate.pread4k[..rounds]);
        let seq = Comparison::new(&baseline.seq1m[..rounds], &candidate.seq1m[..rounds]);
        let confirm = [pread, seq].iter().any(|c| c.verdict() == Verdict::Confirm);
        if !confirm {
            break (pread, seq);
        }
        batch = ab::CONFIRM_ROUNDS;
    };
    drop((baseline, candidate));
    io("remove the bench file", fs::remove_file(PATH))?;
    for (name, cmp) in [("pread4k", pread), ("seq1m", seq)] {
        let verdict = if cmp.passes() { "pass" } else { "fail" };
        println!(
            "FERRIC_BENCH pcache-redoxfs-{name} scheme_per_s={} cached_per_s={} ab_ratio={} \
             ab_tolerance={} ab_rounds={rounds} ab={verdict}",
            cmp.baseline, cmp.candidate, cmp.ratio, cmp.tolerance
        );
    }
    check(pread.passes() && seq.passes(), || {
        "reads from the page cache were slower than reads RedoxFS served".into()
    })
}
