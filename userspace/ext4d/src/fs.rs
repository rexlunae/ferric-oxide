//! The open-file table: what a scheme's `openat`, `read`, `getdents`, `fstat` and `close`
//! do, on an ext4 volume read by `ext4-view`.
//!
//! Handles are numbers the scheme gives its clients. Each names an open file or directory
//! with its path from the volume's root, so relative opens and `fpath` work. Paths are
//! normalized lexically (`.` and `..` resolved as text), as RedoxFS does. Symbolic links are
//! followed by the reader, except where `O_SYMLINK` asks for the link itself.
//!
//! Nothing here changes the volume. [`OpenMode::wants_write`] opens and every changing call
//! fail with [`Error::ReadOnly`].
//!
//! Limitation: `ext4-view` does not expose inode numbers. [`Attributes::ino`] is a stable
//! number made from the path instead, so hard links to one inode get different numbers.

use std::collections::BTreeMap;

use ext4_view::{Ext4, Ext4Read, File, FileType, Metadata};

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
        }
    }

    /// Reads the mode from Redox `openat` flags.
    pub const fn from_flags(flags: usize) -> Self {
        use syscall::flag::{
            O_ACCMODE, O_CREAT, O_DIRECTORY, O_NOFOLLOW, O_RDONLY, O_SYMLINK, O_TRUNC,
        };
        let access = flags & O_ACCMODE;
        Self {
            write: access != O_RDONLY && access != 0,
            create_or_truncate: flags & (O_CREAT | O_TRUNC) != 0,
            directory: flags & O_DIRECTORY != 0,
            symlink: flags & O_SYMLINK != 0,
            no_follow: flags & O_NOFOLLOW != 0,
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
            // `ext4-view` does not expose the change time; the last change to the contents
            // is the closest it has.
            st_ctime: secs(self.modified),
            st_ctime_nsec: self.modified.1,
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
    File(Box<File>),
    Directory { entries: Option<Vec<Entry>> },
    Symlink { target: Vec<u8> },
}

struct Handle {
    /// The path from the volume's root, normalized, starting with `/`.
    path: String,
    metadata: Metadata,
    open: Open,
}

/// A mounted ext4 volume and its open handles.
pub struct Fs {
    ext4: Ext4,
    geometry: Geometry,
    handles: BTreeMap<usize, Handle>,
    next: usize,
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
        let mut sb = [0; superblock::SIZE];
        device
            .read_exact_at(&mut sb, superblock::OFFSET)
            .map_err(Error::Device)?;
        let geometry = Geometry::parse(&sb)?;
        let ext4 = Ext4::load(Box::new(device) as Box<dyn Ext4Read>)?;
        Ok(Self {
            ext4,
            geometry,
            handles: BTreeMap::new(),
            next: 1,
        })
    }

    /// The volume's size and free space.
    pub const fn geometry(&self) -> Geometry {
        self.geometry
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
    /// [`Error::Ext4`] when the path cannot be resolved or read.
    pub fn open(
        &mut self,
        base: Option<usize>,
        path: &str,
        mode: OpenMode,
    ) -> Result<usize, Error> {
        if mode.wants_write() {
            return Err(Error::ReadOnly);
        }
        let start = match base {
            Some(base) if !path.starts_with('/') => self.handle(base)?.path.as_str(),
            _ => "/",
        };
        let path = normalize(start, path);
        let link = self.ext4.symlink_metadata(path.as_str())?;
        let handle = if link.is_symlink() && (mode.symlink || mode.no_follow) {
            if !mode.symlink {
                return Err(Error::SymlinkNotFollowed);
            }
            let target = self.ext4.read_link(path.as_str())?;
            let target = target.as_ref().to_vec();
            Handle {
                path,
                metadata: link,
                open: Open::Symlink { target },
            }
        } else {
            let metadata = if link.is_symlink() {
                self.ext4.metadata(path.as_str())?
            } else {
                link
            };
            if mode.directory && !metadata.is_dir() {
                return Err(Error::NotADirectory);
            }
            let open = if metadata.is_dir() {
                Open::Directory { entries: None }
            } else {
                Open::File(Box::new(self.ext4.open(path.as_str())?))
            };
            Handle {
                path,
                metadata,
                open,
            }
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

    /// The attributes of the file `id` names.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`].
    pub fn attributes(&self, id: usize) -> Result<Attributes, Error> {
        let handle = self.handle(id)?;
        let m = &handle.metadata;
        let time = |t: ext4_view::Timestamp| (t.seconds(), t.nanoseconds());
        let kind = Kind::from(m.file_type());
        Ok(Attributes {
            ino: path_ino(&handle.path),
            mode: kind.mode_bits() | m.mode(),
            kind,
            uid: m.uid(),
            gid: m.gid(),
            size: m.len(),
            block_size: self.geometry.block_size,
            accessed: time(m.accessed()),
            modified: time(m.modified()),
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

    fn handle(&self, id: usize) -> Result<&Handle, Error> {
        self.handles.get(&id).ok_or(Error::BadHandle { handle: id })
    }

    fn handle_mut(&mut self, id: usize) -> Result<&mut Handle, Error> {
        self.handles
            .get_mut(&id)
            .ok_or(Error::BadHandle { handle: id })
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

/// `path` resolved against the directory `start` (both from the root), with `.` and `..`
/// resolved as text and no empty components. The result starts with `/`; `..` at the root
/// stays at the root.
pub fn normalize(start: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let full = if path.starts_with('/') {
        [path, ""]
    } else {
        [start, path]
    };
    for component in full.iter().flat_map(|p| p.split('/')) {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

/// A stable number for the file at `path`: 2 for the root, as in ext4, and a 64-bit FNV-1a
/// hash of the path, kept clear of the small numbers ext4 reserves, for the rest.
fn path_ino(path: &str) -> u64 {
    if path == "/" {
        return 2;
    }
    let hash = path.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    hash | 1 << 63
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
        };
        let st = a.to_stat();
        assert_eq!(
            (st.st_ino, st.st_mode, st.st_uid, st.st_gid),
            (7, 0o100_644, 1, 2)
        );
        assert_eq!((st.st_size, st.st_blocks, st.st_blksize), (1025, 3, 4096));
        assert_eq!((st.st_atime, st.st_atime_nsec), (10, 1));
        assert_eq!((st.st_mtime, st.st_mtime_nsec), (0, 3));
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
