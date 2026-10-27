//! The steps on RedoxFS itself, which serves reads of files opened read-only from the page
//! cache when the kernel has one: a reader sees writes and truncations made through another
//! description, a writable mapping keeps a file out of the cache exactly while it lasts, and
//! the same-boot benchmark of cached reads against reads RedoxFS serves. With cached writes
//! (`kernel.page_cache_writeback`), every description but an `O_FSYNC` one is served from the
//! cache, writes too; the steps that need RedoxFS to serve a description open it `O_FSYNC`,
//! so they test the same thing either way, and the writeback steps check that cached writes
//! reach the file: on `fsync`, at the last close, after a while on their own, before a
//! mapping, and across an `O_TRUNC` open through another description.
//!
//! Whether a write was cached shows in the modification time: RedoxFS moves it when it
//! writes the file, and a cached write does not until it is written back.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::time::{Duration, Instant};

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
    set_old_times(file)?;
    let mut byte = [0];
    io("pread", file.read_exact_at(&mut byte, page * 4096))?;
    let atime = io("fstat", file.metadata())?.atime();
    Ok(atime == OLD_ATIME)
}

/// Sets `file`'s access and modification times to [`OLD_ATIME`].
fn set_old_times(file: &File) -> Result<(), Failure> {
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
    Ok(())
}

/// Whether RedoxFS has not written `file` since [`set_old_times`]: its modification time is
/// still the old one.
fn unwritten(file: &File) -> Result<bool, Failure> {
    Ok(io("fstat", file.metadata())?.mtime() == OLD_ATIME)
}

/// Writes `data` at `at` through `writer` and says whether the write was cached: RedoxFS
/// moves the modification time when it writes the file, and a cached write leaves it until
/// it is written back.
fn write_cached(writer: &File, data: &[u8], at: usize) -> Result<bool, Failure> {
    set_old_times(writer)?;
    io("pwrite", writer.write_all_at(data, at as u64))?;
    unwritten(writer)
}

/// Opens the test file with `O_FSYNC`, which RedoxFS serves itself, read-only or
/// read-write.
fn open_fs(write: bool) -> Result<File, Failure> {
    io(
        "open the test file O_FSYNC",
        OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_FSYNC)
            .open(PATH),
    )
}

/// Opens the test file write-only, without `O_FSYNC`: cached when writes are.
fn open_writer() -> Result<File, Failure> {
    io(
        "open the test file to write",
        OpenOptions::new().write(true).open(PATH),
    )
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
    set_old_times(&reader)?;
    drop(reader);
    let reader = io("reopen the test file", File::open(PATH))?;
    check(served_from_cache(&reader, 0)?, || {
        "closing the last description discarded the clean file's cached pages".into()
    })?;
    expect(&reader, &want, "reopening the retained object")?;
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

/// How [`map_page`] maps a page.
#[derive(Clone, Copy)]
enum Map {
    /// `PROT_READ`, `MAP_SHARED`.
    SharedRead,
    /// `PROT_READ | PROT_WRITE`, `MAP_SHARED`.
    SharedWrite,
    /// `PROT_READ | PROT_WRITE`, `MAP_PRIVATE`.
    PrivateWrite,
}

/// Maps the first page of `file`.
fn map_page(file: &File, map: Map) -> Result<*mut u8, Failure> {
    let (prot, kind) = match map {
        Map::SharedRead => (libc::PROT_READ, libc::MAP_SHARED),
        Map::SharedWrite => (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED),
        Map::PrivateWrite => (libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE),
    };
    // SAFETY: a new mapping of a page of an open file, at an address the kernel picks.
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 4096, prot, kind, file.as_raw_fd(), 0) };
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

/// Checks a read-only description opened now is cached, or is not.
fn expect_cached(want: &[u8], cached: bool, when: &str) -> Result<(), Failure> {
    let (_, is) = open_and_check_cached(want)?;
    check(is == cached, || {
        let not = if cached { " not" } else { "" };
        format!("a read-only description was{not} cached {when}")
    })
}

/// `pcache-redoxfs-mmap`: a file with a shared writable mapping is not cached while the
/// mapping lasts, whatever happens to the descriptor it was made through or to read-only
/// mappings beside it, and is cached again once it is unmapped; a reader cached before the
/// mapping sees what it writes back. A private writable mapping changes neither the file nor
/// its caching.
pub fn mappings() -> Result<(), Failure> {
    let mut want = bytes(6, 0, LEN);
    io("create the test file", fs::write(PATH, &want))?;
    let writer = open_fs(true)?;
    // The check itself: RedoxFS serves the O_FSYNC description.
    expect(&writer, &want, "creating the file")?;
    check(!served_from_cache(&writer, 2)?, || {
        "the access time did not move on a read RedoxFS served".into()
    })?;
    let (reader, cached) = open_and_check_cached(&want)?;
    check(cached, || "a read-only description was not cached".into())?;

    // A private writable mapping: the kernel copies the page on the store, so neither
    // fsync nor munmap changes the file, and the file stays cached.
    let private = map_page(&writer, Map::PrivateWrite)?;
    // SAFETY: `private` maps a whole page, writable, and nothing else uses it.
    unsafe { private.add(7).write(!want[7]) };
    io("fsync", writer.sync_all())?;
    expect(
        &writer,
        &want,
        "a store through a private mapping and fsync",
    )?;
    expect(
        &reader,
        &want,
        "a store through a private mapping and fsync",
    )?;
    expect_cached(&want, true, "with a private writable mapping")?;
    unmap_page(private)?;
    expect(&writer, &want, "unmapping a private mapping")?;

    // A read-only mapping, then a writable one beside it: the file leaves the cache with
    // the writable one and comes back when it goes, though the read-only one stays.
    let read_only = map_page(&writer, Map::SharedRead)?;
    expect_cached(&want, true, "with a read-only mapping")?;
    let writable = map_page(&writer, Map::SharedWrite)?;
    expect_cached(&want, false, "with a shared writable mapping")?;
    unmap_page(writable)?;
    expect_cached(
        &want,
        true,
        "after its writable mapping went, a read-only one left",
    )?;
    unmap_page(read_only)?;

    // A shared writable mapping whose descriptor closes first: the store reaches the file,
    // the reader cached before it sees it, and the file is cached again only after munmap.
    let (reader2, cached) = open_and_check_cached(&want)?;
    check(cached, || "a read-only description was not cached".into())?;
    let shared = map_page(&writer, Map::SharedWrite)?;
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
    expect(
        &reader2,
        &want,
        "a store through a shared mapping and fsync",
    )?;
    expect_cached(
        &want,
        false,
        "with a shared writable mapping, its descriptor closed",
    )?;
    unmap_page(shared)?;
    expect_cached(&want, true, "after unmapping, the descriptor closed first")?;
    drop((reader, reader2));
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
/// file through a read-only description (served from the cache) against an `O_FSYNC` one
/// (served by RedoxFS, as with the option off), in the same boot, with the paired A/B rule
/// of `ferric_blkqueue::ab`. Fails when the cache is slower.
pub fn bench() -> Result<(), Failure> {
    io(
        "create the bench file",
        fs::write(PATH, bytes(4, 0, BENCH_LEN)),
    )?;
    let scheme = open_fs(false)?;
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

/// `pcache-redoxfs-writeback`: data written through a plain description survives write,
/// reread, `fsync`, close and reopen, whether or not the write was cached; prints whether it
/// was (`cached=yes` with `kernel.page_cache_writeback`). A cached write is seen at once by
/// a cached reader and by one RedoxFS serves (which writes the dirty pages back first), and
/// `fsync` writes it to the file. A write left dirty at the last close reaches the file too.
pub fn writeback() -> Result<(), Failure> {
    let mut want = bytes(7, 0, LEN);
    io("create the test file", fs::write(PATH, &want))?;
    let writer = open_writer()?;
    let reader = io("open the test file", File::open(PATH))?;
    expect(&reader, &want, "creating the file")?;
    // Partial pages at both ends: across pages 1 and 2.
    let at = 2 * 4096 - 700;
    let patch = bytes(8, at, 1500);
    let cached = write_cached(&writer, &patch, at)?;
    want[at..at + patch.len()].copy_from_slice(&patch);
    let yes = if cached { "yes" } else { "no" };
    println!("pcache-redoxfs-writeback cached={yes}");
    // Whole pages 4 to 6, and the end of the partial last page.
    let whole = bytes(9, 4 * 4096, 3 * 4096);
    write_cached(&writer, &whole, 4 * 4096)?;
    want[4 * 4096..7 * 4096].copy_from_slice(&whole);
    let tail = bytes(10, LEN - 7, 7);
    write_cached(&writer, &tail, LEN - 7)?;
    want[LEN - 7..].copy_from_slice(&tail);
    expect(&reader, &want, "writes, before fsync")?;
    check(!cached || unwritten(&writer)?, || {
        "a read from the cache wrote cached writes back".into()
    })?;
    io("fsync", writer.sync_all())?;
    check(!unwritten(&writer)?, || {
        "fsync did not write the cached writes to the file".into()
    })?;
    expect(&open_fs(false)?, &want, "fsync, through RedoxFS")?;
    expect(&reader, &want, "fsync, from the cache")?;
    drop((writer, reader));
    // Every description closed. O_FSYNC bypasses the retained object and reads RedoxFS.
    expect(&open_fs(false)?, &want, "closing and reopening")?;

    // A write left dirty at the last close.
    let writer = open_writer()?;
    let patch = bytes(11, 100, 200);
    write_cached(&writer, &patch, 100)?;
    want[100..300].copy_from_slice(&patch);
    drop(writer);
    expect(&open_fs(false)?, &want, "a write and the last close")?;
    // A dirty page a reader RedoxFS serves asks for: RedoxFS writes it back first.
    let writer = open_writer()?;
    let patch = bytes(12, 3 * 4096, 4096);
    write_cached(&writer, &patch, 3 * 4096)?;
    want[3 * 4096..4 * 4096].copy_from_slice(&patch);
    expect(&open_fs(false)?, &want, "a write, read through RedoxFS")?;
    drop(writer);
    io("remove the test file", fs::remove_file(PATH))
}

/// How long [`age`] waits for a dirty page to be written back on its own: the writeback age
/// (`ferric_pcache::writeback::DEFAULT_AGE_MS`), RedoxFS's 1 s flush timer, and slack.
const AGE_WAIT: Duration = Duration::from_millis(ferric_pcache::writeback::DEFAULT_AGE_MS + 4000);

/// `pcache-redoxfs-writeback-age`: a cached write nobody syncs is written back on its own
/// after the writeback age, with its description still open. Passes at once when writes are
/// not cached.
pub fn age() -> Result<(), Failure> {
    let mut want = bytes(13, 0, LEN);
    io("create the test file", fs::write(PATH, &want))?;
    let writer = open_writer()?;
    let patch = bytes(14, 5000, 3000);
    let start = Instant::now();
    if write_cached(&writer, &patch, 5000)? {
        want[5000..8000].copy_from_slice(&patch);
        while unwritten(&writer)? {
            check(start.elapsed() < AGE_WAIT, || {
                format!("a cached write was not written back within {AGE_WAIT:?}")
            })?;
            std::thread::sleep(Duration::from_millis(100));
        }
        println!(
            "pcache-redoxfs-writeback-age written back after {} ms",
            start.elapsed().as_millis()
        );
        // Still open, so the cache still has the file: RedoxFS reads what it wrote back.
        expect(&open_fs(false)?, &want, "writeback on its own")?;
    }
    drop(writer);
    io("remove the test file", fs::remove_file(PATH))
}

/// `pcache-redoxfs-writeback-trunc`: opening a file `O_TRUNC` while another description
/// has cached writes in it truncates the file to nothing and keeps every write made after:
/// RedoxFS writes the cached writes back and holds the file's writes during the truncation
/// (as for `ftruncate`), so none is dropped by it or slips in between. Afterwards the
/// writer's writes are cached again: one that grows the file (which RedoxFS always serves)
/// leaves zeros before it, and one inside the new size is cached, and both reach the file
/// on `fsync`. Passes either way when writes are not cached.
pub fn writeback_truncate() -> Result<(), Failure> {
    io("create the test file", fs::write(PATH, bytes(15, 0, LEN)))?;
    let writer = open_writer()?;
    let reader = io("open the test file", File::open(PATH))?;
    // Inside the file, so cached when writes are: what the O_TRUNC open must write back.
    let cached = write_cached(&writer, &bytes(16, 4096, 2 * 4096), 4096)?;
    let truncating = io(
        "open the test file O_TRUNC",
        OpenOptions::new().write(true).truncate(true).open(PATH),
    )?;
    expect(&reader, &[], "an O_TRUNC open, from the cache")?;
    expect(&open_fs(false)?, &[], "an O_TRUNC open, through RedoxFS")?;
    // A write that grows the file again, through the first writer: zeros before it.
    let at = 2 * 4096 + 100;
    let patch = bytes(17, at, 3 * 4096);
    write_cached(&writer, &patch, at)?;
    let mut want = vec![0; at];
    want.extend_from_slice(&patch);
    // Inside the new size, across pages 0 to 2: cached again once the hold is gone.
    let inside = bytes(18, 1000, 2 * 4096);
    let cached_after = write_cached(&writer, &inside, 1000)?;
    want[1000..1000 + inside.len()].copy_from_slice(&inside);
    check(cached_after == cached, || {
        format!(
            "a write inside the file was cached={cached} before the O_TRUNC open and cached={cached_after} after"
        )
    })?;
    expect(
        &reader,
        &want,
        "writes after the O_TRUNC open, from the cache",
    )?;
    io("fsync", writer.sync_all())?;
    check(!unwritten(&writer)?, || {
        "fsync did not write the cached write after the O_TRUNC open".into()
    })?;
    expect(&open_fs(false)?, &want, "fsync after the O_TRUNC open")?;
    drop((writer, reader, truncating));
    expect(
        &open_fs(false)?,
        &want,
        "closing and reopening after the O_TRUNC open",
    )?;
    io("remove the test file", fs::remove_file(PATH))
}

/// Reads the page `page` maps.
///
/// # Safety
///
/// `page` must map a whole readable page that stays mapped for the call.
unsafe fn mapped(page: *const u8) -> Vec<u8> {
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(page, 4096) }.to_vec()
}

/// `pcache-redoxfs-writeback-mmap`: a mapping made over dirty pages sees them (RedoxFS
/// writes them back before mapping), a write made while a shared writable mapping lasts goes
/// to RedoxFS, and once it is unmapped, writes through a description opened afterwards are
/// cached again (the mapping closed the file's object, as on the read path, so descriptions
/// open across it stay with RedoxFS). The write made while mapped is to another page than
/// the mapped one: how RedoxFS's own mappings see writes to their pages is not the page
/// cache's to test. Passes at once when writes are not cached.
pub fn writeback_mappings() -> Result<(), Failure> {
    let mut want = bytes(15, 0, LEN);
    io("create the test file", fs::write(PATH, &want))?;
    let file = io(
        "open the test file read-write",
        OpenOptions::new().read(true).write(true).open(PATH),
    )?;
    let patch = bytes(16, 10, 20);
    if write_cached(&file, &patch, 10)? {
        want[10..30].copy_from_slice(&patch);
        let page = map_page(&file, Map::SharedWrite)?;
        // SAFETY: `page` maps the file's first page until the munmap below.
        let seen = unsafe { mapped(page) };
        let at = 3 * 4096 + 40;
        let patch = bytes(17, at, 10);
        let cached_while_mapped = write_cached(&file, &patch, at);
        want[at..at + 10].copy_from_slice(&patch);
        unmap_page(page)?;
        check(seen[..] == want[..4096], || {
            "a mapping made over a cached write did not see it".into()
        })?;
        check(!cached_while_mapped?, || {
            "a write was cached while the file had a shared writable mapping".into()
        })?;
        let patch = bytes(18, 60, 10);
        let writer = open_writer()?;
        check(write_cached(&writer, &patch, 60)?, || {
            "writes were not cached again after the mapping went".into()
        })?;
        want[60..70].copy_from_slice(&patch);
        expect(&file, &want, "writes around a mapping")?;
        io("fsync", writer.sync_all())?;
        drop(writer);
        expect(&open_fs(false)?, &want, "writes around a mapping and fsync")?;
    }
    drop(file);
    io("remove the test file", fs::remove_file(PATH))
}
