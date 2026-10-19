//! The steps on RedoxFS itself, which serves reads of files opened read-only from the page
//! cache when the kernel has one: a reader sees writes and truncations made through another
//! description, and the same-boot benchmark of cached reads against reads RedoxFS serves.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
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
    drop((reader, writer));
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
