//! The test on Redox: the file system scheme, the fill loop the main thread runs, and the
//! steps.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU16;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ferric_pcache::abi::{
    ObjectId, Outcome, PAGE_SIZE, QUEUE_PATH, ReadaheadMax, Record, RecordPages, Request,
    SCHEME_NAME,
};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_RDWR};
use redox_scheme::scheme::{SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{CallerCtx, OpenResult, RequestKind, SignalBehavior, Socket};
use syscall::CallFlags;
use syscall::error::{EBADF, ENOENT, Error, Result as SysResult};
use syscall::schemev2::NewFdFlags;

/// The test file system's scheme name.
const SCHEME: &str = "pcache-test";
/// The one file it holds.
const FILE_PATH: &str = "file";
/// The file's size: 64 pages and 100 bytes, so the last page is partly used.
const FILE_SIZE: u64 = 64 * PAGE_SIZE + 100;
/// The file's object ID on the queue.
const OBJECT: ObjectId = ObjectId::new(0x70c);
/// The readahead maximum the test sets, in pages, so readahead stays near the pages read.
const READAHEAD_MAX: u32 = 8;
/// The scheme root's handle number.
const ROOT: usize = 0;
/// relibc's EIO.
const ERRNO_EIO: i32 = 5;
/// How long a step waits for a record or a read.
const WAIT: Duration = Duration::from_secs(5);
/// How long the queue stays empty before the records of one read are taken to be all there.
const QUIET: Duration = Duration::from_millis(100);
/// `pread`s timed for the benchmark line, each way.
const BENCH_READS: u32 = 2000;

/// The byte at `offset` of the file.
fn byte_at(offset: u64) -> u8 {
    ((offset / PAGE_SIZE).wrapping_mul(7) ^ (offset % 251)) as u8
}

/// The file's bytes from `offset`, at most `len` of them.
fn file_bytes(offset: u64, len: usize) -> Vec<u8> {
    let end = offset.saturating_add(len as u64).min(FILE_SIZE);
    (offset..end.max(offset)).map(byte_at).collect()
}

/// Why a step failed.
enum Failure {
    /// A call failed.
    Errno { what: &'static str, errno: i32 },
    /// Something was not as expected.
    Check(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::Check(why) => f.write_str(why),
        }
    }
}

/// The result of a call, with what it was for when it failed.
fn ctx<T>(what: &'static str, result: libredox::error::Result<T>) -> Result<T, Failure> {
    result.map_err(|err| Failure::Errno {
        what,
        errno: err.errno(),
    })
}

/// Fails with `why` unless `ok`.
fn check(ok: bool, why: impl FnOnce() -> String) -> Result<(), Failure> {
    if ok {
        Ok(())
    } else {
        Err(Failure::Check(why()))
    }
}

/// The file system scheme: it opens the file, attaching each description to the object, and
/// serves reads that reach it (counted) from the same bytes.
struct FileScheme {
    /// The object's descriptor, which `pcache-generation` replaces.
    object: Arc<AtomicUsize>,
    next: usize,
    reads: Arc<AtomicUsize>,
}

impl SchemeSync for FileScheme {
    fn scheme_root(&mut self) -> SysResult<usize> {
        Ok(ROOT)
    }

    fn openat(
        &mut self,
        fd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> SysResult<OpenResult> {
        if fd != ROOT || path.trim_matches('/') != FILE_PATH {
            return Err(Error::new(ENOENT));
        }
        self.next += 1;
        let number = self.next;
        let (words, n) = Request::Attach {
            handle: number as u64,
        }
        .encode();
        let object = self.object.load(Ordering::Relaxed);
        libredox::call::call_wo(object, &[], CallFlags::empty(), &words[..n])
            .map_err(|err| Error::new(err.errno()))?;
        // Positioned, so the kernel passes every read's offset.
        Ok(OpenResult::ThisScheme {
            number,
            flags: NewFdFlags::POSITIONED,
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> SysResult<usize> {
        if id == ROOT {
            return Err(Error::new(EBADF));
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let bytes = file_bytes(offset, buf.len());
        buf[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn fsize(&mut self, _id: usize, _ctx: &CallerCtx) -> SysResult<u64> {
        Ok(FILE_SIZE)
    }
}

/// Serves the scheme until the process exits.
fn serve(socket: Socket, mut scheme: FileScheme) {
    let mut state = SchemeState::new();
    loop {
        let request = match socket.next_request(SignalBehavior::Restart) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(err) => {
                eprintln!("pcache-test scheme: {err}");
                return;
            }
        };
        let response = match request.kind() {
            RequestKind::Call(call) => call.handle_sync(&mut scheme, &mut state),
            _ => continue,
        };
        if let Err(err) = socket.write_response(response, SignalBehavior::Restart) {
            eprintln!("pcache-test scheme: {err}");
            return;
        }
    }
}

/// The cache side of the test: the queue (until the close step) and the object.
struct Cache {
    queue: Option<Fd>,
    object: Fd,
    /// The descriptor the scheme attaches to, kept equal to `object`'s.
    scheme_object: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    hit_ns: u128,
}

impl Cache {
    fn queue(&self) -> Result<&Fd, Failure> {
        self.queue
            .as_ref()
            .ok_or(Failure::Check("the queue is closed".into()))
    }

    /// Waits up to [`WAIT`] for records, then takes all that arrive until the queue has been
    /// empty for [`QUIET`].
    fn records(&self) -> Result<Vec<Record>, Failure> {
        let queue = self.queue()?;
        progress("read the queue");
        let mut records = Vec::new();
        let start = Instant::now();
        let mut last = start;
        let mut buf = [0_u8; Record::BYTES * 64];
        loop {
            match queue.read(&mut buf) {
                Ok(n) => {
                    for chunk in buf[..n].chunks_exact(Record::BYTES) {
                        let record = Record::from_bytes(chunk).map_err(|err| {
                            Failure::Check(format!("the queue gave a bad record: {err}"))
                        })?;
                        records.push(record);
                    }
                    last = Instant::now();
                }
                Err(err) if err.is_wouldblock() => {
                    let waited = if records.is_empty() { WAIT } else { QUIET };
                    if last.elapsed() >= waited {
                        return Ok(records);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(err) => {
                    return Err(Failure::Errno {
                        what: "reading the queue",
                        errno: err.errno(),
                    });
                }
            }
        }
    }

    /// Answers a fill record: with the pages' bytes, or failed with `EIO`.
    fn answer(&self, record: Record, ok: bool) -> Result<(), Failure> {
        let Record::Fill {
            object,
            generation,
            pages,
        } = record
        else {
            return Err(Failure::Check(format!("unexpected record {record:?}")));
        };
        check(object == OBJECT, || {
            format!("a record for object {object:?}")
        })?;
        let (payload, outcome) = if ok {
            (page_bytes(pages), Outcome::Done)
        } else {
            let eio = NonZeroU16::new(ERRNO_EIO as u16).expect("EIO is not zero");
            (Vec::new(), Outcome::Failed(eio))
        };
        progress(&format!("Filled {:?}", pages.pages()));
        let (words, n) = Request::Filled {
            pages,
            outcome,
            generation,
        }
        .encode();
        ctx(
            "Filled",
            self.object
                .call_wo(&payload, CallFlags::empty(), &words[..n]),
        )?;
        Ok(())
    }

    /// Answers every record in `records` with the pages' bytes.
    fn fill_all(&self, records: &[Record]) -> Result<(), Failure> {
        records
            .iter()
            .try_for_each(|&record| self.answer(record, true))
    }
}

/// The bytes of whole pages `pages`, zero past the end of the file.
fn page_bytes(pages: RecordPages) -> Vec<u8> {
    let range = pages.pages();
    let len = range.count() as usize * PAGE_SIZE as usize;
    let mut bytes = file_bytes(range.first() * PAGE_SIZE, len);
    bytes.resize(len, 0);
    bytes
}

/// The pages `records` fill.
fn pages_of(records: &[Record]) -> BTreeSet<u64> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::Fill { pages, .. } => Some(pages.pages()),
            _ => None,
        })
        .flat_map(|range| range.first()..range.end())
        .collect()
}

/// A `pread` of `len` bytes at `offset` of descriptor `fd`, in its own thread.
fn spawn_pread(fd: usize, offset: u64, len: usize) -> mpsc::Receiver<Result<Vec<u8>, i32>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(pread(fd, offset, len));
    });
    receive
}

/// A `pread` of `len` bytes at `offset` of descriptor `fd`.
fn pread(fd: usize, offset: u64, len: usize) -> Result<Vec<u8>, i32> {
    let mut buf = vec![0_u8; len];
    // SAFETY: `buf` has room for `len` bytes.
    let n = unsafe {
        libc::pread(
            fd as libc::c_int,
            buf.as_mut_ptr().cast(),
            len,
            offset as libc::off_t,
        )
    };
    match usize::try_from(n) {
        Ok(n) => {
            buf.truncate(n);
            Ok(buf)
        }
        Err(_) => Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(ERRNO_EIO)),
    }
}

/// The outcome of a threaded `pread`, within [`WAIT`].
fn finished(read: &mpsc::Receiver<Result<Vec<u8>, i32>>) -> Result<Result<Vec<u8>, i32>, Failure> {
    read.recv_timeout(WAIT)
        .map_err(|_| Failure::Check(format!("a read did not finish within {WAIT:?}")))
}

/// Whether a threaded `pread` has finished already (without anybody answering a fill).
fn finished_now(
    read: &mpsc::Receiver<Result<Vec<u8>, i32>>,
) -> Result<Result<Vec<u8>, i32>, Failure> {
    read.recv_timeout(QUIET * 10)
        .map_err(|_| Failure::Check("a read of cached pages waited".into()))
}

/// Checks a read returned the file's bytes at `offset`.
fn check_bytes(read: Result<Vec<u8>, i32>, offset: u64, len: usize) -> Result<(), Failure> {
    let bytes = read.map_err(|errno| Failure::Errno {
        what: "pread",
        errno,
    })?;
    check(bytes == file_bytes(offset, len), || {
        format!(
            "pread at {offset} returned {} bytes that are not the file's",
            bytes.len()
        )
    })
}

/// Opens the file on the test scheme.
fn open_file() -> Result<Fd, Failure> {
    ctx(
        "open the test file",
        Fd::open(
            &format!("/scheme/{SCHEME}/{FILE_PATH}"),
            O_RDONLY | O_CLOEXEC,
            0,
        ),
    )
}

/// Mean nanoseconds of [`BENCH_READS`] 4 KiB `pread`s at offset 0 of `fd`.
fn time_preads(fd: &Fd) -> Result<u128, Failure> {
    let start = Instant::now();
    for _ in 0..BENCH_READS {
        check_bytes(
            pread(fd.raw(), 0, PAGE_SIZE as usize),
            0,
            PAGE_SIZE as usize,
        )?;
    }
    Ok(start.elapsed().as_nanos() / u128::from(BENCH_READS))
}

/// The queue, bound to the scheme, and the file's object, sized; `None` without a
/// `kernel.pcache` scheme.
fn setup(socket: &Socket) -> Result<Option<(Fd, Fd)>, Failure> {
    progress("open the queue");
    let queue = match Fd::open(
        &format!("/scheme/{SCHEME_NAME}/{QUEUE_PATH}"),
        O_RDWR | O_CLOEXEC | O_NONBLOCK,
        0,
    ) {
        Ok(queue) => queue,
        Err(err) if err.errno() == ENOENT || err.errno() == syscall::error::ENODEV => {
            return Ok(None);
        }
        Err(err) => {
            return Err(Failure::Errno {
                what: "open the queue",
                errno: err.errno(),
            });
        }
    };
    progress("Bind");
    let (words, n) = Request::Bind.encode();
    ctx(
        "Bind",
        libredox::call::call_wo(
            queue.raw(),
            &socket.inner().raw().to_ne_bytes(),
            CallFlags::FD | CallFlags::FD_CLONE,
            &words[..n],
        ),
    )?;
    // Binding twice is refused.
    let again = libredox::call::call_wo(
        queue.raw(),
        &socket.inner().raw().to_ne_bytes(),
        CallFlags::FD | CallFlags::FD_CLONE,
        &words[..n],
    );
    check(again.is_err(), || "a second Bind worked".into())?;
    progress("SetReadaheadMax");
    let max = ReadaheadMax::new(READAHEAD_MAX).expect("in range");
    let (words, n) = Request::SetReadaheadMax { pages: max }.encode();
    ctx(
        "SetReadaheadMax",
        queue.call_wo(&[], CallFlags::empty(), &words[..n]),
    )?;
    progress("create the object");
    let mut path = [0; ObjectId::PATH_MAX];
    let object = ctx(
        "create the object",
        queue.openat(OBJECT.encode_path(&mut path), O_RDWR | O_CLOEXEC, 0),
    )?;
    let duplicate = queue.openat(OBJECT.encode_path(&mut path), O_RDWR | O_CLOEXEC, 0);
    check(duplicate.is_err(), || {
        "a second object with the same ID".into()
    })?;
    progress("SetSize");
    let (words, n) = Request::SetSize { size: FILE_SIZE }.encode();
    ctx(
        "SetSize",
        object.call_wo(&[], CallFlags::empty(), &words[..n]),
    )?;
    Ok(Some((queue, object)))
}

/// `pcache-miss`.
fn miss(cache: &mut Cache, file: &Fd) -> Result<(), Failure> {
    let read = spawn_pread(file.raw(), 0, 2 * PAGE_SIZE as usize);
    let records = cache.records()?;
    let pages = pages_of(&records);
    check(pages.contains(&0) && pages.contains(&1), || {
        format!("the fills of a read of pages 0 and 1 were {pages:?}")
    })?;
    cache.fill_all(&records)?;
    check_bytes(finished(&read)?, 0, 2 * PAGE_SIZE as usize)?;
    let reads = cache.reads.load(Ordering::Relaxed);
    check(reads == 0, || {
        format!("the scheme was asked for {reads} read(s)")
    })
}

/// `pcache-hit`.
fn hit(cache: &mut Cache, file: &Fd) -> Result<(), Failure> {
    let read = spawn_pread(file.raw(), 100, 2 * PAGE_SIZE as usize - 200);
    check_bytes(finished_now(&read)?, 100, 2 * PAGE_SIZE as usize - 200)?;
    // A sequential read reads ahead further; answer that, then time cached reads.
    let ahead = cache.records()?;
    check(!pages_of(&ahead).contains(&0), || {
        "page 0 was filled again".into()
    })?;
    cache.fill_all(&ahead)?;
    cache.hit_ns = time_preads(file)?;
    let more = cache.records()?;
    cache.fill_all(&more)?;
    let reads = cache.reads.load(Ordering::Relaxed);
    check(reads == 0, || {
        format!("the scheme was asked for {reads} read(s)")
    })
}

/// `pcache-readahead`.
fn readahead(cache: &mut Cache) -> Result<(), Failure> {
    let file = open_file()?;
    let lone = spawn_pread(file.raw(), 20 * PAGE_SIZE, 10);
    let records = cache.records()?;
    let pages = pages_of(&records);
    check(pages == BTreeSet::from([20]), || {
        format!("a lone read of page 20 filled {pages:?}")
    })?;
    cache.fill_all(&records)?;
    check_bytes(finished(&lone)?, 20 * PAGE_SIZE, 10)?;
    let next = spawn_pread(file.raw(), 21 * PAGE_SIZE, PAGE_SIZE as usize);
    let records = cache.records()?;
    let pages = pages_of(&records);
    check(pages.contains(&21) && pages.contains(&22), || {
        format!("the sequential read of page 21 filled {pages:?}, no readahead")
    })?;
    cache.fill_all(&records)?;
    check_bytes(finished(&next)?, 21 * PAGE_SIZE, PAGE_SIZE as usize)?;
    let ahead = spawn_pread(file.raw(), 22 * PAGE_SIZE, PAGE_SIZE as usize);
    check_bytes(finished_now(&ahead)?, 22 * PAGE_SIZE, PAGE_SIZE as usize)?;
    let rest = cache.records()?;
    cache.fill_all(&rest)
}

/// `pcache-eio`.
fn eio(cache: &mut Cache) -> Result<(), Failure> {
    let file = open_file()?;
    let read = spawn_pread(file.raw(), 40 * PAGE_SIZE, 64);
    let records = cache.records()?;
    check(pages_of(&records) == BTreeSet::from([40]), || {
        format!("a read of page 40 filled {:?}", pages_of(&records))
    })?;
    for &record in &records {
        cache.answer(record, false)?;
    }
    let failed = finished(&read)?;
    check(failed == Err(ERRNO_EIO), || {
        format!(
            "a read whose fill failed returned {:?}",
            failed.map(|b| b.len())
        )
    })?;
    let again = spawn_pread(file.raw(), 40 * PAGE_SIZE, 64);
    let records = cache.records()?;
    check(pages_of(&records).contains(&40), || {
        "page 40 was not filled again".into()
    })?;
    cache.fill_all(&records)?;
    check_bytes(finished(&again)?, 40 * PAGE_SIZE, 64)
}

/// The page `pcache-restore` reads; no earlier step reads it.
const RESTORE_PAGE: u64 = 60;
/// The pages `pcache-generation` reads; no earlier step reads them.
const GENERATION_PAGES: [u64; 2] = [50, 52];
/// relibc's EFAULT, EPERM and ESTALE.
const ERRNO_EFAULT: i32 = 14;
const ERRNO_EPERM: i32 = 1;
const ERRNO_ESTALE: i32 = 116;

/// `pcache-restore`: a queue read that faults copying records out puts them back, so the
/// fill is still there for the next read.
fn restore(cache: &mut Cache) -> Result<(), Failure> {
    let file = open_file()?;
    let read = spawn_pread(file.raw(), RESTORE_PAGE * PAGE_SIZE, 64);
    // Wait until the record is queued, without taking it: the reader is waiting by then.
    thread::sleep(QUIET * 5);
    let queue = cache.queue()?.raw();
    // An address in user space with nothing mapped: the copy out faults.
    // SAFETY: an anonymous private mapping of one page, unmapped again at once; nothing
    // else uses the address.
    let hole = unsafe {
        let page = libc::mmap(
            core::ptr::null_mut(),
            PAGE_SIZE as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        check(page != libc::MAP_FAILED, || "mmap failed".into())?;
        libc::munmap(page, PAGE_SIZE as usize);
        page
    };
    // SAFETY: the kernel checks the buffer; `hole` is not dereferenced here.
    let n = unsafe { libc::read(queue as libc::c_int, hole, Record::BYTES) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    check(n < 0 && errno == Some(ERRNO_EFAULT), || {
        format!("a queue read into an unmapped buffer returned {n} (errno {errno:?})")
    })?;
    let records = cache.records()?;
    check(pages_of(&records).contains(&RESTORE_PAGE), || {
        format!(
            "the record a faulting read took was lost: the queue gave {:?}",
            pages_of(&records)
        )
    })?;
    cache.fill_all(&records)?;
    check_bytes(finished(&read)?, RESTORE_PAGE * PAGE_SIZE, 64)
}

/// `pcache-owner`: the object's descriptor, inherited by a child process, is refused there,
/// so another process cannot attach a client's description to pages it fills.
fn owner(cache: &mut Cache) -> Result<(), Failure> {
    let object = cache.object.raw();
    let (words, n) = Request::Attach { handle: 4242 }.encode();
    // SAFETY: the child makes one call and exits without returning to Rust code that would
    // touch the parent's threads or locks.
    let child = unsafe { libc::fork() };
    if child == 0 {
        let code = match libredox::call::call_wo(object, &[], CallFlags::empty(), &words[..n]) {
            Err(err) if err.errno() == ERRNO_EPERM => 0,
            Err(_) => 2,
            Ok(_) => 1,
        };
        // SAFETY: ends the child at once.
        unsafe { libc::_exit(code) };
    }
    check(child > 0, || "fork failed".into())?;
    let mut status = 0;
    // SAFETY: `status` is a valid place for the child's status.
    let waited = unsafe { libc::waitpid(child, &mut status, 0) };
    check(waited == child && libc::WIFEXITED(status), || {
        "the child did not exit".into()
    })?;
    match libc::WEXITSTATUS(status) {
        0 => Ok(()),
        1 => Err(Failure::Check(
            "another process attached a description with the object's descriptor".into(),
        )),
        code => Err(Failure::Check(format!(
            "another process's Attach failed with an error other than EPERM ({code})"
        ))),
    }
}

/// `pcache-generation`: closing an object drops its records still on the queue, and a
/// record already taken cannot fill a new object with the same ID.
fn generation(cache: &mut Cache) -> Result<(), Failure> {
    let [taken, left] = GENERATION_PAGES;
    let file = open_file()?;
    let first = spawn_pread(file.raw(), taken * PAGE_SIZE, 64);
    let old = cache.records()?;
    check(pages_of(&old) == BTreeSet::from([taken]), || {
        format!("a read of page {taken} filled {:?}", pages_of(&old))
    })?;
    // A second fill stays on the queue, not taken.
    let second = spawn_pread(file.raw(), left * PAGE_SIZE, 64);
    thread::sleep(QUIET * 5);
    // A new object with the same ID replaces the old one, whose readers fail with EIO.
    replace_object(cache)?;
    for read in [&first, &second] {
        let got = finished(read)?;
        check(got == Err(ERRNO_EIO), || {
            "a reader of a closed object did not get EIO".into()
        })?;
    }
    // A description opened now reads the new object. The queue holds only its record: the
    // closed object's fill of page `left` went with it.
    let file = open_file()?;
    let read = spawn_pread(file.raw(), taken * PAGE_SIZE, 64);
    let new = cache.records()?;
    check(pages_of(&new) == BTreeSet::from([taken]), || {
        format!(
            "after the object was replaced the queue gave {:?}",
            pages_of(&new)
        )
    })?;
    check(
        new.iter()
            .all(|record| record.generation() != old[0].generation()),
        || "the new object has the old one's generation".into(),
    )?;
    // The old record's answer, on the new object, is refused.
    match cache.answer(old[0], true) {
        Err(Failure::Errno { errno, .. }) if errno == ERRNO_ESTALE => {}
        Err(failure) => return Err(failure),
        Ok(()) => {
            return Err(Failure::Check(
                "an answer to a closed object's record filled the new object".into(),
            ));
        }
    }
    cache.fill_all(&new)?;
    check_bytes(finished(&read)?, taken * PAGE_SIZE, 64)
}

/// Closes the object and creates a new one with the same ID and size, which the scheme
/// attaches its descriptions to from now on.
fn replace_object(cache: &mut Cache) -> Result<(), Failure> {
    // A placeholder that is never used or closed, so the old object closes before the new
    // one opens (an ID in use cannot be opened again).
    let old = std::mem::replace(&mut cache.object, Fd::new(usize::MAX));
    drop(old);
    let mut path = [0; ObjectId::PATH_MAX];
    let object = ctx(
        "create the replacement object",
        cache
            .queue()?
            .openat(OBJECT.encode_path(&mut path), O_RDWR | O_CLOEXEC, 0),
    )?;
    let (words, n) = Request::SetSize { size: FILE_SIZE }.encode();
    ctx(
        "SetSize",
        object.call_wo(&[], CallFlags::empty(), &words[..n]),
    )?;
    cache.scheme_object.store(object.raw(), Ordering::Relaxed);
    std::mem::forget(std::mem::replace(&mut cache.object, object));
    Ok(())
}

/// `pcache-close`, which also times reads through the scheme.
fn close(cache: &mut Cache) -> Result<(), Failure> {
    let file = open_file()?;
    let read = spawn_pread(file.raw(), 56 * PAGE_SIZE, 64);
    let records = cache.records()?;
    check(pages_of(&records).contains(&56), || {
        format!("a read of page 56 filled {:?}", pages_of(&records))
    })?;
    // Nobody answers: closing the queue must release the reader.
    drop(cache.queue.take());
    let released = finished(&read)?;
    check(released == Err(ERRNO_EIO), || {
        format!(
            "a read waiting when the queue closed returned {:?}",
            released.map(|b| b.len())
        )
    })?;
    let before = cache.reads.load(Ordering::Relaxed);
    check_bytes(
        pread(file.raw(), 3 * PAGE_SIZE + 7, 100),
        3 * PAGE_SIZE + 7,
        100,
    )?;
    let after = cache.reads.load(Ordering::Relaxed);
    check(after == before + 1, || {
        "a read after the queue closed did not go to the scheme".into()
    })?;
    let scheme_ns = time_preads(&file)?;
    println!(
        "FERRIC_BENCH pcache-pread4k reads={BENCH_READS} hit_ns={} scheme_ns={scheme_ns}",
        cache.hit_ns
    );
    Ok(())
}

/// How long the whole test may take before the watchdog fails it.
const WATCHDOG: Duration = Duration::from_secs(90);

/// Prints where the test is, so a hang in the kernel shows the call it is stuck in.
fn progress(what: &str) {
    println!("pcache-test: {what}");
}

/// Fails the run and exits if it is still going after [`WATCHDOG`], so a stuck call does
/// not stall the rest of the smoke script.
fn start_watchdog() {
    thread::spawn(|| {
        thread::sleep(WATCHDOG);
        println!("FERRIC_TEST_FAIL pcache-watchdog: still running after {WATCHDOG:?}");
        std::process::exit(1);
    });
}

/// Runs the test.
pub fn main() -> ExitCode {
    start_watchdog();
    match run() {
        Ok(code) => code,
        Err((step, failure)) => {
            println!("FERRIC_TEST_FAIL {step}: {failure}");
            ExitCode::FAILURE
        }
    }
}

/// A step on the test's own scheme.
type CacheStep<'a> = dyn Fn(&mut Cache) -> Result<(), Failure> + 'a;

fn run() -> Result<ExitCode, (&'static str, Failure)> {
    let socket = Socket::create().map_err(|err| {
        (
            "pcache-setup",
            Failure::Errno {
                what: "create the scheme socket",
                errno: err.errno,
            },
        )
    })?;
    let Some((queue, object)) = setup(&socket).map_err(|f| ("pcache-setup", f))? else {
        println!("FERRIC_TEST_SKIP pcache: no kernel.pcache scheme (kernel.page_cache is off)");
        return Ok(ExitCode::SUCCESS);
    };
    let reads = Arc::new(AtomicUsize::new(0));
    let scheme_object = Arc::new(AtomicUsize::new(object.raw()));
    let mut scheme = FileScheme {
        object: Arc::clone(&scheme_object),
        next: ROOT,
        reads: Arc::clone(&reads),
    };
    progress("register the scheme");
    register_sync_scheme(&socket, SCHEME, &mut scheme).map_err(|err| {
        (
            "pcache-setup",
            Failure::Errno {
                what: "register the scheme",
                errno: err.errno,
            },
        )
    })?;
    // The scheme was created on this thread and is served from another, so opening it here
    // is the case where the kernel must not direct-switch a caller to itself (#143).
    thread::spawn(move || serve(socket, scheme));
    progress("open the file");
    let mut cache = Cache {
        queue: Some(queue),
        object,
        scheme_object,
        reads,
        hit_ns: 0,
    };
    let file = open_file().map_err(|f| ("pcache-setup", f))?;
    progress("opened the file");
    let steps: [(&'static str, &CacheStep<'_>); 8] = [
        ("pcache-miss", &|cache| miss(cache, &file)),
        ("pcache-hit", &|cache| hit(cache, &file)),
        ("pcache-readahead", &readahead),
        ("pcache-eio", &eio),
        ("pcache-restore", &restore),
        ("pcache-owner", &owner),
        ("pcache-generation", &generation),
        ("pcache-close", &close),
    ];
    for (name, step) in steps {
        progress(name);
        step(&mut cache).map_err(|failure| (name, failure))?;
        println!("FERRIC_TEST_PASS {name}");
    }
    Ok(ExitCode::SUCCESS)
}
