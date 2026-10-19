//! Ferric Oxide: the `kernel.ring` scheme, submission/completion rings whose requests the
//! kernel runs on any scheme (io/rings-design.md in the project files; built with the `rings`
//! feature; format and calls in `ferric_ring`).
//!
//! - Opening [`RING_PATH`] from the scheme root gives an unset ring. [`Request::Setup`]
//!   allocates its memory, zeroed pages the ring owns ([`Frames`]), and writes the header;
//!   the application maps them with `mmap` (`MAP_SHARED`, the whole [`Layout::map_len`]).
//!   The mapping holds its own references to the frames, so the application may unmap it, and
//!   the ring's frames are freed once both the ring and every mapping are gone.
//! - [`Request::Enter`] is a *brokered* submission: in the caller's own context, the kernel
//!   takes complete chains from the SQ (`ferric_ring::chain`) and runs each SQE as the system
//!   call it stands for on the caller's descriptor: `Read` and `Write` as `read`/`write`
//!   (offset [`CURRENT_POSITION`]) or `SYS_READ2`/`SYS_WRITE2`, `Fsync` as `fsync`. A user
//!   scheme sees an ordinary scheme request (`scheme/user.rs`), so every scheme works
//!   unchanged, and the buffer is checked like any system call's. Each SQE gets one CQE; a
//!   chain is taken only when the CQ has room for all of its CQEs, so none is ever dropped.
//!   After posting completions, `Enter` triggers `EVENT_READ` on the ring.
//! - `fevent` reports `EVENT_READ` while the CQ holds completions the application has not
//!   taken.
//!
//! The ring memory is shared with the application, which may write any of it at any time.
//! The kernel reads it only through its own mapping of the frames, with atomic accesses
//! (`RaiiFrame::words`), keeps its SQ head and CQ tail privately (`ferric_ring::queue`),
//! copies each SQE out once and checks it, and treats an out-of-range application index as a
//! corrupt ring (`EINVAL`). Only one `Enter` runs on a ring at a time: it takes the ring's
//! private indices out of their lock for the duration (`Entered`), and a second one gets
//! `EBUSY`. No lock is held while an SQE runs, since that may block in another scheme.
//! `Enter` looks up every file a chain names when it takes the chain, before any of it runs,
//! and holds those descriptions until the chain is done ([`resolve_files`]), so a chain acts
//! on the files its submitter named even if another thread closes or reuses one of the
//! descriptors meanwhile, as a single system call would.
//!
//! Not yet (later PRs of the design): fixed files and grant buffers (`EOPNOTSUPP`), waiting in
//! `Enter` for completions, the `SqPoll` and `Umwait` notify modes (`EOPNOTSUPP`), and rings
//! served natively by user schemes.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use ferric_ring::{
    abi::{header, Layout, NotifyMode, Request, PAGE_SIZE as RING_PAGE_SIZE, RING_PATH},
    chain::{Chain, Next, SqReader},
    entry::{Buffer, FileRef, Op, SqeError, CURRENT_POSITION},
    queue::{HeaderInfo, Memory, RingError},
    Consumer, Cqe, Producer, Sqe,
};
use spin::{Mutex, Once};
use syscall::{
    error::{
        Error, Result, EBADF, EBUSY, ECANCELED, EEXIST, EINVAL, EIO, ENOENT, ENOMEM, EOPNOTSUPP,
    },
    flag::{CallFlags, EventFlags, MapFlags, EVENT_READ},
};

use super::{CallerCtx, FileHandle, KernelScheme, OpenResult, SchemeExt, StrOrBytes};
use crate::{
    context::{
        file::{InternalFlags, LockedFileDescription},
        memory::{handle_notify_files, AddrSpaceWrapper, Grant, PageSpan},
    },
    event,
    memory::{Frame, RaiiFrame, PAGE_SIZE},
    sync::CleanLockToken,
    syscall::{
        data::{GlobalSchemes, Map},
        fs::{
            description_op, file_description, read_description, release_description,
            write_description,
        },
        usercopy::{UserSlice, UserSliceRw},
    },
};

/// The number of the scheme root.
const ROOT: usize = 0;

/// 64-bit words per page.
const WORDS_PER_PAGE: usize = PAGE_SIZE / 8;

const _: () = assert!(RING_PAGE_SIZE == PAGE_SIZE);

/// The scheme.
pub struct RingScheme;

/// A ring's memory: zeroed frames the ring owns, reached through the kernel's linear map.
struct Frames(Vec<RaiiFrame>);

impl Memory for Frames {
    fn word(&self, index: usize) -> Option<&AtomicU64> {
        self.0
            .get(index / WORDS_PER_PAGE)?
            .words()
            .get(index % WORDS_PER_PAGE)
    }

    fn len(&self) -> usize {
        self.0.len().saturating_mul(WORDS_PER_PAGE)
    }
}

/// What `Setup` fixes for the ring's life.
struct Shared {
    frames: Frames,
    layout: Layout,
}

/// The kernel's private indices: the SQ's head and the CQ's tail.
struct Indices {
    sq: SqReader,
    cq: Producer<Cqe>,
}

/// A file an SQE runs on: a reference to its open file description.
type Description = Arc<LockedFileDescription>;

/// A ring.
struct Ring {
    shared: Once<Shared>,
    /// `None` before `Setup`, and while an `Enter` holds the indices.
    indices: Mutex<Option<Indices>>,
}

/// The indices of a ring, taken by the running `Enter` and put back when it ends.
struct Entered<'r> {
    ring: &'r Ring,
    indices: Option<Indices>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        *self.ring.indices.lock() = self.indices.take();
    }
}

/// Every open ring, by handle number.
static HANDLES: Mutex<BTreeMap<usize, Arc<Ring>>> = Mutex::new(BTreeMap::new());

/// The next handle number.
static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT + 1);

/// The ring with handle number `id`.
fn ring(id: usize) -> Result<Arc<Ring>> {
    HANDLES.lock().get(&id).cloned().ok_or(Error::new(EBADF))
}

/// The errno for a ring that cannot be used: the application corrupted it.
fn ring_errno(_err: RingError) -> Error {
    Error::new(EINVAL)
}

/// The errno an SQE that is not a valid request completes with.
fn sqe_errno(err: SqeError) -> Error {
    Error::new(match err {
        SqeError::Opcode(_) => EOPNOTSUPP,
        _ => EINVAL,
    })
}

/// [`Request::Setup`]: allocates the ring's memory and writes its header.
///
/// # Errors
///
/// `EOPNOTSUPP` for a notify mode other than `Event`, `EINVAL` for entry counts that make no
/// layout, `ENOMEM` when the frames cannot be allocated, `EEXIST` when the ring is set up.
fn setup(ring: &Ring, sq_entries: u32, cq_entries: u32, notify: NotifyMode) -> Result<usize> {
    if notify != NotifyMode::Event {
        return Err(Error::new(EOPNOTSUPP));
    }
    let layout = Layout::new(sq_entries, cq_entries).map_err(|_| Error::new(EINVAL))?;
    if ring.shared.is_completed() {
        return Err(Error::new(EEXIST));
    }
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(layout.pages())
        .map_err(|_| Error::new(ENOMEM))?;
    for _ in 0..layout.pages() {
        frames.push(RaiiFrame::allocate().map_err(|_| Error::new(ENOMEM))?);
    }
    let frames = Frames(frames);
    HeaderInfo::write(&frames, &layout, notify).map_err(|_| Error::new(EIO))?;

    let mut indices = ring.indices.lock();
    // A concurrent Setup may have won; its memory stays, this one's frames are freed.
    if ring.shared.is_completed() {
        return Err(Error::new(EEXIST));
    }
    ring.shared.call_once(|| Shared { frames, layout });
    *indices = Some(Indices {
        sq: SqReader::new(Consumer::new(layout.sq())),
        cq: Producer::new(layout.cq()),
    });
    Ok(0)
}

/// [`Request::Enter`]: runs complete chains from the SQ until `to_submit` SQEs are consumed
/// (the last chain may take it past), the SQ has no complete chain, or the CQ has no room for
/// the next one; returns the SQEs consumed.
///
/// # Errors
///
/// `EINVAL` before `Setup` or for a corrupt ring, `EBUSY` while another `Enter` runs or when
/// the CQ has no room for the first chain.
fn enter(ring: &Ring, to_submit: u32, token: &mut CleanLockToken) -> Result<usize> {
    let shared = ring.shared.get().ok_or(Error::new(EINVAL))?;
    let mut entered = Entered {
        ring,
        indices: ring.indices.lock().take(),
    };
    let indices = entered.indices.as_mut().ok_or(Error::new(EBUSY))?;
    let mem = &shared.frames;
    let mut consumed: u32 = 0;
    while consumed < to_submit {
        let next = indices.sq.peek(mem).map_err(ring_errno)?;
        let (Next::Chain(chain) | Next::Refused(chain)) = next else {
            break;
        };
        if indices.cq.free(mem).map_err(ring_errno)? < chain.len() {
            if consumed == 0 {
                return Err(Error::new(EBUSY));
            }
            break;
        }
        indices.sq.consume(mem, &next).map_err(ring_errno)?;
        consumed = consumed.saturating_add(chain.len());

        let cq = &mut indices.cq;
        let mut posted = Ok(());
        let mut post = |cqe: Cqe| {
            if posted.is_ok() {
                posted = cq.stage(mem, &cqe);
            }
        };
        if matches!(next, Next::Refused(_)) {
            chain.refuse(errno_result(EINVAL), post);
        } else {
            let files = resolve_files(&chain, token);
            run_chain(&chain, files, token, &mut post);
        }
        // The room was checked, so staging fails only if the application moved its CQ head
        // out of range meanwhile.
        posted.map_err(|_| Error::new(EINVAL))?;
        cq.publish(mem).map_err(ring_errno)?;
    }
    Ok(consumed as usize)
}

/// The CQE result of a failure with `errno`.
fn errno_result(errno: i32) -> i64 {
    i64::from(errno).saturating_neg()
}

/// Each SQE's file in a chain, looked up when `Enter` takes the chain: `None` for an SQE
/// that names none.
type ChainFiles = Vec<Option<Result<Description>>>;

/// Looks up every file `chain` names and holds its description, before any of the chain
/// runs.
fn resolve_files(chain: &Chain, token: &mut CleanLockToken) -> ChainFiles {
    chain
        .entries()
        .iter()
        .map(|sqe| {
            let file = sqe.op().ok().and_then(|op| op.file());
            file.map(|file| resolve(file, token))
        })
        .collect()
}

/// [`release_description`] for every file of a chain that is still held.
fn release_files(files: ChainFiles, token: &mut CleanLockToken) {
    for description in files.into_iter().flatten().flatten() {
        release_description(description, token);
    }
}

/// Runs `chain` by its break rules on the files [`resolve_files`] found for it, passes each
/// CQE to `post`, and lets go of the files.
fn run_chain(
    chain: &Chain,
    mut files: ChainFiles,
    token: &mut CleanLockToken,
    post: impl FnMut(Cqe),
) {
    // `Chain::run` runs the SQEs in order, so the next slot is always the running SQE's.
    let mut slots = files.iter_mut();
    chain.run(
        |sqe| {
            let file = slots.next().and_then(Option::take);
            run(sqe, file, token)
        },
        errno_result(ECANCELED),
        post,
    );
    release_files(files, token);
}

/// Runs `sqe` on `file`, the file it names as `Enter` found it, and returns its CQE result.
fn run(sqe: &Sqe, file: Option<Result<Description>>, token: &mut CleanLockToken) -> i64 {
    let mut file = file;
    let result = sqe
        .op()
        .map_err(sqe_errno)
        .and_then(|op| run_op(op, &mut file, token));
    if let Some(Ok(description)) = file {
        release_description(description, token);
    }
    match result {
        Ok(value) => Cqe::encode_result(Ok(value as u64)),
        Err(err) => errno_result(err.errno),
    }
}

/// The description `file` names: the caller's descriptor's.
///
/// # Errors
///
/// `EBADF` when the descriptor is not open, `EOPNOTSUPP` for a fixed file.
fn resolve(file: FileRef, token: &mut CleanLockToken) -> Result<Description> {
    match file {
        FileRef::Fd(fd) => file_description(FileHandle::from(fd as usize), token),
        FileRef::Fixed(_) => Err(Error::new(EOPNOTSUPP)),
    }
}

/// Runs `op` on the SQE's file, taken from `file`; `op` lets go of it.
fn with_file<T>(
    file: &mut Option<Result<Description>>,
    token: &mut CleanLockToken,
    op: impl FnOnce(Description, &mut CleanLockToken) -> Result<T>,
) -> Result<T> {
    // The op hands the reference on to `description_op`, which lets go of it afterwards.
    op(file.take().unwrap_or(Err(Error::new(EBADF)))?, token)
}

/// The address and length of a buffer in the caller's address space.
fn user_buffer(buf: Buffer) -> Result<(usize, usize)> {
    match buf {
        Buffer::Addr { addr, len } => Ok((
            usize::try_from(addr).map_err(|_| Error::new(EINVAL))?,
            len as usize,
        )),
        Buffer::Grant { .. } => Err(Error::new(EOPNOTSUPP)),
    }
}

/// Runs `op` as the system call it stands for, on the caller's memory and `file`, the
/// description of the descriptor it names.
fn run_op(
    op: Op,
    file: &mut Option<Result<Description>>,
    token: &mut CleanLockToken,
) -> Result<usize> {
    match op {
        Op::Nop => Ok(0),
        Op::Read { offset, buf, .. } => {
            let (addr, len) = user_buffer(buf)?;
            let buf = UserSlice::wo(addr, len)?;
            with_file(file, token, |description, token| {
                if offset == CURRENT_POSITION {
                    return read_description(description, buf, token);
                }
                description_op(description, token, |scheme, _, desc, token| {
                    scheme.kreadoff(desc.number, buf, offset, desc.flags, desc.flags, token)
                })
            })
        }
        Op::Write { offset, buf, .. } => {
            let (addr, len) = user_buffer(buf)?;
            let buf = UserSlice::ro(addr, len)?;
            with_file(file, token, |description, token| {
                if offset == CURRENT_POSITION {
                    return write_description(description, buf, token);
                }
                description_op(description, token, |scheme, _, desc, token| {
                    scheme.kwriteoff(desc.number, buf, offset, desc.flags, desc.flags, token)
                })
            })
        }
        Op::Fsync { .. } => with_file(file, token, |description, token| {
            description_op(description, token, |scheme, _, desc, token| {
                scheme.fsync(desc.number, token).map(|()| 0)
            })
        }),
        _ => Err(Error::new(EOPNOTSUPP)),
    }
}

/// Maps the ring's `frames` into `space`, anywhere, as one shared grant the process may unmap.
///
/// # Errors
///
/// `ENOMEM` when the address space or its page tables have no room.
fn map_frames(
    space: &Arc<AddrSpaceWrapper>,
    frames: &[RaiiFrame],
    prot: MapFlags,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let frames: Vec<Frame> = frames.iter().map(RaiiFrame::get).collect();
    let count = NonZeroUsize::new(frames.len()).ok_or(Error::new(EINVAL))?;
    let span = {
        let mut lock_token = token.token();
        let mut guard = space.acquire_write(lock_token.downgrade());
        let base = guard.mmap(
            space,
            None,
            count,
            prot | MapFlags::MAP_SHARED,
            None,
            |base, flags, mapper, flusher| {
                let is_pinned = false;
                Grant::allocated_shared(&frames, base, flags, mapper, flusher, is_pinned)
            },
        )?;
        let pages = guard
            .grants
            .contains(base)
            .map_or(0, |(_, info)| info.page_count());
        PageSpan::new(base, pages)
    };
    if span.count == frames.len() {
        return Ok(span.base.start_address().data());
    }
    // The page tables ran out part way: undo the part that was mapped.
    let notify = space.munmap(span, false, token)?;
    handle_notify_files(notify, token);
    Err(Error::new(ENOMEM))
}

impl KernelScheme for RingScheme {
    fn scheme_root(&self, _token: &mut CleanLockToken) -> Result<usize> {
        Ok(ROOT)
    }

    fn kopenat(
        &self,
        file: usize,
        path: StrOrBytes,
        _flags: usize,
        _fcntl_flags: u32,
        _caller: CallerCtx,
        _token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        let path = path.as_bytes();
        let path = path.strip_prefix(b"/").unwrap_or(path);
        if file != ROOT || path != RING_PATH.as_bytes() {
            return Err(Error::new(ENOENT));
        }
        let ring = Arc::new(Ring {
            shared: Once::new(),
            indices: Mutex::new(None),
        });
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        HANDLES.lock().insert(id, ring);
        Ok(OpenResult::SchemeLocal(id, InternalFlags::empty()))
    }

    fn close(&self, id: usize, _token: &mut CleanLockToken) -> Result<()> {
        // A running Enter keeps its own reference; the frames go with the last one, or with
        // the last mapping.
        HANDLES.lock().remove(&id);
        Ok(())
    }

    fn kcall(
        &self,
        fds: &[usize],
        _payload: UserSliceRw,
        _flags: CallFlags,
        metadata: &[u64],
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let &[id] = fds else {
            return Err(Error::new(EINVAL));
        };
        let ring = ring(id)?;
        match Request::decode(metadata).map_err(|_| Error::new(EINVAL))? {
            Request::Setup {
                sq_entries,
                cq_entries,
                notify,
            } => setup(&ring, sq_entries, cq_entries, notify),
            Request::Enter { to_submit } => {
                let consumed = enter(&ring, to_submit, token)?;
                if consumed != 0 {
                    event::trigger(GlobalSchemes::Ring.scheme_id(), id, EVENT_READ, token);
                }
                Ok(consumed)
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn fevent(
        &self,
        id: usize,
        flags: EventFlags,
        _token: &mut CleanLockToken,
    ) -> Result<EventFlags> {
        let ring = ring(id)?;
        let Some(shared) = ring.shared.get() else {
            return Ok(EventFlags::empty());
        };
        let load = |index| {
            shared
                .frames
                .word(index)
                .map_or(0, |word| word.load(Ordering::Acquire))
        };
        if load(header::CQ_TAIL) != load(header::CQ_HEAD) {
            Ok(flags & EVENT_READ)
        } else {
            Ok(EventFlags::empty())
        }
    }

    fn kfmap(
        &self,
        number: usize,
        addr_space: &Arc<AddrSpaceWrapper>,
        map: &Map,
        _consume: bool,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let ring = ring(number)?;
        let shared = ring.shared.get().ok_or(Error::new(EINVAL))?;
        if map
            .flags
            .intersects(MapFlags::MAP_FIXED | MapFlags::MAP_FIXED_NOREPLACE)
        {
            return Err(Error::new(EOPNOTSUPP));
        }
        if !map.flags.contains(MapFlags::MAP_SHARED)
            || map.offset != 0
            || map.size != shared.layout.map_len()
        {
            return Err(Error::new(EINVAL));
        }
        let prot = map.flags & (MapFlags::PROT_READ | MapFlags::PROT_WRITE);
        map_frames(addr_space, &shared.frames.0, prot, token)
    }
}
