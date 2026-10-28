//! The request paths a job's submitters read with (`--ioengine`, `--align`; the "Request
//! paths" section of `ferric_iobench::job`): each submitter gets a [`Reader`], set up once per
//! job before any round is timed, and each read is one [`Reader::read`].

use std::ffi::c_int;
use std::fmt;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ferric_blkgrant::abi::{EncodeError, Request as DiskRequest, Slot, Transfer};
use ferric_iobench::job::{Align, Engine, Job};
use ferric_regbuf::abi::{Access, GrantSpec, REGION_PATH, Request as RegbufRequest};
use ferric_ring::abi::{RING_PATH, Request as RingRequest};
use ferric_ring::{Consumer, Cqe, Layout, NotifyMode, Producer, Sqe};
use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_RDWR};
use syscall::CallFlags;
use syscall::error::{EIO, ENODEV, EOPNOTSUPP};

/// The page size buffers are aligned to, and grant offsets are counted in.
const PAGE: usize = 4096;
/// The grant slot of the disk descriptor the registered buffer is attached to.
const GRANT_SLOT: u64 = 0;

/// A kernel or driver feature a request path needs, off by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// The `kernel.regbuf` scheme (`kernel.regbuf`).
    Regbuf,
    /// Grant DMA in nvmed (`drivers.grant_dma`).
    GrantDma,
    /// A DMA domain for nvmed: an IOMMU (`kernel.iommu`), since grants have no
    /// physical-address fallback.
    DmaDomain,
    /// The `kernel.ring` scheme (`kernel.rings`).
    Rings,
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Regbuf => "kernel.regbuf (there is no kernel.regbuf scheme)",
            Self::GrantDma => "drivers.grant_dma (nvmed takes no grants)",
            Self::DmaDomain => "an IOMMU (nvmed has no DMA domain to map the grant into)",
            Self::Rings => "kernel.rings (there is no kernel.ring scheme)",
        })
    }
}

/// Why a request path could not be set up or a read on it failed.
#[derive(Debug)]
pub enum EngineError {
    /// The path needs a feature this system was built without.
    Unavailable {
        /// The path.
        engine: Engine,
        /// What it needs.
        feature: Feature,
        /// The errno that showed it.
        errno: i32,
    },
    /// A call failed with an errno.
    Errno { what: &'static str, errno: i32 },
    /// A read through the grant does not fit the call metadata.
    Encode(EncodeError),
    /// The submitters' buffers do not fit in memory, or a grant range is out of range.
    TooLarge { bytes: u64 },
    /// The ring returned no completion for an entry it took.
    NoCompletion,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable {
                engine,
                feature,
                errno,
            } => write!(
                f,
                "--ioengine={} needs {feature}, which this system was built without \
                 (errno {errno})",
                engine.as_str()
            ),
            Self::Errno { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::Encode(_) => f.write_str("a read through the grant cannot be encoded"),
            Self::TooLarge { bytes } => {
                write!(f, "the submitters' buffers ({bytes} bytes) are too large")
            }
            Self::NoCompletion => f.write_str("the ring posted no completion for a read"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(err) => Some(err),
            _ => None,
        }
    }
}

fn ctx<T>(what: &'static str, r: libredox::error::Result<T>) -> Result<T, EngineError> {
    r.map_err(|e| EngineError::Errno {
        what,
        errno: e.errno(),
    })
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(EIO)
}

/// `bs` bytes on the heap, starting where [`Align`] says.
pub(crate) struct HeapBuffer {
    bytes: Vec<u8>,
    start: usize,
    len: usize,
}

impl HeapBuffer {
    pub(crate) fn new(len: usize, align: Align) -> Self {
        match align {
            Align::Page => {
                // One page more than needed, so a page boundary falls in the first page.
                let bytes = vec![0; len + PAGE];
                let start = bytes.as_ptr().align_offset(PAGE);
                Self { bytes, start, len }
            }
            _ => Self {
                bytes: vec![0; len],
                start: 0,
                len,
            },
        }
    }

    pub(crate) fn get(&mut self) -> &mut [u8] {
        &mut self.bytes[self.start..self.start + self.len]
    }
}

/// An anonymous, page-aligned mapping of private memory, unmapped on drop. This process
/// never reads or writes it (only the controller does, by DMA), so it is kept as an address.
struct Anon {
    addr: usize,
    len: usize,
}

impl Anon {
    fn new(len: usize) -> Result<Self, EngineError> {
        // SAFETY: a fresh anonymous mapping at an address the kernel chooses.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(EngineError::Errno {
                what: "mmap the registered buffer",
                errno: last_errno(),
            });
        }
        Ok(Self {
            addr: ptr as usize,
            len,
        })
    }
}

impl Drop for Anon {
    fn drop(&mut self) {
        // SAFETY: unmaps this mapping, which nothing uses afterwards; the grant onto it was
        // revoked when its region closed, before this runs (`Registered`'s field order).
        unsafe { libc::munmap(self.addr as *mut libc::c_void, self.len) };
    }
}

/// A buffer registered with `kernel.regbuf` and attached as a read-write grant to slot
/// [`GRANT_SLOT`] of the disk descriptor, cut into one page-aligned stride per submitter.
pub struct Registered {
    /// Declared before `buffer`, so it closes first: closing the region revokes the grant and
    /// unpins the buffer, and the driver stops before the buffer is unmapped.
    region: Fd,
    buffer: Anon,
    stride: usize,
}

impl Registered {
    /// Registers a buffer for `submitters` reads of `bs` bytes and attaches it to `disk`.
    ///
    /// # Errors
    ///
    /// [`EngineError::Unavailable`] for a system without `kernel.regbuf`, `drivers.grant_dma`
    /// or an IOMMU, and [`EngineError::Errno`] when a step fails otherwise (registering more
    /// than the pin budget, say).
    fn new(disk: &File, submitters: u32, bs: u32) -> Result<Self, EngineError> {
        let unavailable = |feature, errno| EngineError::Unavailable {
            engine: Engine::Regbuf,
            feature,
            errno,
        };
        let stride = (bs as usize).next_multiple_of(PAGE);
        let len = stride
            .checked_mul(submitters as usize)
            .ok_or(EngineError::TooLarge { bytes: u64::MAX })?;
        let path = format!("/scheme/{}/{REGION_PATH}", ferric_regbuf::abi::SCHEME_NAME);
        let region = Fd::open(&path, O_RDWR | O_CLOEXEC, 0)
            .map_err(|e| unavailable(Feature::Regbuf, e.errno()))?;
        // From here on a failure drops `registered`, which closes the region before it unmaps
        // the buffer.
        let registered = Self {
            region,
            buffer: Anon::new(len)?,
            stride,
        };
        let (words, n) = RegbufRequest::Register {
            addr: registered.buffer.addr as u64,
            len: len as u64,
        }
        .encode();
        ctx(
            "register the buffer",
            registered
                .region
                .call_wo(&[], CallFlags::empty(), &words[..n]),
        )?;
        let spec = GrantSpec::new(Access::ReadWrite, 0, len as u64)
            .map_err(|_| EngineError::TooLarge { bytes: len as u64 })?;
        let mut path = [0; GrantSpec::PATH_MAX];
        let grant = ctx(
            "open a grant of the buffer",
            registered
                .region
                .openat(spec.encode_path(&mut path), O_RDWR | O_CLOEXEC, 0),
        )?;
        // The grant's descriptor moves to the driver, so it must not be closed here.
        let raw = grant.into_raw();
        match libredox::call::call_wo(
            disk.as_raw_fd() as usize,
            &raw.to_ne_bytes(),
            CallFlags::FD,
            &[GRANT_SLOT],
        ) {
            Ok(_) => {}
            Err(e) if e.errno() == EOPNOTSUPP => {
                return Err(unavailable(Feature::GrantDma, e.errno()));
            }
            Err(e) if e.errno() == ENODEV => {
                return Err(unavailable(Feature::DmaDomain, e.errno()));
            }
            Err(e) => {
                return Err(EngineError::Errno {
                    what: "attach the grant to the disk",
                    errno: e.errno(),
                });
            }
        }
        Ok(registered)
    }
}

/// A `kernel.ring` ring of one SQ entry, set up and mapped.
struct Ring {
    fd: Fd,
    base: NonNull<u8>,
    layout: Layout,
    sq: Producer<Sqe>,
    cq: Consumer<Cqe>,
}

// SAFETY: the ring's memory is mapped for as long as the `Ring` lives and is only reached
// through `&mut self`, so moving a `Ring` to its submitter thread moves the only access to it.
unsafe impl Send for Ring {}

impl Ring {
    fn new() -> Result<Self, EngineError> {
        let layout = Layout::new(1, 0).expect("one SQ and two CQ entries are a valid layout");
        let path = format!("/scheme/{}/{RING_PATH}", ferric_ring::abi::SCHEME_NAME);
        let fd = Fd::open(&path, O_RDWR | O_CLOEXEC, 0).map_err(|e| EngineError::Unavailable {
            engine: Engine::Ring,
            feature: Feature::Rings,
            errno: e.errno(),
        })?;
        call(
            &fd,
            "set up the ring",
            RingRequest::Setup {
                sq_entries: 1,
                cq_entries: 0,
                notify: NotifyMode::Event,
            },
        )?;
        // SAFETY: a fresh shared mapping of the ring's memory at an address the kernel chooses.
        let addr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                layout.map_len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.raw() as c_int,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(EngineError::Errno {
                what: "mmap the ring",
                errno: last_errno(),
            });
        }
        let base = NonNull::new(addr.cast()).ok_or(EngineError::Errno {
            what: "mmap the ring",
            errno: EIO,
        })?;
        Ok(Self {
            fd,
            base,
            layout,
            sq: Producer::new(layout.sq()),
            cq: Consumer::new(layout.cq()),
        })
    }

    /// Runs `sqe` with one `Enter` and returns its result: bytes, or minus the errno.
    fn run(&mut self, sqe: &Sqe) -> Result<i64, EngineError> {
        // SAFETY: the mapping is `map_len` bytes of page-aligned shared memory, mapped until
        // drop, which needs `&mut self` and so cannot overlap this borrow. `AtomicU64` has a
        // `u64`'s size and alignment, any bit pattern is valid, and the kernel accesses the
        // memory only atomically.
        let mem = unsafe {
            core::slice::from_raw_parts(self.base.as_ptr().cast::<AtomicU64>(), self.layout.words())
        };
        let broken = |what| EngineError::Errno { what, errno: EIO };
        self.sq
            .stage(mem, sqe)
            .map_err(|_| broken("stage a ring entry"))?;
        self.sq
            .publish(mem)
            .map_err(|_| broken("publish a ring entry"))?;
        call(
            &self.fd,
            "enter the ring",
            RingRequest::Enter {
                to_submit: 1,
                min_complete: 0,
            },
        )?;
        let cqe = self
            .cq
            .pop(mem)
            .map_err(|_| broken("read the completion queue"))?
            .ok_or(EngineError::NoCompletion)?;
        Ok(cqe.result)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`; no borrow of it outlives `&mut self`.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.layout.map_len()) };
    }
}

fn call(fd: &Fd, what: &'static str, request: RingRequest) -> Result<usize, EngineError> {
    let (words, n) = request.encode();
    ctx(what, fd.call_wo(&[], CallFlags::empty(), &words[..n]))
}

enum Kind {
    /// `pread` into a heap buffer.
    Psync(HeapBuffer),
    /// A read through the grant into the submitter's stride of the registered buffer, which
    /// starts `offset` bytes into the grant.
    Regbuf { offset: u64 },
    /// A `Read` entry on the submitter's own ring, into a heap buffer.
    Ring { ring: Ring, buf: HeapBuffer },
}

/// What one submitter reads with.
pub struct Reader<'a> {
    file: &'a File,
    bs: usize,
    /// The disk's logical block size, which grant reads count in.
    block: u64,
    kind: Kind,
}

impl Reader<'_> {
    /// Reads the job's `bs` bytes at byte `at` of the disk and returns how many it read.
    ///
    /// # Errors
    ///
    /// [`EngineError::Errno`] when the read fails, [`EngineError::Encode`] for a grant read
    /// the call metadata cannot hold.
    pub fn read(&mut self, at: u64) -> Result<usize, EngineError> {
        let errno = |what, errno| EngineError::Errno { what, errno };
        match &mut self.kind {
            Kind::Psync(buf) => self
                .file
                .read_at(buf.get(), at)
                .map_err(|e| errno("pread", e.raw_os_error().unwrap_or(EIO))),
            Kind::Regbuf { offset } => {
                let transfer = Transfer {
                    slot: Slot::new(GRANT_SLOT).expect("slot 0 is below MAX_SLOTS"),
                    offset: *offset,
                    lba: at / self.block,
                    len: self.bs as u64,
                };
                let (words, n) = DiskRequest::Read(transfer)
                    .encode()
                    .map_err(EngineError::Encode)?;
                libredox::call::call_wo(
                    self.file.as_raw_fd() as usize,
                    &[],
                    CallFlags::empty(),
                    &words[..n],
                )
                .map_err(|e| errno("a read through the grant", e.errno()))?;
                // A grant read moves the whole range or fails.
                Ok(self.bs)
            }
            Kind::Ring { ring, buf } => {
                let bytes = buf.get();
                let sqe = Sqe::read(
                    self.file.as_raw_fd() as u32,
                    at,
                    bytes.as_mut_ptr() as u64,
                    bytes.len() as u32,
                    0,
                );
                let result = ring.run(&sqe)?;
                usize::try_from(result)
                    .map_err(|_| errno("a ring read", i32::try_from(-result).unwrap_or(EIO)))
            }
        }
    }
}

/// Every submitter's [`Reader`] for a job, and the registered buffer they share.
pub struct Readers<'a> {
    readers: Vec<Mutex<Reader<'a>>>,
    /// Declared after `readers`; nothing in them points into it.
    _registered: Option<Registered>,
}

impl<'a> Readers<'a> {
    /// Sets up `job`'s request path on `file`, a disk of `block`-byte blocks: a reader for
    /// each of its submitters.
    ///
    /// # Errors
    ///
    /// [`EngineError::Unavailable`] when the path needs a feature this system lacks, and
    /// the setup's other errors ([`EngineError::Errno`], [`EngineError::TooLarge`]).
    pub fn new(file: &'a File, job: &Job, block: u32) -> Result<Self, EngineError> {
        let bs = job.bs as usize;
        let n = job.submitters();
        let registered = match job.engine {
            Engine::Regbuf => Some(Registered::new(file, n, job.bs)?),
            _ => None,
        };
        let readers = (0..n)
            .map(|i| {
                let kind = match job.engine {
                    Engine::Regbuf => Kind::Regbuf {
                        offset: registered.as_ref().map_or(0, |r| r.stride * i as usize) as u64,
                    },
                    Engine::Ring => Kind::Ring {
                        ring: Ring::new()?,
                        buf: HeapBuffer::new(bs, job.buffer_align()),
                    },
                    _ => Kind::Psync(HeapBuffer::new(bs, job.buffer_align())),
                };
                Ok(Mutex::new(Reader {
                    file,
                    bs,
                    block: u64::from(block.max(1)),
                    kind,
                }))
            })
            .collect::<Result<_, EngineError>>()?;
        Ok(Self {
            readers,
            _registered: registered,
        })
    }

    /// Submitter `i`'s reader, which only that submitter uses during a round.
    pub fn get(&self, i: usize) -> Option<MutexGuard<'_, Reader<'a>>> {
        let reader = self.readers.get(i)?;
        // A reader stays usable after a submitter panicked holding it.
        Some(reader.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
