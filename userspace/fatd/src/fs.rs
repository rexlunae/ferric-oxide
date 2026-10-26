//! The open-file table: what a scheme's `openat`, `read`, `getdents`, `fstat` and `close`
//! do, on a FAT volume read by `fatfs`.
//!
//! Handles are numbers the scheme gives its clients. Each names an open file or directory
//! with its path from the volume's root, so relative opens and `fpath` work. A path is
//! resolved one component at a time, in order: each name is looked up in its directory the
//! way the FAT specification asks, ignoring case and matching either the long name or the
//! 8.3 short name, and the handle keeps the names as the directory stores them. So
//! `/efi/boot/bootx64.efi` opens `/EFI/BOOT/BOOTX64.EFI`, and both get the same inode number.
//!
//! FAT records no owner and no permissions. Every file is presented as owned by root and
//! readable by everyone (`0444`), every directory as searchable by everyone (`0555`), and no
//! caller is refused a read. Nothing here changes the volume: [`OpenMode::wants_write`] opens
//! and every changing call fail with [`Error::ReadOnly`].
//!
//! Limitations: FAT has no inode numbers, so [`Attributes::ino`] is made from the path
//! ([`ferric_fspath::path_ino`]); and FAT times carry no time zone, so they are read as UTC
//! (see [`crate::time`]).

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};

use ferric_fspath::{normalize, path_ino};

use crate::Error;
use crate::disk::{Device, Disk};
use crate::time::{self, UnixTime};

/// A mounted FAT volume on the device `D`: the `fatfs` file system the handles borrow.
pub type Volume<D> = fatfs::FileSystem<Disk<D>>;

type Dir<'v, D> = fatfs::Dir<'v, Disk<D>>;
type DirEntry<'v, D> = fatfs::DirEntry<'v, Disk<D>>;
type File<'v, D> = fatfs::File<'v, Disk<D>>;

/// The longest path, in bytes, that an open resolves.
pub const PATH_MAX: usize = 4096;

/// The mode bits of every directory: searchable and listable by everyone.
const DIRECTORY_MODE: u16 = 0o555;
/// The mode bits of every file: readable by everyone.
const FILE_MODE: u16 = 0o444;

/// Reads the FAT boot sector (and, on FAT32, the FSInfo sector) of `device` and mounts the
/// volume, read-only.
///
/// # Errors
///
/// [`Error::Mount`] when `device` does not start with a FAT boot sector `fatfs` accepts, or
/// cannot be read.
pub fn mount<D: Device>(device: D) -> Result<Volume<D>, Error> {
    // The default options leave access dates alone, so reading a file writes nothing.
    fatfs::FileSystem::new(Disk::new(device), fatfs::FsOptions::new()).map_err(Error::Mount)
}

/// What an `openat` asks for, from its flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenMode {
    /// Write access (`O_WRONLY` or `O_RDWR`).
    pub write: bool,
    /// Creation or truncation (`O_CREAT`, `O_TRUNC`).
    pub create_or_truncate: bool,
    /// The path must be a directory (`O_DIRECTORY`).
    pub directory: bool,
    /// Open a symbolic link itself (`O_SYMLINK`); FAT has none, so this always fails.
    pub symlink: bool,
    /// Open metadata only (`O_STAT`).
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
            stat: false,
        }
    }

    /// Reads the mode from Redox `openat` flags.
    pub const fn from_flags(flags: usize) -> Self {
        use syscall::flag::{
            O_ACCMODE, O_CREAT, O_DIRECTORY, O_RDONLY, O_STAT, O_SYMLINK, O_TRUNC,
        };
        let access = flags & O_ACCMODE;
        Self {
            write: access != O_RDONLY && access != 0,
            create_or_truncate: flags & (O_CREAT | O_TRUNC) != 0,
            directory: flags & O_DIRECTORY != 0,
            symlink: flags & O_SYMLINK != 0,
            stat: flags & O_STAT != 0,
        }
    }

    /// Whether the open would change the volume or allow changing it.
    pub const fn wants_write(&self) -> bool {
        self.write || self.create_or_truncate
    }
}

/// The kind of file a handle or directory entry names: FAT has only these two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

impl Kind {
    /// The kind of a directory entry: a directory when its `ATTR_DIRECTORY` bit is set.
    fn of<D: Device>(entry: &DirEntry<'_, D>) -> Self {
        if entry.is_dir() {
            Self::Directory
        } else {
            Self::File
        }
    }

    /// The file type and permission bits of `st_mode` for this kind (see the module docs).
    pub const fn mode(self) -> u16 {
        match self {
            Self::File => syscall::flag::MODE_FILE | FILE_MODE,
            Self::Directory => syscall::flag::MODE_DIR | DIRECTORY_MODE,
        }
    }

    /// The kind as a scheme directory entry's kind.
    pub const fn dirent_kind(self) -> syscall::dirent::DirentKind {
        match self {
            Self::File => syscall::dirent::DirentKind::Regular,
            Self::Directory => syscall::dirent::DirentKind::Directory,
        }
    }
}

/// A file's attributes, for `fstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes {
    /// A stable number for the file (see the module docs).
    pub ino: u64,
    /// The kind of file.
    pub kind: Kind,
    /// The size in bytes; 0 for a directory, as FAT records.
    pub size: u64,
    /// The volume's cluster size, the unit it allocates in.
    pub cluster_size: u32,
    /// The last access: FAT keeps only the date, so this is its start.
    pub accessed: Option<UnixTime>,
    /// The last write.
    pub modified: Option<UnixTime>,
}

impl Attributes {
    /// The attributes of the root directory, which has no directory entry and so no times.
    fn root(cluster_size: u32) -> Self {
        Self {
            ino: path_ino("/"),
            kind: Kind::Directory,
            size: 0,
            cluster_size,
            accessed: None,
            modified: None,
        }
    }

    fn of<D: Device>(entry: &DirEntry<'_, D>, path: &str, cluster_size: u32) -> Self {
        Self {
            ino: path_ino(path),
            kind: Kind::of(entry),
            size: entry.len(),
            cluster_size,
            accessed: time::date(entry.accessed()),
            modified: time::date_time(entry.modified()),
        }
    }

    /// The attributes as the scheme ABI's `Stat`. Owner and group are root, `st_blocks`
    /// counts 512-byte units of the size, and missing times read as 1970. FAT has no change
    /// time; the last write stands in for it.
    pub fn to_stat(&self) -> syscall::data::Stat {
        let (mtime, mtime_nsec) = self.modified.unwrap_or_default();
        let (atime, atime_nsec) = self.accessed.unwrap_or_default();
        let secs = |t: i64| u64::try_from(t).unwrap_or(0);
        syscall::data::Stat {
            st_dev: 0,
            st_ino: self.ino,
            st_mode: self.kind.mode(),
            st_nlink: 1,
            st_uid: 0,
            st_gid: 0,
            st_size: self.size,
            st_blksize: self.cluster_size,
            st_blocks: self.size.div_ceil(512),
            st_mtime: secs(mtime),
            st_mtime_nsec: mtime_nsec,
            st_atime: secs(atime),
            st_atime_nsec: atime_nsec,
            st_ctime: secs(mtime),
            st_ctime_nsec: mtime_nsec,
        }
    }
}

/// The volume's size and free space, in clusters, for `statvfs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The cluster size in bytes.
    pub cluster_size: u32,
    /// The clusters of the data region.
    pub clusters: u64,
    /// The clusters the allocation table marks free.
    pub free_clusters: u64,
}

impl Geometry {
    /// The geometry as the scheme ABI's `StatVfs`. FAT reserves nothing for the superuser,
    /// so every free cluster is available.
    pub const fn to_statvfs(&self) -> syscall::data::StatVfs {
        syscall::data::StatVfs {
            f_bsize: self.cluster_size,
            f_blocks: self.clusters,
            f_bfree: self.free_clusters,
            f_bavail: self.free_clusters,
        }
    }
}

/// One directory entry, for `getdents`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry's name: its long name, or its 8.3 short name when it has none.
    pub name: String,
    /// The kind of file it names.
    pub kind: Kind,
    /// A stable number for the file, as in [`Attributes::ino`].
    pub ino: u64,
}

enum Open<'v, D: Device> {
    Metadata,
    File(File<'v, D>),
    Directory {
        dir: Dir<'v, D>,
        entries: Option<Vec<Entry>>,
    },
}

struct Handle<'v, D: Device> {
    /// The path from the volume's root, as the directories store its names, starting with
    /// `/`.
    path: String,
    attributes: Attributes,
    open: Open<'v, D>,
}

/// What a path names: the root directory, which has no entry, or a directory entry.
enum Found<'v, D: Device> {
    Root,
    Entry(DirEntry<'v, D>),
}

/// The open handles of a mounted FAT volume.
pub struct Fs<'v, D: Device> {
    volume: &'v Volume<D>,
    cluster_size: u32,
    handles: BTreeMap<usize, Handle<'v, D>>,
    next: usize,
}

impl<'v, D: Device> Fs<'v, D> {
    /// The handle table of `volume`, with nothing open.
    pub fn new(volume: &'v Volume<D>) -> Self {
        Self {
            volume,
            cluster_size: volume.cluster_size(),
            handles: BTreeMap::new(),
            next: 1,
        }
    }

    /// The volume's label from its boot sector, without the padding; empty when it has none.
    pub fn label(&self) -> String {
        let label = self.volume.volume_label();
        let label = label.trim_end_matches(' ');
        if label == "NO NAME" {
            String::new()
        } else {
            label.to_owned()
        }
    }

    /// Which FAT the volume uses: 12, 16 or 32, chosen by its cluster count as the
    /// specification's "FAT Type Determination" section says.
    pub fn fat_bits(&self) -> u8 {
        match self.volume.fat_type() {
            fatfs::FatType::Fat12 => 12,
            fatfs::FatType::Fat16 => 16,
            fatfs::FatType::Fat32 => 32,
        }
    }

    /// The volume's size and free space. On FAT12 and FAT16, and on FAT32 without a
    /// trustworthy FSInfo sector, this reads the whole allocation table.
    ///
    /// # Errors
    ///
    /// [`Error::Read`] when the allocation table cannot be read.
    pub fn geometry(&self) -> Result<Geometry, Error> {
        let stats = self.volume.stats().map_err(Error::Read)?;
        Ok(Geometry {
            cluster_size: stats.cluster_size(),
            clusters: u64::from(stats.total_clusters()),
            free_clusters: u64::from(stats.free_clusters()),
        })
    }

    /// Opens the root directory, as the scheme's root handle.
    ///
    /// # Errors
    ///
    /// None in practice; the signature matches [`Self::open`].
    pub fn root(&mut self) -> Result<usize, Error> {
        self.open(None, "/", OpenMode::read())
    }

    /// Opens `path`: from the root when it is absolute or `base` is `None`, otherwise from
    /// the directory `base` names. Returns the new handle.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] for any open that could change the volume, [`Error::BadHandle`]
    /// for an unknown `base`, [`Error::NotADirectory`] when `base` or a component on the way
    /// is not a directory or `mode.directory` is set and the path is not one,
    /// [`Error::NotASymlink`] for `O_SYMLINK`, [`Error::PathTooLong`], [`Error::NotFound`],
    /// and [`Error::Read`] when a directory cannot be read.
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
            Some(base) if !path.starts_with('/') => {
                let handle = self.handle(base)?;
                if handle.attributes.kind != Kind::Directory {
                    return Err(Error::NotADirectory);
                }
                handle.path.as_str()
            }
            _ => "/",
        };
        // The path as resolved, base included: a short name relative to a deep directory
        // can still be too long.
        let combined = if path.starts_with('/') {
            path.len()
        } else {
            start.len() + 1 + path.len()
        };
        if combined > PATH_MAX {
            return Err(Error::PathTooLong);
        }
        let (path, found) = self.resolve(start, path)?;
        let attributes = match &found {
            Found::Root => Attributes::root(self.cluster_size),
            Found::Entry(entry) => Attributes::of(entry, &path, self.cluster_size),
        };
        if mode.symlink {
            return Err(Error::NotASymlink);
        }
        if mode.directory && attributes.kind != Kind::Directory {
            return Err(Error::NotADirectory);
        }
        let open = match found {
            _ if mode.stat => Open::Metadata,
            Found::Root => Open::Directory {
                dir: self.volume.root_dir(),
                entries: None,
            },
            Found::Entry(entry) if entry.is_dir() => Open::Directory {
                dir: entry.to_dir(),
                entries: None,
            },
            Found::Entry(entry) => Open::File(entry.to_file()),
        };
        let id = self.next;
        self.next += 1;
        self.handles.insert(
            id,
            Handle {
                path,
                attributes,
                open,
            },
        );
        Ok(id)
    }

    /// Closes `id`. Unknown handles are ignored, as a close has no answer.
    pub fn close(&mut self, id: usize) {
        self.handles.remove(&id);
    }

    /// Reads into `buf` from byte `offset` of the file `id` names. Returns how many bytes
    /// were read: fewer than `buf.len()` only at the end of the file.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`] (also for a handle opened with `O_STAT`), [`Error::IsADirectory`]
    /// for a directory, and [`Error::Read`] when the file's clusters cannot be read.
    pub fn read(&mut self, id: usize, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let handle = self.handle_mut(id)?;
        let size = handle.attributes.size;
        let file = match &mut handle.open {
            Open::Metadata => return Err(Error::BadHandle { handle: id }),
            Open::Directory { .. } => return Err(Error::IsADirectory),
            Open::File(file) => file,
        };
        // `fatfs` follows the cluster chain from the start on a seek to another cluster,
        // and not at all for the position the last read left, so sequential reads stay
        // cheap.
        file.seek(SeekFrom::Start(offset.min(size)))
            .map_err(Error::Read)?;
        let mut done = 0;
        while done < buf.len() {
            let n = file.read(&mut buf[done..]).map_err(Error::Read)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    /// The entries of the directory `id` names, without `.` and `..` and the volume label,
    /// in the order the directory stores them. The list is read on the first call and kept,
    /// so offsets into it stay valid while the handle is open.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::NotADirectory`] for anything else, and [`Error::Read`]
    /// when the directory cannot be read.
    pub fn entries(&mut self, id: usize) -> Result<&[Entry], Error> {
        let handle = self
            .handles
            .get_mut(&id)
            .ok_or(Error::BadHandle { handle: id })?;
        let Open::Directory { dir, entries } = &mut handle.open else {
            return Err(Error::NotADirectory);
        };
        if entries.is_none() {
            *entries = Some(read_entries(dir, &handle.path)?);
        }
        Ok(entries.as_deref().unwrap_or_default())
    }

    /// The attributes of the file `id` names.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`].
    pub fn attributes(&self, id: usize) -> Result<Attributes, Error> {
        Ok(self.handle(id)?.attributes)
    }

    /// The path of the file `id` names, from the volume's root, starting with `/`, with the
    /// names as the directories store them.
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

    /// Resolves `path` from the directory `start` (from the root when `path` is absolute),
    /// one component at a time and in order, as POSIX does: each name is looked up in the
    /// directory reached so far, `..` goes back to that directory's parent, and any component
    /// after a file (`..` and `.` included) fails with [`Error::NotADirectory`]. So
    /// `/hello.txt/../EFI` is `ENOTDIR` and `/missing/../EFI` is `ENOENT`, not `/EFI`.
    /// Returns the path with the names as stored, and what it names.
    fn resolve(&self, start: &str, path: &str) -> Result<(String, Found<'v, D>), Error> {
        let start = if path.starts_with('/') { "" } else { start };
        let components = start.split('/').chain(path.split('/'));
        // The entries from the root to where the walk is; empty at the root.
        let mut walked: Vec<(String, DirEntry<'v, D>)> = Vec::new();
        for component in components {
            let dir = match walked.last() {
                None => self.volume.root_dir(),
                Some((_, entry)) if entry.is_dir() => entry.to_dir(),
                Some(_) => return Err(Error::NotADirectory),
            };
            match component {
                "" | "." => {}
                ".." => {
                    walked.pop();
                }
                name => {
                    let entry = lookup(&dir, name)?;
                    walked.push((entry.file_name(), entry));
                }
            }
        }
        let mut stored = String::new();
        for (name, _) in &walked {
            stored.push('/');
            stored.push_str(name);
        }
        if stored.is_empty() {
            stored.push('/');
        }
        let found = walked
            .pop()
            .map_or(Found::Root, |(_, entry)| Found::Entry(entry));
        Ok((stored, found))
    }

    fn handle(&self, id: usize) -> Result<&Handle<'v, D>, Error> {
        self.handles.get(&id).ok_or(Error::BadHandle { handle: id })
    }

    fn handle_mut(&mut self, id: usize) -> Result<&mut Handle<'v, D>, Error> {
        self.handles
            .get_mut(&id)
            .ok_or(Error::BadHandle { handle: id })
    }
}

/// Whether two names are equal ignoring case, as FAT compares names: by their upper-case
/// forms, character by character.
fn same_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_uppercase)
        .eq(b.chars().flat_map(char::to_uppercase))
}

/// Whether `entry` is the `.` or `..` entry every directory but the root starts with.
fn is_dot<D: Device>(entry: &DirEntry<'_, D>) -> bool {
    matches!(entry.short_file_name_as_bytes(), b"." | b"..")
}

/// The entry of `dir` named `name`, by its long name or its short name, ignoring case.
fn lookup<'v, D: Device>(dir: &Dir<'v, D>, name: &str) -> Result<DirEntry<'v, D>, Error> {
    for entry in dir.iter() {
        let entry = entry.map_err(Error::Read)?;
        if is_dot(&entry) {
            continue;
        }
        if same_name(&entry.file_name(), name) || same_name(&entry.short_file_name(), name) {
            return Ok(entry);
        }
    }
    Err(Error::NotFound)
}

fn read_entries<D: Device>(dir: &Dir<'_, D>, path: &str) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    for entry in dir.iter() {
        let entry = entry.map_err(Error::Read)?;
        if is_dot(&entry) {
            continue;
        }
        let name = entry.file_name();
        entries.push(Entry {
            ino: path_ino(&normalize(path, &name)),
            kind: Kind::of(&entry),
            name,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compare_without_case() {
        assert!(same_name("BOOTX64.EFI", "bootx64.efi"));
        assert!(same_name("Ünïcode.txt", "üNÏCODE.TXT"));
        assert!(!same_name("a.txt", "a.txt2"));
    }

    #[test]
    fn flags_read_as_modes() {
        use syscall::flag::{O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_STAT, O_TRUNC, O_WRONLY};
        assert!(!OpenMode::from_flags(O_RDONLY).wants_write());
        assert!(!OpenMode::from_flags(O_STAT).wants_write());
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
            kind: Kind::File,
            size: 1025,
            cluster_size: 2048,
            accessed: Some((86_400, 0)),
            modified: Some((90_000, 500_000_000)),
        };
        let st = a.to_stat();
        assert_eq!((st.st_ino, st.st_mode), (7, 0o100_444));
        assert_eq!((st.st_uid, st.st_gid, st.st_nlink), (0, 0, 1));
        assert_eq!((st.st_size, st.st_blocks, st.st_blksize), (1025, 3, 2048));
        assert_eq!((st.st_atime, st.st_atime_nsec), (86_400, 0));
        assert_eq!((st.st_mtime, st.st_mtime_nsec), (90_000, 500_000_000));
        assert_eq!(st.st_ctime, 90_000);
        let root = Attributes::root(512).to_stat();
        assert_eq!(
            (root.st_ino, root.st_mode, root.st_mtime),
            (2, 0o040_555, 0)
        );
    }

    #[test]
    fn geometry_becomes_a_statvfs() {
        let g = Geometry {
            cluster_size: 4096,
            clusters: 100,
            free_clusters: 40,
        };
        let vfs = g.to_statvfs();
        assert_eq!(
            (vfs.f_bsize, vfs.f_blocks, vfs.f_bfree, vfs.f_bavail),
            (4096, 100, 40, 40)
        );
    }
}
