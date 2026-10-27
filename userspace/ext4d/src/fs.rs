//! The open-file table: what a scheme's `openat`, `read`, `getdents`, `fstat` and `close`
//! do, on an ext4 volume read by `ext4-view`.
//!
//! Handles are numbers the scheme gives its clients. Each names an open file or directory
//! with its path from the volume's root, so relative opens and `fpath` work. Paths are
//! resolved component by component, checking directory search permissions before descending
//! or processing `..`. Symbolic links are expanded with the same checks, except where
//! `O_SYMLINK` or `O_STAT | O_NOFOLLOW` asks for the final link itself.
//!
//! [`OpenMode::wants_write`] opens and every changing call fail with [`Error::ReadOnly`],
//! with three exceptions: on a volume mounted with `Fs::new_writable` (Cargo feature
//! `write`, features.toml `fs.ext4_write`), [`Fs::set_times`], [`Fs::set_mode`] and
//! [`Fs::set_owner`] set a file's times, permission bits, and owner and group through the
//! journal (`crate::write`), each checking the caller's permission as POSIX says. The inode to change is found from the handle's path by
//! `crate::lookup`. After each change the `ext4-view` reader is loaded again, since it keeps
//! the blocks it has read, and every handle's metadata is read again from it.
//!
//! `ext4-view` exposes neither inode numbers nor change times. Each handle's inode is found
//! at open by `crate::lookup`, and [`Fs::attributes`] reads the change time from it.
//!
//! Limitation: [`Attributes::ino`] is a stable number made from the path, so hard links to
//! one inode get different numbers.

/// Text path normalization shared with the other filesystem daemons.
pub use ferric_fspath::normalize;
use ferric_fspath::path_ino;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use ext4_view::{Ext4, Ext4Read, File, FileType, Metadata};
use ferric_abi::time::{UTIME_NOW, UTIME_OMIT};

use crate::Error;
use crate::superblock::{self, Geometry};

/// What an `openat` asks for, from its flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenMode {
    /// Write access (`O_WRONLY` or `O_RDWR`).
    pub write: bool,
    /// Creation or truncation (`O_CREAT`, `O_TRUNC`).
    pub create_or_truncate: bool,
    /// The path must be a directory (`O_DIRECTORY`).
    pub directory: bool,
    /// Open a final symbolic link itself, to read its target (`O_SYMLINK`).
    pub symlink: bool,
    /// Do not follow a final symbolic link (`O_NOFOLLOW`).
    pub no_follow: bool,
    /// Open metadata only (`O_STAT`), without permitting file reads.
    pub stat: bool,
}

impl OpenMode {
    /// A plain read-only open.
    pub const fn read() -> Self {
        Self {
            write: false,
            create_or_truncate: false,
            directory: false,
            symlink: false,
            no_follow: false,
            stat: false,
        }
    }

    /// Reads the mode from Redox `openat` flags.
    pub const fn from_flags(flags: usize) -> Self {
        use syscall::flag::{
            O_ACCMODE, O_CREAT, O_DIRECTORY, O_NOFOLLOW, O_RDONLY, O_STAT, O_SYMLINK, O_TRUNC,
        };
        let access = flags & O_ACCMODE;
        Self {
            write: access != O_RDONLY && access != 0,
            create_or_truncate: flags & (O_CREAT | O_TRUNC) != 0,
            directory: flags & O_DIRECTORY != 0,
            symlink: flags & O_SYMLINK != 0,
            no_follow: flags & O_NOFOLLOW != 0,
            stat: flags & O_STAT != 0,
        }
    }

    /// Whether the open would change the volume or allow changing it.
    pub const fn wants_write(&self) -> bool {
        self.write || self.create_or_truncate
    }
}

/// The kind of file a handle or directory entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A character device.
    CharDevice,
    /// A block device.
    BlockDevice,
    /// A named pipe.
    Fifo,
    /// A socket.
    Socket,
}

impl Kind {
    /// The file type bits of `i_mode` (and of `st_mode`) for this kind, from the `i_mode`
    /// table in "Ext4 Disk Layout" (the "Inode Table" section).
    pub const fn mode_bits(self) -> u16 {
        match self {
            Self::Fifo => 0x1000,
            Self::CharDevice => 0x2000,
            Self::Directory => 0x4000,
            Self::BlockDevice => 0x6000,
            Self::File => 0x8000,
            Self::Symlink => 0xA000,
            Self::Socket => 0xC000,
        }
    }
}

impl From<FileType> for Kind {
    fn from(file_type: FileType) -> Self {
        match file_type {
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::Symlink,
            FileType::CharacterDevice => Self::CharDevice,
            FileType::BlockDevice => Self::BlockDevice,
            FileType::Fifo => Self::Fifo,
            FileType::Socket => Self::Socket,
            FileType::Regular => Self::File,
        }
    }
}

/// A file's attributes, for `fstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes {
    /// A stable number for the file (see the module docs).
    pub ino: u64,
    /// The mode: the file type bits ([`Kind::mode_bits`]) and the permission bits.
    pub mode: u16,
    /// The kind of file.
    pub kind: Kind,
    /// The owner.
    pub uid: u32,
    /// The group.
    pub gid: u32,
    /// The size in bytes (for a symbolic link, of its target).
    pub size: u64,
    /// The volume's block size.
    pub block_size: u32,
    /// The last access, in seconds and nanoseconds since the epoch.
    pub accessed: (i64, u32),
    /// The last change to the contents.
    pub modified: (i64, u32),
    /// The last change to the inode (`i_ctime`): its contents, or attributes such as its
    /// times. The modification time when the inode could not be found (see
    /// [`Fs::attributes`]).
    pub changed: (i64, u32),
}

impl Attributes {
    /// The attributes as the scheme ABI's `Stat`. `st_blocks` counts 512-byte units of the
    /// size, as `ext4-view` does not expose the blocks allocated; times before 1970 read as
    /// 1970.
    pub fn to_stat(&self) -> syscall::data::Stat {
        let secs = |t: (i64, u32)| u64::try_from(t.0).unwrap_or(0);
        syscall::data::Stat {
            st_dev: 0,
            st_ino: self.ino,
            st_mode: self.mode,
            st_nlink: 1,
            st_uid: self.uid,
            st_gid: self.gid,
            st_size: self.size,
            st_blksize: self.block_size,
            st_blocks: self.size.div_ceil(512),
            st_mtime: secs(self.modified),
            st_mtime_nsec: self.modified.1,
            st_atime: secs(self.accessed),
            st_atime_nsec: self.accessed.1,
            st_ctime: secs(self.changed),
            st_ctime_nsec: self.changed.1,
        }
    }
}

impl Kind {
    /// The kind as a scheme directory entry's kind.
    pub const fn dirent_kind(self) -> syscall::dirent::DirentKind {
        use syscall::dirent::DirentKind;
        match self {
            Self::File => DirentKind::Regular,
            Self::Directory => DirentKind::Directory,
            Self::Symlink => DirentKind::Symlink,
            Self::CharDevice => DirentKind::CharDev,
            Self::BlockDevice => DirentKind::BlockDev,
            Self::Socket => DirentKind::Socket,
            // The scheme ABI has no kind for named pipes.
            Self::Fifo => DirentKind::Unspecified,
        }
    }
}

/// What a `futimens` call asks for one of a file's times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeChange {
    /// Set it to the current time (`UTIME_NOW`).
    Now,
    /// Leave it as it is (`UTIME_OMIT`).
    Omit,
    /// Set it to this time.
    At {
        /// Seconds since 1970-01-01 00:00:00 UTC; negative before it.
        seconds: i64,
        /// Nanoseconds, below 1 000 000 000.
        nanoseconds: u32,
    },
}

impl TimeChange {
    /// Reads a `futimens` time: `tv_nsec` is [`UTIME_NOW`], [`UTIME_OMIT`] or the
    /// nanoseconds of `tv_sec`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTime`] when `nanoseconds` is none of those.
    pub fn from_timespec(seconds: i64, nanoseconds: i64) -> Result<Self, Error> {
        match nanoseconds {
            UTIME_NOW => Ok(Self::Now),
            UTIME_OMIT => Ok(Self::Omit),
            0..1_000_000_000 => Ok(Self::At {
                seconds,
                // In range, so it fits.
                nanoseconds: nanoseconds as u32,
            }),
            _ => Err(Error::InvalidTime),
        }
    }
}

/// One directory entry, for `getdents`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry's name.
    pub name: String,
    /// The kind of file it names.
    pub kind: Kind,
    /// A stable number for the file, as in [`Attributes::ino`].
    pub ino: u64,
}

enum Open {
    Metadata,
    File(Box<File>),
    Directory { entries: Option<Vec<Entry>> },
    Symlink { target: Vec<u8> },
}

struct Handle {
    /// The path from the volume's root, normalized, starting with `/`.
    path: String,
    /// Its inode number, for what `ext4-view` does not expose (the change time), when
    /// [`crate::lookup`] could find it.
    inode: Option<u32>,
    metadata: Metadata,
    open: Open,
}

/// Why [`Fs::mount_preferring_write`] mounted a volume read-only.
#[cfg(feature = "write")]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadOnlyReason<E> {
    /// The volume cannot be written ([`crate::write::check_writable`]).
    #[error("the volume cannot be written")]
    Volume(#[source] crate::write::WriteError),
    /// The device could not be opened for writing (`reopen` failed).
    #[error("the device cannot be opened for writing")]
    Device(#[source] E),
}

/// `i_mode`'s permission bits and the set-group-ID bit among them.
#[cfg(feature = "write")]
const PERMISSION_BITS: u16 = 0o7777;
#[cfg(feature = "write")]
const S_ISGID: u16 = 0o2000;

/// A mounted ext4 volume and its open handles.
pub struct Fs {
    ext4: Ext4,
    geometry: Geometry,
    needs_recovery: bool,
    handles: BTreeMap<usize, Handle>,
    next: usize,
    /// The device `ext4-view` reads, for reading what it does not expose.
    device: Box<dyn Device>,
    /// The same device, for changes, on a volume mounted with [`Fs::new_writable`].
    #[cfg(feature = "write")]
    writer: Option<Box<dyn Writable>>,
}

/// The volume's device, with its type erased, beside `ext4-view`'s reader of it.
trait Device {
    /// A reader of the device for `ext4-view`, which sees every change made to it.
    fn reader(&self) -> Box<dyn Ext4Read>;
    /// The inode `path` names.
    fn inode_of(&self, path: &str) -> Result<u32, crate::lookup::LookupError>;
    /// Inode `number`'s access, modification and change times, from one read of it.
    fn times(&self, number: u32) -> Result<crate::lookup::InodeTimes, crate::lookup::LookupError>;
}

/// The device of a volume mounted with [`Fs::new_writable`], for changes.
#[cfg(feature = "write")]
trait Writable {
    /// Makes `change` to inode `number` through the journal.
    fn change(
        &self,
        number: u32,
        change: &crate::write::Change,
    ) -> Result<(), crate::write::WriteError>;
}

/// The device, shared by `ext4-view`'s reader, [`Device`] and the writer. Fs is
/// single-threaded and `ext4-view` reads only inside an `Fs` call, so no borrow is held
/// across another.
struct Shared<W>(Rc<RefCell<crate::device::Window<W>>>);

impl<W: crate::device::ReadAt> Ext4Read for Shared<W> {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> Result<(), Box<dyn core::error::Error + Send + Sync + 'static>> {
        Ok(self.0.try_borrow()?.read_exact_at(dst, start_byte)?)
    }
}

/// The device read as `ext4-view` reads it: on a volume that needs recovery, with the
/// committed journal blocks in place of their home blocks (`journal::committed_blocks`),
/// since `ext4-view` overlays them too and its metadata would otherwise disagree with what
/// is read here.
struct Raw<R> {
    device: Rc<RefCell<crate::device::Window<R>>>,
    /// The committed journal blocks, or `None` when they could not be read; each read then
    /// tries again, to return the error.
    overlay: Option<BTreeMap<u64, Vec<u8>>>,
    block_size: u64,
}

impl<R: crate::device::ReadAt> Raw<R> {
    /// The device, with its overlay read now.
    fn new(device: Rc<RefCell<crate::device::Window<R>>>, block_size: u32) -> Self {
        let overlay = crate::journal::committed_blocks(&device.borrow())
            .inspect_err(|err| {
                log::warn!("cannot read the journal's committed blocks: {err}");
            })
            .ok();
        Self {
            device,
            overlay,
            block_size: u64::from(block_size),
        }
    }

    /// Runs `read` on the device as `ext4-view` sees it.
    fn read<T>(
        &self,
        read: impl FnOnce(
            &crate::device::Window<crate::journal::Overlaid<'_, R>>,
        ) -> Result<T, crate::lookup::LookupError>,
    ) -> Result<T, crate::lookup::LookupError> {
        let device = self.device.borrow();
        let fresh;
        let blocks = match &self.overlay {
            Some(blocks) => blocks,
            None => {
                fresh = crate::journal::committed_blocks(&device)?;
                &fresh
            }
        };
        let overlaid = crate::journal::Overlaid {
            window: &device,
            blocks,
            block_size: self.block_size,
        };
        let window =
            crate::device::Window::whole(overlaid).map_err(crate::journal::JournalError::from)?;
        read(&window)
    }
}

impl<R: crate::device::ReadAt + 'static> Device for Raw<R> {
    fn reader(&self) -> Box<dyn Ext4Read> {
        Box::new(Shared(Rc::clone(&self.device)))
    }

    fn inode_of(&self, path: &str) -> Result<u32, crate::lookup::LookupError> {
        self.read(|window| crate::lookup::inode_of(window, path))
    }

    fn times(&self, number: u32) -> Result<crate::lookup::InodeTimes, crate::lookup::LookupError> {
        self.read(|window| crate::lookup::times(window, number))
    }
}

#[cfg(feature = "write")]
impl<W: crate::device::WriteAt + 'static> Writable for Rc<RefCell<crate::device::Window<W>>> {
    fn change(
        &self,
        number: u32,
        change: &crate::write::Change,
    ) -> Result<(), crate::write::WriteError> {
        crate::write::change(&mut self.borrow_mut(), number, change)
    }
}

impl Fs {
    /// Loads the volume `device` holds.
    ///
    /// # Errors
    ///
    /// [`Error::Device`] when the superblock cannot be read, [`Error::Superblock`] when it is
    /// not an ext4 one, and [`Error::Ext4`] when the reader refuses the volume (an
    /// incompatible feature, a bad checksum, a journal it cannot replay).
    pub fn new<R: crate::device::ReadAt + 'static>(
        device: crate::device::Window<R>,
    ) -> Result<Self, Error> {
        let sb = read_superblock(&device)?;
        let block_size = Geometry::parse(&sb)?.block_size;
        let raw = Raw::new(Rc::new(RefCell::new(device)), block_size);
        Self::load(&sb, Box::new(raw))
    }

    /// The volume whose superblock is `sb`, read through `device`.
    fn load(sb: &[u8; superblock::SIZE], device: Box<dyn Device>) -> Result<Self, Error> {
        let geometry = Geometry::parse(sb)?;
        let needs_recovery = superblock::needs_recovery(sb);
        let ext4 = Ext4::load(device.reader())?;
        Ok(Self {
            ext4,
            geometry,
            needs_recovery,
            handles: BTreeMap::new(),
            next: 1,
            device,
            #[cfg(feature = "write")]
            writer: None,
        })
    }

    /// Loads the volume `device` holds for reading and for the changes `crate::write`
    /// makes: so far, setting times, mode and owner ([`Self::set_times`],
    /// [`Self::set_mode`], [`Self::set_owner`]).
    ///
    /// # Errors
    ///
    /// [`Error::Write`] when the volume cannot be written ([`crate::write::check_writable`]:
    /// it needs replaying, has no journal or has a feature `crate::write` does not write
    /// under); mount it with [`Self::new`] then. Otherwise the errors of [`Self::new`].
    #[cfg(feature = "write")]
    pub fn new_writable<W: crate::device::WriteAt + 'static>(
        device: crate::device::Window<W>,
    ) -> Result<Self, Error> {
        crate::write::check_writable(&device)?;
        let sb = read_superblock(&device)?;
        let device = Rc::new(RefCell::new(device));
        let block_size = Geometry::parse(&sb)?.block_size;
        // A writable volume never needs recovery (check_writable), so its overlay is empty.
        let raw = Raw::new(Rc::clone(&device), block_size);
        let mut fs = Self::load(&sb, Box::new(raw))?;
        fs.writer = Some(Box::new(device));
        Ok(fs)
    }

    /// Loads the volume `window` holds for changes if it can be, and read-only otherwise:
    /// what the daemon does at mount. The volume is checked first
    /// ([`crate::write::check_writable`]); only when it can be written is the device opened
    /// again for writing, with `reopen`. If either refuses, the volume is served read-only
    /// from `window`, and the reason comes back with it for the caller to report.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::new`] and [`Self::new_writable`]: neither refusal above is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use ext4d::device::Window;
    /// use ext4d::fs::{Fs, ReadOnlyReason};
    ///
    /// # fn main() -> Result<(), ext4d::Error> {
    /// # let image = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/basic.ext4"))
    /// #     .expect("the fixture");
    /// let window = Window::whole(image.clone()).expect("its length");
    /// // A device that cannot be opened for writing: the volume is still served.
    /// let (fs, reason) = Fs::mount_preferring_write(window, || {
    ///     Err::<Window<Vec<u8>>, _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    /// })?;
    /// assert!(!fs.is_writable());
    /// assert!(matches!(reason, Some(ReadOnlyReason::Device(_))));
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "write")]
    pub fn mount_preferring_write<R, W, E>(
        window: crate::device::Window<R>,
        reopen: impl FnOnce() -> Result<crate::device::Window<W>, E>,
    ) -> Result<(Self, Option<ReadOnlyReason<E>>), Error>
    where
        R: crate::device::ReadAt + 'static,
        W: crate::device::WriteAt + 'static,
        E: core::error::Error + 'static,
    {
        let reason = match crate::write::check_writable(&window) {
            Err(err) => ReadOnlyReason::Volume(err),
            Ok(()) => match reopen() {
                Ok(writable) => return Ok((Self::new_writable(writable)?, None)),
                Err(err) => ReadOnlyReason::Device(err),
            },
        };
        Ok((Self::new(window)?, Some(reason)))
    }

    /// Whether the volume was mounted for changes (`Fs::new_writable`, Cargo feature `write`).
    pub fn is_writable(&self) -> bool {
        #[cfg(feature = "write")]
        {
            self.writer.is_some()
        }
        #[cfg(not(feature = "write"))]
        {
            false
        }
    }

    /// The volume's size and free space.
    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Whether the volume's journal still needed replaying when it was mounted: it is then
    /// read through `ext4-view`'s in-memory view of the journal, which does not handle every
    /// journal (see `journal`, features.toml `fs.ext4_journal_replay`).
    pub const fn needs_recovery(&self) -> bool {
        self.needs_recovery
    }

    /// The volume's label.
    pub fn label(&self) -> String {
        self.ext4.label().to_str().unwrap_or_default().to_owned()
    }

    /// Opens the root directory, as the scheme's root handle.
    ///
    /// # Errors
    ///
    /// [`Error::Ext4`] when the root directory cannot be read.
    pub fn root(&mut self) -> Result<usize, Error> {
        self.open(None, "/", OpenMode::read())
    }

    /// Opens `path`: from the root when it is absolute or `base` is `None`, otherwise from
    /// the directory `base` names. Returns the new handle.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] for any open that could change the volume, [`Error::BadHandle`]
    /// for an unknown `base`, [`Error::NotADirectory`] when `mode.directory` is set and the
    /// path is not one, [`Error::SymlinkNotFollowed`] for `O_NOFOLLOW` on a link, and
    /// [`Error::Ext4`] when the path cannot be resolved or read. This convenience
    /// entry point opens as UID/GID zero; schemes must use [`Self::open_as`].
    pub fn open(
        &mut self,
        base: Option<usize>,
        path: &str,
        mode: OpenMode,
    ) -> Result<usize, Error> {
        self.open_as(base, path, mode, 0, 0)
    }

    /// Opens a path using the caller's user and primary group IDs.
    ///
    /// Checks search permission on every traversed directory, including symlink targets,
    /// and read permission on the final object unless `mode.stat` requests metadata only.
    /// UID zero bypasses permission checks. Supplementary groups and ACLs are not supported;
    /// access is restricted to the inode's owner/group/other mode bits.
    ///
    /// # Errors
    /// Returns the errors of [`Self::open`], [`Error::PermissionDenied`] for denied access,
    /// and [`Error::NotASymlink`] for `O_SYMLINK` on an ordinary file.
    pub fn open_as(
        &mut self,
        base: Option<usize>,
        path: &str,
        mode: OpenMode,
        uid: u32,
        gid: u32,
    ) -> Result<usize, Error> {
        if mode.wants_write() {
            return Err(Error::ReadOnly);
        }
        let start = match base {
            Some(base) if !path.starts_with('/') => {
                let handle = self.handle(base)?;
                if !handle.metadata.is_dir() {
                    return Err(Error::NotADirectory);
                }
                handle.path.as_str()
            }
            _ => "/",
        };
        let (path, metadata) = self.resolve(start, path, mode, uid, gid)?;
        if mode.directory && !metadata.is_dir() {
            return Err(Error::NotADirectory);
        }
        if mode.symlink && !metadata.is_symlink() {
            return Err(Error::NotASymlink);
        }
        let open = if mode.stat {
            Open::Metadata
        } else if metadata.is_symlink() {
            let target = self.ext4.read_link(path.as_str())?.as_ref().to_vec();
            Open::Symlink { target }
        } else {
            require_access(&metadata, uid, gid, 0o4)?;
            if metadata.is_dir() {
                Open::Directory { entries: None }
            } else {
                Open::File(Box::new(self.ext4.open(path.as_str())?))
            }
        };
        let inode = self.inode_of(&path);
        let handle = Handle {
            path,
            inode,
            metadata,
            open,
        };
        let id = self.next;
        self.next += 1;
        self.handles.insert(id, handle);
        Ok(id)
    }

    /// Closes `id`. Unknown handles are ignored, as a close has no answer.
    pub fn close(&mut self, id: usize) {
        self.handles.remove(&id);
    }

    /// Reads into `buf` from byte `offset` of the file `id` names (for a symbolic link
    /// opened with `O_SYMLINK`, of its target). Returns how many bytes were read: fewer than
    /// `buf.len()` only at the end of the file.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::IsADirectory`] for a directory, and [`Error::Ext4`]
    /// when the reader fails.
    pub fn read(&mut self, id: usize, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        match &mut self.handle_mut(id)?.open {
            Open::Metadata => Err(Error::BadHandle { handle: id }),
            Open::Directory { .. } => Err(Error::IsADirectory),
            Open::Symlink { target } => {
                let start = usize::try_from(offset).map_or(target.len(), |o| o.min(target.len()));
                let src = &target[start..];
                let n = src.len().min(buf.len());
                buf[..n].copy_from_slice(&src[..n]);
                Ok(n)
            }
            Open::File(file) => {
                let size = file.metadata().len();
                file.seek_to(offset.min(size))?;
                let mut done = 0;
                while done < buf.len() {
                    let n = file.read_bytes(&mut buf[done..])?;
                    if n == 0 {
                        break;
                    }
                    done += n;
                }
                Ok(done)
            }
        }
    }

    /// The entries of the directory `id` names, without `.` and `..`, in the order the
    /// directory stores them. The list is read on the first call and kept, so offsets into
    /// it stay valid while the handle is open.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::NotADirectory`] for anything else, and
    /// [`Error::Ext4`] when the directory cannot be read.
    pub fn entries(&mut self, id: usize) -> Result<&[Entry], Error> {
        let ext4 = &self.ext4;
        let handle = self
            .handles
            .get_mut(&id)
            .ok_or(Error::BadHandle { handle: id })?;
        let Open::Directory { entries } = &mut handle.open else {
            return Err(Error::NotADirectory);
        };
        if entries.is_none() {
            *entries = Some(read_entries(ext4, &handle.path)?);
        }
        Ok(entries.as_deref().unwrap_or_default())
    }

    /// The attributes of the file `id` names. Most come from `ext4-view`. The times are read
    /// from the inode itself, all three from one read, because `ext4-view` does not expose
    /// the change time and, on a volume whose journal it does not follow (a wrapped log), can
    /// serve an older copy of the inode than the one the journal holds; taking some times from
    /// each would mix two versions. When the inode could not be found or read (logged), the
    /// times are `ext4-view`'s, with the modification time as the change time.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`].
    pub fn attributes(&self, id: usize) -> Result<Attributes, Error> {
        let handle = self.handle(id)?;
        let m = &handle.metadata;
        let time = |t: ext4_view::Timestamp| (t.seconds(), t.nanoseconds());
        let kind = Kind::from(m.file_type());
        let times = self.inode_times(handle);
        Ok(Attributes {
            ino: path_ino(&handle.path),
            mode: kind.mode_bits() | m.mode(),
            kind,
            uid: m.uid(),
            gid: m.gid(),
            size: m.len(),
            block_size: self.geometry.block_size,
            accessed: times.map_or(time(m.accessed()), |t| t.accessed),
            modified: times.map_or(time(m.modified()), |t| t.modified),
            changed: times.map_or(time(m.modified()), |t| t.changed),
        })
    }

    /// The path of the file `id` names, from the volume's root, starting with `/`.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`].
    pub fn path(&self, id: usize) -> Result<&str, Error> {
        Ok(self.handle(id)?.path.as_str())
    }

    /// Checks that `id` is open, for calls that would change the file and so fail with
    /// [`Error::ReadOnly`] once the handle is known to be good.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`] for an unknown handle, otherwise [`Error::ReadOnly`].
    pub fn refuse_change(&self, id: usize) -> Result<(), Error> {
        self.handle(id)?;
        Err(Error::ReadOnly)
    }

    /// Sets the access and modification times of the file `id` names, as `futimens` does,
    /// and its change time to now, in one journal transaction.
    ///
    /// Following POSIX `futimens`: two [`TimeChange::Omit`]s change nothing and need no
    /// permission; [`TimeChange::Now`] for both needs the caller (`uid`, `gid`) to own the
    /// file or be able to write it; anything else needs the caller to own it. UID zero may
    /// do all of them.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::ReadOnly`] on a volume not mounted with
    /// `Fs::new_writable` (and always without the `write` feature),
    /// [`Error::PermissionDenied`] and [`Error::NotOwner`] as above, `Error::Lookup` when
    /// the file's inode cannot be found, and `Error::Write` when the change fails (see
    /// `write::set_times`).
    pub fn set_times(
        &mut self,
        id: usize,
        accessed: TimeChange,
        modified: TimeChange,
        uid: u32,
        gid: u32,
    ) -> Result<(), Error> {
        let handle = self.handle(id)?;
        #[cfg(not(feature = "write"))]
        {
            let _ = (handle, accessed, modified, uid, gid);
            Err(Error::ReadOnly)
        }
        #[cfg(feature = "write")]
        {
            use crate::write::{Change, Times, Timestamp};

            self.writer.as_ref().ok_or(Error::ReadOnly)?;
            if (accessed, modified) == (TimeChange::Omit, TimeChange::Omit) {
                return Ok(());
            }
            let metadata = &handle.metadata;
            if uid != 0 && metadata.uid() != uid {
                if (accessed, modified) != (TimeChange::Now, TimeChange::Now) {
                    return Err(Error::NotOwner);
                }
                require_access(metadata, uid, gid, 0o2)?;
            }
            let now = Timestamp::now();
            let time = |change| match change {
                TimeChange::Now => Some(now),
                TimeChange::Omit => None,
                TimeChange::At {
                    seconds,
                    nanoseconds,
                } => Some(Timestamp {
                    seconds,
                    nanoseconds,
                }),
            };
            let times = Times {
                accessed: time(accessed),
                modified: time(modified),
                changed: now,
            };
            self.commit(id, &Change::Times(times))
        }
    }

    /// Sets the permission bits of the file `id` names to `mode`'s (`mode & 0o7777`; its
    /// file type bits are ignored), as `fchmod` does, and its change time to now, in one
    /// journal transaction.
    ///
    /// Following POSIX `chmod`: the caller (`uid`, `gid`) must own the file, or be UID 0.
    /// When a caller other than UID 0 is not in the file's group (`gid` is its one group)
    /// and the file is a regular one, the set-group-ID bit is cleared.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::ReadOnly`] as for [`Self::set_times`],
    /// [`Error::SymlinkMode`] for a symbolic link opened itself (its mode is not used),
    /// [`Error::NotOwner`], `Error::Lookup` when the file's inode cannot be found, and
    /// `Error::Write` when the change fails (see `write::change`).
    pub fn set_mode(&mut self, id: usize, mode: u16, uid: u32, gid: u32) -> Result<(), Error> {
        let handle = self.handle(id)?;
        #[cfg(not(feature = "write"))]
        {
            let _ = (handle, mode, uid, gid);
            Err(Error::ReadOnly)
        }
        #[cfg(feature = "write")]
        {
            use crate::write::{Change, Mode, Timestamp};

            self.writer.as_ref().ok_or(Error::ReadOnly)?;
            let metadata = &handle.metadata;
            if metadata.is_symlink() {
                return Err(Error::SymlinkMode);
            }
            if uid != 0 && metadata.uid() != uid {
                return Err(Error::NotOwner);
            }
            let mut permissions = mode & PERMISSION_BITS;
            if uid != 0 && metadata.gid() != gid && metadata.file_type() == FileType::Regular {
                permissions &= !S_ISGID;
            }
            let mode = Mode {
                permissions,
                changed: Timestamp::now(),
            };
            self.commit(id, &Change::Mode(mode))
        }
    }

    /// Sets the owner and group of the file `id` names, as `fchown` does: `None` keeps one
    /// (`-1` in the call). Its change time is set to now, in one journal transaction.
    ///
    /// Following POSIX `chown` with `_POSIX_CHOWN_RESTRICTED`: UID 0 may set any owner and
    /// group. Any other caller (`uid`, `gid`) must own the file, may not give it to another
    /// owner, and may set its group only to the one it has or to `gid`, the caller's group;
    /// the set-user-ID and set-group-ID bits of a regular file are then cleared. UID 0 keeps
    /// them (POSIX leaves that to the implementation). A symbolic link opened itself has its
    /// own owner and group set.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::ReadOnly`] as for [`Self::set_times`],
    /// [`Error::NotOwner`] and [`Error::ChownRestricted`] as above, `Error::Lookup` when the
    /// file's inode cannot be found, and `Error::Write` when the change fails (see
    /// `write::change`: a new owner or group on a volume with `quota` is refused).
    pub fn set_owner(
        &mut self,
        id: usize,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        uid: u32,
        gid: u32,
    ) -> Result<(), Error> {
        let handle = self.handle(id)?;
        #[cfg(not(feature = "write"))]
        {
            let _ = (handle, new_uid, new_gid, uid, gid);
            Err(Error::ReadOnly)
        }
        #[cfg(feature = "write")]
        {
            use crate::write::{Change, Owner, Timestamp};

            self.writer.as_ref().ok_or(Error::ReadOnly)?;
            let metadata = &handle.metadata;
            if uid != 0 {
                if metadata.uid() != uid {
                    return Err(Error::NotOwner);
                }
                let gives_away = new_uid.is_some_and(|new| new != metadata.uid());
                let foreign_group = new_gid.is_some_and(|new| new != metadata.gid() && new != gid);
                if gives_away || foreign_group {
                    return Err(Error::ChownRestricted);
                }
            }
            let owner = Owner {
                uid: new_uid,
                gid: new_gid,
                drop_set_id: uid != 0,
                changed: Timestamp::now(),
            };
            self.commit(id, &Change::Owner(owner))
        }
    }

    /// Makes `change` to the inode of the file `id` names, then loads the volume again.
    #[cfg(feature = "write")]
    fn commit(&mut self, id: usize, change: &crate::write::Change) -> Result<(), Error> {
        let handle = self.handle(id)?;
        let writer = self.writer.as_ref().ok_or(Error::ReadOnly)?;
        let number = match handle.inode {
            Some(number) => number,
            None => self.device.inode_of(&handle.path)?,
        };
        let result = writer.change(number, change);
        // Even a failed change may have written part of the transaction.
        self.reload();
        Ok(result?)
    }

    /// Loads `ext4-view`'s reader again after a change, as it keeps the blocks it has read,
    /// and reads every handle's metadata again from it. If the volume cannot be loaded, the
    /// old reader is kept, and the change is still on the disk.
    #[cfg(feature = "write")]
    fn reload(&mut self) {
        match Ext4::load(self.device.reader()) {
            Ok(ext4) => {
                for handle in self.handles.values_mut() {
                    if let Ok(metadata) = ext4.symlink_metadata(handle.path.as_str()) {
                        handle.metadata = metadata;
                    }
                }
                self.ext4 = ext4;
            }
            Err(err) => log::warn!("cannot load the volume again after a change: {err}"),
        }
    }

    fn resolve(
        &self,
        start: &str,
        path: &str,
        mode: OpenMode,
        uid: u32,
        gid: u32,
    ) -> Result<(String, Metadata), Error> {
        if path.len() > 4096 {
            return Err(ext4_view::Ext4Error::PathTooLong.into());
        }
        let mut current = start.to_owned();
        let mut pending: VecDeque<String> = path.split('/').map(str::to_owned).collect();
        let mut links = 0;
        while let Some(component) = pending.pop_front() {
            let directory = self.ext4.symlink_metadata(current.as_str())?;
            if !directory.is_dir() {
                return Err(Error::NotADirectory);
            }
            require_access(&directory, uid, gid, 0o1)?;
            match component.as_str() {
                "" | "." => continue,
                ".." => {
                    current = normalize(&current, "..");
                    continue;
                }
                _ => {}
            }
            let next = normalize(&current, &component);
            let metadata = self.ext4.symlink_metadata(next.as_str())?;
            if metadata.is_symlink() {
                let final_link = pending.is_empty();
                if final_link && (mode.symlink || mode.no_follow) {
                    if mode.symlink || mode.stat {
                        return Ok((next, metadata));
                    }
                    return Err(Error::SymlinkNotFollowed);
                }
                links += 1;
                if links > 40 {
                    return Err(ext4_view::Ext4Error::TooManySymlinks.into());
                }
                let target = self.ext4.read_link(next.as_str())?;
                let target = std::str::from_utf8(target.as_ref())
                    .map_err(|_| ext4_view::Ext4Error::NotUtf8)?;
                if target.is_empty() {
                    return Err(ext4_view::Ext4Error::NotFound.into());
                }
                if target.starts_with('/') {
                    current = "/".to_owned();
                }
                for part in target.split('/').rev() {
                    pending.push_front(part.to_owned());
                }
            } else {
                current = next;
            }
            if current.len() + pending.iter().map(|s| s.len() + 1).sum::<usize>() > 4096 {
                return Err(ext4_view::Ext4Error::PathTooLong.into());
            }
        }
        let metadata = self.ext4.symlink_metadata(current.as_str())?;
        Ok((current, metadata))
    }

    /// The inode `path` names, for [`Handle::inode`]. When it cannot be found (a path through
    /// a directory kind `crate::lookup` does not read, such as an inline-data one), the
    /// handle does without, and reports its modification time as its change time.
    fn inode_of(&self, path: &str) -> Option<u32> {
        self.device
            .inode_of(path)
            .inspect_err(|err| log::debug!("{path}: no inode number: {err}"))
            .ok()
    }

    /// The times of the inode `handle` names, if it is known and can be read.
    fn inode_times(&self, handle: &Handle) -> Option<crate::lookup::InodeTimes> {
        let number = handle.inode?;
        self.device
            .times(number)
            .inspect_err(|err| log::warn!("{}: cannot read the inode's times: {err}", handle.path))
            .ok()
    }

    fn handle(&self, id: usize) -> Result<&Handle, Error> {
        self.handles.get(&id).ok_or(Error::BadHandle { handle: id })
    }

    fn handle_mut(&mut self, id: usize) -> Result<&mut Handle, Error> {
        self.handles
            .get_mut(&id)
            .ok_or(Error::BadHandle { handle: id })
    }
}

/// The volume superblock's bytes.
fn read_superblock<R: crate::device::ReadAt>(
    device: &crate::device::Window<R>,
) -> Result<[u8; superblock::SIZE], Error> {
    let mut sb = [0; superblock::SIZE];
    device
        .read_exact_at(&mut sb, superblock::OFFSET)
        .map_err(Error::Device)?;
    Ok(sb)
}

/// Apply one inode's owner, primary group or other permission class.
fn require_access(metadata: &Metadata, uid: u32, gid: u32, operation: u16) -> Result<(), Error> {
    let shift = if metadata.uid() == uid {
        6
    } else if metadata.gid() == gid {
        3
    } else {
        0
    };
    if uid == 0 || (metadata.mode() >> shift) & operation == operation {
        Ok(())
    } else {
        Err(Error::PermissionDenied)
    }
}

fn read_entries(ext4: &Ext4, path: &str) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    for entry in ext4.read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.as_ref();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        // Scheme directory entries are UTF-8; other names are shown lossily, and cannot be
        // opened by that name.
        let name = String::from_utf8_lossy(bytes).into_owned();
        let ino = path_ino(&normalize(path, &name));
        entries.push(Entry {
            kind: entry.file_type()?.into(),
            name,
            ino,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_as_text() {
        assert_eq!(normalize("/", ""), "/");
        assert_eq!(normalize("/", "a/b"), "/a/b");
        assert_eq!(normalize("/a", "b/../c"), "/a/c");
        assert_eq!(normalize("/a/b", "/x/./y"), "/x/y");
        assert_eq!(normalize("/", "../../.."), "/");
        assert_eq!(normalize("/a", "..//b/"), "/b");
    }

    #[test]
    fn futimens_times_are_read() {
        let read = |seconds, nanoseconds| TimeChange::from_timespec(seconds, nanoseconds).ok();
        assert_eq!(read(5, UTIME_NOW), Some(TimeChange::Now));
        assert_eq!(read(5, UTIME_OMIT), Some(TimeChange::Omit));
        assert_eq!(
            read(-5, 999_999_999),
            Some(TimeChange::At {
                seconds: -5,
                nanoseconds: 999_999_999
            })
        );
        for bad in [-1, 1_000_000_000, UTIME_NOW + 1, i64::MIN] {
            assert!(matches!(
                TimeChange::from_timespec(0, bad),
                Err(Error::InvalidTime)
            ));
        }
    }

    #[test]
    fn flags_read_as_modes() {
        use syscall::flag::{O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_STAT, O_TRUNC, O_WRONLY};
        assert!(!OpenMode::from_flags(O_RDONLY).wants_write());
        assert!(!OpenMode::from_flags(O_STAT).wants_write());
        assert!(!OpenMode::from_flags(O_RDONLY | O_DIRECTORY).wants_write());
        assert!(OpenMode::from_flags(O_RDONLY | O_DIRECTORY).directory);
        assert!(OpenMode::from_flags(O_WRONLY).wants_write());
        assert!(OpenMode::from_flags(O_RDWR).wants_write());
        assert!(OpenMode::from_flags(O_RDONLY | O_CREAT).wants_write());
        assert!(OpenMode::from_flags(O_RDONLY | O_TRUNC).wants_write());
    }

    #[test]
    fn attributes_become_a_stat() {
        let a = Attributes {
            ino: 7,
            mode: 0o100_644,
            kind: Kind::File,
            uid: 1,
            gid: 2,
            size: 1025,
            block_size: 4096,
            accessed: (10, 1),
            modified: (-5, 3),
            changed: (20, 4),
        };
        let st = a.to_stat();
        assert_eq!(
            (st.st_ino, st.st_mode, st.st_uid, st.st_gid),
            (7, 0o100_644, 1, 2)
        );
        assert_eq!((st.st_size, st.st_blocks, st.st_blksize), (1025, 3, 4096));
        assert_eq!((st.st_atime, st.st_atime_nsec), (10, 1));
        assert_eq!((st.st_mtime, st.st_mtime_nsec), (0, 3));
        assert_eq!((st.st_ctime, st.st_ctime_nsec), (20, 4));
    }

    #[test]
    fn kinds_have_mode_bits_and_dirent_kinds() {
        use syscall::dirent::DirentKind;
        assert_eq!(Kind::Directory.mode_bits(), syscall::flag::MODE_DIR);
        assert_eq!(Kind::File.mode_bits(), syscall::flag::MODE_FILE);
        assert_eq!(Kind::Symlink.mode_bits(), syscall::flag::MODE_SYMLINK);
        assert_eq!(
            Kind::Directory.dirent_kind() as u8,
            DirentKind::Directory as u8
        );
        assert_eq!(
            Kind::Fifo.dirent_kind() as u8,
            DirentKind::Unspecified as u8
        );
    }

    #[test]
    fn inode_numbers_are_stable_and_clear_of_reserved_ones() {
        assert_eq!(path_ino("/"), 2);
        assert_eq!(path_ino("/a"), path_ino("/a"));
        assert_ne!(path_ino("/a"), path_ino("/b"));
        assert!(path_ino("/a") > 1 << 32);
    }
}
