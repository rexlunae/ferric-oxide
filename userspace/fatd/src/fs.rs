//! The open-file table: what a scheme's `openat`, `read`, `getdents`, `fstat` and `close`
//! do, on a FAT volume read by `fatfs` or, with the `exfat` feature, an exFAT volume read by
//! `ferric-exfat`. [`mount`] tells the two apart by the boot sector's file system name.
//!
//! Handles are numbers the scheme gives its clients. Each names an open file or directory
//! with its path from the volume's root, so relative opens and `fpath` work. A path is
//! resolved one component at a time, in order: each name is looked up in its directory the
//! way the format asks, ignoring case (on FAT matching either the long name or the 8.3
//! short name; on exFAT comparing through the volume's up-case table), and the handle keeps
//! the names as the directory stores them. So `/efi/boot/bootx64.efi` opens
//! `/EFI/BOOT/BOOTX64.EFI`, and both get the same inode number.
//!
//! Neither format records an owner or permissions. Every file is presented as owned by root and
//! readable by everyone (`0444`), every directory as searchable by everyone (`0555`), and no
//! caller is refused a read. Nothing here changes the volume: [`OpenMode::wants_write`] opens
//! and every changing call fail with [`Error::ReadOnly`].
//!
//! Limitations: neither format has inode numbers, so [`Attributes::ino`] is made from the
//! path ([`ferric_fspath::path_ino`]); and FAT times carry no time zone, so they are read as
//! UTC (see [`crate::time`]).

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};

use ferric_fspath::{normalize, path_ino};

use crate::Error;
use crate::disk::{Device, Disk};
use crate::time::{self, UnixTime};

/// A mounted volume on the device `D`, which the handles borrow.
#[non_exhaustive]
#[cfg_attr(
    feature = "exfat",
    expect(
        clippy::large_enum_variant,
        reason = "a daemon mounts one volume, once, and never moves it"
    )
)]
pub enum Volume<D: Device> {
    /// FAT12, FAT16 or FAT32, read by `fatfs`.
    Fat(fatfs::FileSystem<Disk<D>>),
    /// exFAT, read by `ferric-exfat` (feature `exfat`, features.toml `fs.exfat`).
    #[cfg(feature = "exfat")]
    ExFat(ferric_exfat::ExFat<D>),
}

/// The format of a mounted volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// FAT12.
    Fat12,
    /// FAT16.
    Fat16,
    /// FAT32.
    Fat32,
    /// exFAT.
    ExFat,
}

impl Format {
    /// The format's usual name: `FAT12`, `FAT16`, `FAT32` or `exFAT`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
            Self::ExFat => "exFAT",
        }
    }
}

#[cfg(feature = "exfat")]
type ExFat<D> = ferric_exfat::ExFat<D>;

type Dir<'v, D> = fatfs::Dir<'v, Disk<D>>;
type DirEntry<'v, D> = fatfs::DirEntry<'v, Disk<D>>;
type File<'v, D> = fatfs::File<'v, Disk<D>>;

/// The longest path, in bytes, that an open resolves.
pub const PATH_MAX: usize = 4096;

/// The mode bits of every directory: searchable and listable by everyone.
const DIRECTORY_MODE: u16 = 0o555;
/// The mode bits of every file: readable by everyone.
const FILE_MODE: u16 = 0o444;

/// Reads the boot sector of `device` and mounts the volume, read-only: as exFAT when its
/// file system name is `EXFAT`, otherwise as FAT (reading, on FAT32, the FSInfo sector too).
///
/// # Errors
///
/// [`Error::Mount`] when `device` cannot be read or does not start with a FAT boot sector
/// `fatfs` accepts, [`Error::ExFatDisabled`] for an exFAT volume when the `exfat` feature
/// is off, and [`Error::ExFat`] when the exFAT reader refuses the volume.
pub fn mount<D: Device>(device: D) -> Result<Volume<D>, Error> {
    let mut first = [0; ferric_exfat::boot::SECTOR_PREFIX];
    device
        .read_at(0, &mut first)
        .map_err(|e| Error::Mount(io::Error::other(e)))?;
    if &first[3..11] == ferric_exfat::boot::SIGNATURE {
        return mount_exfat(device);
    }
    // The default options leave access dates alone, so reading a file writes nothing.
    fatfs::FileSystem::new(Disk::new(device), fatfs::FsOptions::new())
        .map(Volume::Fat)
        .map_err(Error::Mount)
}

#[cfg(feature = "exfat")]
fn mount_exfat<D: Device>(device: D) -> Result<Volume<D>, Error> {
    ExFat::open(device)
        .map(Volume::ExFat)
        .map_err(Error::from_exfat)
}

#[cfg(not(feature = "exfat"))]
fn mount_exfat<D: Device>(_device: D) -> Result<Volume<D>, Error> {
    Err(Error::ExFatDisabled)
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

/// The kind of file a handle or directory entry names: FAT and exFAT have only these two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

impl Kind {
    /// A directory when `is_dir` (the entry's `ATTR_DIRECTORY` bit is set), else a file.
    const fn of(is_dir: bool) -> Self {
        if is_dir { Self::Directory } else { Self::File }
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
    /// The size in bytes, as the directory entry records it: 0 for a FAT directory, the
    /// allocated length of an exFAT one.
    pub size: u64,
    /// The volume's cluster size, the unit it allocates in.
    pub cluster_size: u32,
    /// The last access: FAT keeps only the date, so this is its start; exFAT keeps the time.
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

    fn of_fat<D: Device>(entry: &DirEntry<'_, D>, path: &str, cluster_size: u32) -> Self {
        Self {
            ino: path_ino(path),
            kind: Kind::of(entry.is_dir()),
            size: entry.len(),
            cluster_size,
            accessed: time::date(entry.accessed()),
            modified: time::date_time(entry.modified()),
        }
    }

    #[cfg(feature = "exfat")]
    fn of_exfat(entry: &ferric_exfat::Entry, path: &str, cluster_size: u32) -> Self {
        Self {
            ino: path_ino(path),
            kind: Kind::of(entry.is_dir()),
            size: entry.stream.length,
            cluster_size,
            accessed: entry.accessed.to_unix(),
            modified: entry.modified.to_unix(),
        }
    }

    /// The attributes as the scheme ABI's `Stat`. Owner and group are root, `st_blocks`
    /// counts 512-byte units of the size, and missing times read as 1970. Neither format has
    /// a change time; the last write stands in for it.
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
    /// The clusters the allocation table (on exFAT, the allocation bitmap) marks free.
    pub free_clusters: u64,
}

impl Geometry {
    /// The geometry as the scheme ABI's `StatVfs`. Neither format reserves space for the superuser,
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
    /// The entry's name: on FAT its long name, or its 8.3 short name when it has none.
    pub name: String,
    /// The kind of file it names.
    pub kind: Kind,
    /// A stable number for the file, as in [`Attributes::ino`].
    pub ino: u64,
}

enum Open<'v, D: Device> {
    Metadata,
    File(Data<'v, D>),
    Directory {
        dir: Directory<'v, D>,
        entries: Option<Vec<Entry>>,
    },
}

/// An open file's data.
enum Data<'v, D: Device> {
    Fat(File<'v, D>),
    #[cfg(feature = "exfat")]
    ExFat {
        volume: &'v ExFat<D>,
        stream: ferric_exfat::Stream,
        cursor: ferric_exfat::Cursor,
    },
}

/// An open directory, before its entries are read.
enum Directory<'v, D: Device> {
    Fat(Dir<'v, D>),
    #[cfg(feature = "exfat")]
    ExFat {
        volume: &'v ExFat<D>,
        stream: ferric_exfat::Stream,
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
    Fat(DirEntry<'v, D>),
    #[cfg(feature = "exfat")]
    ExFat(&'v ExFat<D>, ferric_exfat::Entry),
}

/// The open handles of a mounted volume.
pub struct Fs<'v, D: Device> {
    volume: &'v Volume<D>,
    cluster_size: u32,
    handles: BTreeMap<usize, Handle<'v, D>>,
    next: usize,
}

impl<'v, D: Device> Fs<'v, D> {
    /// The handle table of `volume`, with nothing open.
    pub fn new(volume: &'v Volume<D>) -> Self {
        let cluster_size = match volume {
            Volume::Fat(fat) => fat.cluster_size(),
            // At most 32 MiB, which the boot sector check ensures.
            #[cfg(feature = "exfat")]
            Volume::ExFat(exfat) => exfat.boot_sector().cluster_size() as u32,
        };
        Self {
            volume,
            cluster_size,
            handles: BTreeMap::new(),
            next: 1,
        }
    }

    /// The volume's label, without the padding; empty when it has none. FAT keeps it in the
    /// boot sector, exFAT in a root directory entry.
    pub fn label(&self) -> String {
        match self.volume {
            Volume::Fat(fat) => {
                let label = fat.volume_label();
                let label = label.trim_end_matches(' ');
                if label == "NO NAME" {
                    String::new()
                } else {
                    label.to_owned()
                }
            }
            #[cfg(feature = "exfat")]
            Volume::ExFat(exfat) => exfat.label().to_owned(),
        }
    }

    /// The volume's format. Which FAT a FAT volume uses is chosen by its cluster count, as
    /// the FAT specification's "FAT Type Determination" section says.
    pub fn format(&self) -> Format {
        match self.volume {
            Volume::Fat(fat) => match fat.fat_type() {
                fatfs::FatType::Fat12 => Format::Fat12,
                fatfs::FatType::Fat16 => Format::Fat16,
                fatfs::FatType::Fat32 => Format::Fat32,
            },
            #[cfg(feature = "exfat")]
            Volume::ExFat(_) => Format::ExFat,
        }
    }

    /// The volume's size and free space. On FAT12 and FAT16, and on FAT32 without a
    /// trustworthy FSInfo sector, this reads the whole allocation table; on exFAT, the
    /// allocation bitmap.
    ///
    /// # Errors
    ///
    /// [`Error::Read`] or [`Error::ExFat`] when the table or bitmap cannot be read.
    pub fn geometry(&self) -> Result<Geometry, Error> {
        match self.volume {
            Volume::Fat(fat) => {
                let stats = fat.stats().map_err(Error::Read)?;
                Ok(Geometry {
                    cluster_size: stats.cluster_size(),
                    clusters: u64::from(stats.total_clusters()),
                    free_clusters: u64::from(stats.free_clusters()),
                })
            }
            #[cfg(feature = "exfat")]
            Volume::ExFat(exfat) => Ok(Geometry {
                cluster_size: self.cluster_size,
                clusters: u64::from(exfat.boot_sector().cluster_count),
                free_clusters: exfat.free_clusters().map_err(Error::from_exfat)?,
            }),
        }
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
    /// and [`Error::Read`] or [`Error::ExFat`] when a directory cannot be read.
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
            Found::Fat(entry) => Attributes::of_fat(entry, &path, self.cluster_size),
            #[cfg(feature = "exfat")]
            Found::ExFat(_, entry) => Attributes::of_exfat(entry, &path, self.cluster_size),
        };
        if mode.symlink {
            return Err(Error::NotASymlink);
        }
        if mode.directory && attributes.kind != Kind::Directory {
            return Err(Error::NotADirectory);
        }
        let open = if mode.stat {
            Open::Metadata
        } else {
            self.open_found(found)
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
    /// for a directory, and [`Error::Read`] or [`Error::ExFat`] when the file's clusters
    /// cannot be read.
    pub fn read(&mut self, id: usize, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let handle = self.handle_mut(id)?;
        let size = handle.attributes.size;
        let data = match &mut handle.open {
            Open::Metadata => return Err(Error::BadHandle { handle: id }),
            Open::Directory { .. } => return Err(Error::IsADirectory),
            Open::File(data) => data,
        };
        match data {
            Data::Fat(file) => read_fat(file, size, offset, buf),
            #[cfg(feature = "exfat")]
            Data::ExFat {
                volume,
                stream,
                cursor,
            } => volume
                .read(stream, cursor, offset, buf)
                .map_err(Error::from_exfat),
        }
    }

    /// The entries of the directory `id` names, without `.` and `..` and the volume label,
    /// in the order the directory stores them. The list is read on the first call and kept,
    /// so offsets into it stay valid while the handle is open.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`], [`Error::NotADirectory`] for anything else, and [`Error::Read`]
    /// or [`Error::ExFat`] when the directory cannot be read.
    pub fn entries(&mut self, id: usize) -> Result<&[Entry], Error> {
        let handle = self
            .handles
            .get_mut(&id)
            .ok_or(Error::BadHandle { handle: id })?;
        let Open::Directory { dir, entries } = &mut handle.open else {
            return Err(Error::NotADirectory);
        };
        if entries.is_none() {
            *entries = Some(match dir {
                Directory::Fat(dir) => read_fat_entries(dir, &handle.path)?,
                #[cfg(feature = "exfat")]
                Directory::ExFat { volume, stream } => {
                    read_exfat_entries(volume, stream, &handle.path)?
                }
            });
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
    /// one component at a time and in order (see [`walk`]). Returns the path with the names
    /// as stored, and what it names.
    fn resolve(&self, start: &str, path: &str) -> Result<(String, Found<'v, D>), Error> {
        match self.volume {
            Volume::Fat(fat) => {
                let (stored, entry) = walk(
                    start,
                    path,
                    |parent: Option<&DirEntry<'v, D>>, name| {
                        let dir = parent.map_or_else(|| fat.root_dir(), DirEntry::to_dir);
                        let entry = lookup(&dir, name)?;
                        Ok((entry.file_name(), entry))
                    },
                    DirEntry::is_dir,
                )?;
                Ok((stored, entry.map_or(Found::Root, Found::Fat)))
            }
            #[cfg(feature = "exfat")]
            Volume::ExFat(exfat) => {
                let (stored, entry) = walk(
                    start,
                    path,
                    |parent: Option<&ferric_exfat::Entry>, name| {
                        let dir = parent.map_or_else(|| exfat.root(), |entry| entry.stream);
                        let entry = exfat
                            .find(&dir, name)
                            .map_err(Error::from_exfat)?
                            .ok_or(Error::NotFound)?;
                        Ok((entry.name_lossy(), entry))
                    },
                    ferric_exfat::Entry::is_dir,
                )?;
                Ok((
                    stored,
                    entry.map_or(Found::Root, |e| Found::ExFat(exfat, e)),
                ))
            }
        }
    }

    /// What a handle to `found` reads from.
    fn open_found(&self, found: Found<'v, D>) -> Open<'v, D> {
        let dir = |dir| Open::Directory { dir, entries: None };
        match found {
            Found::Root => match self.volume {
                Volume::Fat(fat) => dir(Directory::Fat(fat.root_dir())),
                #[cfg(feature = "exfat")]
                Volume::ExFat(volume) => dir(Directory::ExFat {
                    volume,
                    stream: volume.root(),
                }),
            },
            Found::Fat(entry) if entry.is_dir() => dir(Directory::Fat(entry.to_dir())),
            Found::Fat(entry) => Open::File(Data::Fat(entry.to_file())),
            #[cfg(feature = "exfat")]
            Found::ExFat(volume, entry) if entry.is_dir() => dir(Directory::ExFat {
                volume,
                stream: entry.stream,
            }),
            #[cfg(feature = "exfat")]
            Found::ExFat(volume, entry) => Open::File(Data::ExFat {
                volume,
                stream: entry.stream,
                cursor: ferric_exfat::Cursor::default(),
            }),
        }
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

/// Walks `path` from the directory `start` (from the root when `path` is absolute), one
/// component at a time and in order, as POSIX does: each name is looked up with `lookup` in
/// the directory reached so far (`None` for the root), which returns the entry and its name
/// as stored; `..` goes back to that directory's parent; and any component after a file
/// (`..` and `.` included) fails with [`Error::NotADirectory`]. So `/hello.txt/../EFI` is
/// `ENOTDIR` and `/missing/../EFI` is `ENOENT`, not `/EFI`. Returns the path with the names as
/// stored, and the entry it names (`None` for the root).
fn walk<E>(
    start: &str,
    path: &str,
    mut lookup: impl FnMut(Option<&E>, &str) -> Result<(String, E), Error>,
    is_dir: impl Fn(&E) -> bool,
) -> Result<(String, Option<E>), Error> {
    let start = if path.starts_with('/') { "" } else { start };
    // The entries from the root to where the walk is; empty at the root.
    let mut walked: Vec<(String, E)> = Vec::new();
    for component in start.split('/').chain(path.split('/')) {
        if walked.last().is_some_and(|(_, entry)| !is_dir(entry)) {
            return Err(Error::NotADirectory);
        }
        match component {
            "" | "." => {}
            ".." => {
                walked.pop();
            }
            name => {
                let next = lookup(walked.last().map(|(_, entry)| entry), name)?;
                walked.push(next);
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
    Ok((stored, walked.pop().map(|(_, entry)| entry)))
}

/// Reads into `buf` from byte `offset` of a FAT file of `size` bytes.
fn read_fat<D: Device>(
    file: &mut File<'_, D>,
    size: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, Error> {
    // `fatfs` follows the cluster chain from the start on a seek to another cluster, and not
    // at all for the position the last read left, so sequential reads stay cheap.
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

fn read_fat_entries<D: Device>(dir: &Dir<'_, D>, path: &str) -> Result<Vec<Entry>, Error> {
    let mut entries = Vec::new();
    for entry in dir.iter() {
        let entry = entry.map_err(Error::Read)?;
        if is_dot(&entry) {
            continue;
        }
        let name = entry.file_name();
        entries.push(Entry {
            ino: path_ino(&normalize(path, &name)),
            kind: Kind::of(entry.is_dir()),
            name,
        });
    }
    Ok(entries)
}

#[cfg(feature = "exfat")]
fn read_exfat_entries<D: Device>(
    volume: &ExFat<D>,
    dir: &ferric_exfat::Stream,
    path: &str,
) -> Result<Vec<Entry>, Error> {
    let listing = volume.list(dir).map_err(Error::from_exfat)?;
    Ok(listing
        .into_iter()
        .map(|entry| {
            let name = entry.name_lossy();
            Entry {
                ino: path_ino(&normalize(path, &name)),
                kind: Kind::of(entry.is_dir()),
                name,
            }
        })
        .collect())
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
