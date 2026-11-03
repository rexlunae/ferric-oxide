use std::collections::BTreeMap;
use std::convert::TryInto;
use std::os::unix::io::AsRawFd;
use std::{fs, iter, time};

use ferric_abi::id::{
    chmod_permitted, chown_permitted, mode_after_chmod, mode_after_chown, owner_after,
    times_access, Caller, TimesAccess,
};
use ferric_abi::time::TimeChange;
use indexmap::IndexMap;
use libredox::Fd;
use redox_path::RedoxReference;
use syscall::error::{
    EACCES, EBADFD, EIO, EISDIR, ENFILE, ENOENT, ENOMEM, ENXIO, EOVERFLOW, EPERM,
};
use syscall::{Error, Result, TimeSpec, EINVAL, MODE_DIR, MODE_FILE, MODE_PERM, MODE_TYPE};

use super::scheme::current_perm;

#[derive(Debug)]
pub struct File {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub nlink: usize,
    pub parent: Inode,

    pub open_handles: usize,

    pub atime: TimeSpec,
    pub ctime: TimeSpec,
    pub mtime: TimeSpec,

    pub data: FileData,
}

impl File {
    /// Ferric Oxide: `fchown` by `caller` (see [`Ownership::chown_by`]).
    ///
    /// # Errors
    /// `EPERM` when `caller` may not make the change.
    pub fn chown_by(&mut self, caller: Caller, uid: u32, gid: u32) -> Result<()> {
        self.set_ownership(self.ownership().chown_by(caller, uid, gid)?);
        Ok(())
    }

    /// Ferric Oxide: `fchmod` by `caller` (see [`Ownership::chmod_by`]).
    ///
    /// # Errors
    /// `EPERM` when `caller` may not make the change.
    pub fn chmod_by(&mut self, caller: Caller, mode: u16) -> Result<()> {
        self.set_ownership(self.ownership().chmod_by(caller, mode)?);
        Ok(())
    }

    /// Ferric Oxide: `futimens` by `caller` (see [`Times::after_futimens`]).
    ///
    /// # Errors
    /// `EPERM` or `EACCES` when `caller` may not make the change.
    pub fn set_times_by(
        &mut self,
        caller: Caller,
        accessed: TimeChange,
        modified: TimeChange,
        now: TimeSpec,
    ) -> Result<()> {
        let times =
            self.times()
                .after_futimens(caller, self.ownership(), accessed, modified, now)?;
        Times {
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
        } = times;
        Ok(())
    }

    fn times(&self) -> Times {
        Times {
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
        }
    }

    /// The file's owner, group and mode.
    pub fn ownership(&self) -> Ownership {
        Ownership {
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
        }
    }

    fn set_ownership(&mut self, ownership: Ownership) {
        Ownership {
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
        } = ownership;
    }

    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        match self.data {
            FileData::File(ref bytes) => {
                if self.mode & MODE_TYPE == MODE_DIR {
                    return Err(Error::new(EBADFD));
                }

                let src_bytes = bytes.get(offset..).unwrap_or(&[]);
                let bytes_to_read = src_bytes.len().min(buf.len());
                buf[..bytes_to_read].copy_from_slice(&src_bytes[..bytes_to_read]);
                Ok(bytes_to_read)
            }

            FileData::Directory(_) => Err(Error::new(EISDIR)),
            FileData::Socket(_) => Err(Error::new(ENXIO)),
        }
    }

    /// Ferric Oxide: writes `buf` at the end of the file (`O_APPEND`). The kernel then moves
    /// the description's cursor to the size `fsize` reports, the end of `buf`.
    ///
    /// # Errors
    /// Those of [`Self::write`].
    pub fn append(&mut self, buf: &[u8]) -> Result<usize> {
        self.write(self.data.size(), buf)
    }

    pub fn write(&mut self, offset: usize, buf: &[u8]) -> Result<usize> {
        match self.data {
            FileData::File(ref mut bytes) => {
                if self.mode & MODE_TYPE == MODE_DIR {
                    return Err(Error::new(EBADFD));
                }

                // if there's a seek hole, fill it with 0 and continue writing.
                let end_off = offset.checked_add(buf.len()).ok_or(Error::new(EOVERFLOW))?;
                if end_off > bytes.len() {
                    let additional = end_off - bytes.len();
                    bytes.try_reserve(additional).or(Err(Error::new(ENOMEM)))?;
                    bytes.resize(end_off, 0u8);
                }
                bytes[offset..][..buf.len()].copy_from_slice(buf);

                Ok(buf.len())
            }

            FileData::Directory(_) => Err(Error::new(EISDIR)),
            FileData::Socket(_) => Err(Error::new(ENXIO)),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Inode(pub usize);

pub enum FileData {
    File(Vec<u8>),
    Directory(IndexMap<String, Inode>),
    Socket(Fd),
}

impl FileData {
    pub fn size(&self) -> usize {
        match self {
            &Self::File(ref data) => data.len(),
            &Self::Directory(_) | &Self::Socket(_) => 0,
        }
    }
}

impl std::fmt::Debug for FileData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(data) => f.debug_tuple("File").field(data).finish(),
            Self::Directory(files) => f.debug_tuple("Directory").field(files).finish(),
            Self::Socket(fd) => f.debug_tuple("Socket").field(&fd.raw()).finish(),
        }
    }
}
pub struct Filesystem {
    pub files: BTreeMap<usize, File>,
    pub memory_file: fs::File,
    pub last_inode_number: usize,
}
impl Filesystem {
    pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
    pub const ROOT_INODE: usize = 1;

    pub fn new(root_mode: u16) -> Result<Self> {
        Ok(Self {
            files: iter::once((Self::ROOT_INODE, Self::create_root_inode(root_mode))).collect(),
            memory_file: fs::File::open("/scheme/memory").or(Err(Error::new(EIO)))?,
            last_inode_number: Self::ROOT_INODE,
        })
    }
    fn create_root_inode(mode: u16) -> File {
        let cur_time = current_time();
        File {
            atime: cur_time,
            ctime: cur_time,
            mtime: cur_time,

            mode: MODE_DIR | (mode & 0o7777),
            nlink: 1,
            open_handles: 0,

            uid: 0,
            gid: 0,

            data: FileData::Directory(IndexMap::new()),
            parent: Inode(Self::ROOT_INODE),
        }
    }
    pub fn get_block_size(&self) -> Result<u32> {
        Ok(libredox::call::fstatvfs(self.memory_file.as_raw_fd() as usize)?.f_bsize as u32)
    }
    pub fn block_size(&self) -> u32 {
        self.get_block_size().unwrap_or(Self::DEFAULT_BLOCK_SIZE)
    }
    pub fn next_inode_number(&mut self) -> Result<usize> {
        let next = self
            .last_inode_number
            .checked_add(1)
            .ok_or(Error::new(ENFILE))?;
        self.last_inode_number = next;
        Ok(next)
    }
    fn resolve_generic(&self, parts: RedoxReference<'_>, uid: u32, gid: u32) -> Result<usize> {
        let mut current_file = self
            .files
            .get(&Self::ROOT_INODE)
            .ok_or(Error::new(ENOENT))?;
        let mut current_inode = Self::ROOT_INODE;

        if parts.as_ref().is_empty() {
            return Ok(current_inode);
        }

        let mut parts = parts.as_ref().split('/');

        loop {
            let Some(part) = parts.next() else {
                break;
            };
            let dentries = match current_file.data {
                FileData::Directory(ref dentries) => dentries,
                FileData::File(_) | FileData::Socket(_) => return Err(Error::new(ENOENT)),
            };
            let perm = current_perm(&current_file, uid, gid);
            if perm & 0o1 == 0 {
                return Err(Error::new(EACCES));
            }

            current_inode = dentries.get(part).ok_or(Error::new(ENOENT))?.0;
            current_file = self.files.get(&current_inode).ok_or(Error::new(EIO))?;
        }
        Ok(current_inode)
    }
    pub fn resolve_except_last<'a>(
        &self,
        path: &'a str,
        uid: u32,
        gid: u32,
    ) -> Result<(usize, Option<RedoxReference<'a>>)> {
        let path = RedoxReference::new(path)
            .ok_or(Error::new(EINVAL))?
            .canonical();

        let (dir, name) = path.dirname_split();
        Ok((
            self.resolve_generic(dir, uid, gid)?,
            name.map(|s| s.into_owned()),
        ))
    }
    pub fn resolve(&self, path: &str, uid: u32, gid: u32) -> Result<usize> {
        let path = RedoxReference::new(path).ok_or(Error::new(EINVAL))?;

        self.resolve_generic(path.canonical(), uid, gid)
    }
}

pub fn current_time() -> TimeSpec {
    let sys_time = time::SystemTime::now();

    let duration = match sys_time.duration_since(time::SystemTime::UNIX_EPOCH) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("The time is apparently now before the Unix epoch...");

            let negative_duration = e.duration();

            return TimeSpec {
                tv_sec: negative_duration
                    .as_secs()
                    .try_into()
                    .unwrap_or(i64::min_value()),
                tv_nsec: negative_duration
                    .subsec_nanos()
                    .try_into()
                    .unwrap_or(i32::min_value()),
            };
        }
    };

    TimeSpec {
        tv_sec: duration.as_secs().try_into().unwrap_or(i64::max_value()),
        tv_nsec: duration
            .subsec_nanos()
            .try_into()
            .unwrap_or(i32::max_value()),
    }
}

/// Ferric Oxide: a file's owner, group and mode (type and permission bits), which
/// `fchown` and `fchmod` change under the POSIX rules in `ferric_abi::id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ownership {
    /// The file type and permission bits (`MODE_TYPE` and `MODE_PERM`).
    pub mode: u16,
    /// The owner's user ID.
    pub uid: u32,
    /// The file's group ID.
    pub gid: u32,
}

impl Ownership {
    /// After `fchown(uid, gid)` by `caller`: the owner and group asked for, each kept when it
    /// is `ferric_abi::id::UNCHANGED` (-1, as POSIX `chown` defines it), and the set-ID bits
    /// an unprivileged change must clear, cleared.
    ///
    /// # Errors
    /// `EPERM` unless `ferric_abi::id::chown_permitted` allows it: root, or the owner
    /// keeping the owner and choosing its own group.
    pub fn chown_by(self, caller: Caller, uid: u32, gid: u32) -> Result<Self> {
        if !chown_permitted(caller, (self.uid, self.gid), uid, gid) {
            return Err(Error::new(EPERM));
        }
        let (uid, gid) = owner_after((self.uid, self.gid), uid, gid);
        Ok(Self {
            mode: mode_after_chown(self.mode, self.is_regular(), caller),
            uid,
            gid,
        })
    }

    /// After `fchmod(mode)` by `caller`: the permission bits of `mode`, the file's type
    /// kept. Bits outside the permissions are ignored, and set-group-ID is dropped when an
    /// unprivileged caller is not in the file's group.
    ///
    /// # Errors
    /// `EPERM` unless `caller` is root or owns the file.
    pub fn chmod_by(self, caller: Caller, mode: u16) -> Result<Self> {
        if !chmod_permitted(caller, self.uid) {
            return Err(Error::new(EPERM));
        }
        let permissions = mode_after_chmod(mode & MODE_PERM, self.is_regular(), self.gid, caller);
        Ok(Self {
            mode: permissions | (self.mode & MODE_TYPE),
            ..self
        })
    }

    /// The permission bits (`0o4` read, `0o2` write, `0o1` execute or search) a caller with
    /// effective IDs `uid` and `gid` has: all of them for root, else those of the owner, the
    /// group or others, whichever class the caller is in first.
    pub fn permissions(self, uid: u32, gid: u32) -> u8 {
        if uid == 0 {
            return 0o7;
        }
        let perm = self.mode & MODE_PERM;
        let class = if uid == self.uid {
            perm >> 6
        } else if gid == self.gid {
            perm >> 3
        } else {
            perm
        };
        (class & 0o7) as u8
    }

    fn is_regular(self) -> bool {
        self.mode & MODE_TYPE == MODE_FILE
    }
}

/// Ferric Oxide: a file's access, modification and change times, which `futimens` sets
/// under the POSIX rule in `ferric_abi::id`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Times {
    /// The last access time.
    pub atime: TimeSpec,
    /// The last modification time.
    pub mtime: TimeSpec,
    /// The last status change time.
    pub ctime: TimeSpec,
}

impl Times {
    /// After `futimens` by `caller` on a file with `ownership`: the access and modification
    /// times set as `accessed` and `modified` ask, [`TimeChange::Now`] meaning `now`, and the
    /// change time `now`. Two [`TimeChange::Omit`]s change nothing.
    ///
    /// # Errors
    /// Under `ferric_abi::id::times_access`: `EPERM` when only the owner or root may make
    /// the change, and `EACCES` when `caller` asks for the current time for both without
    /// owning the file or being able to write it.
    pub fn after_futimens(
        self,
        caller: Caller,
        ownership: Ownership,
        accessed: TimeChange,
        modified: TimeChange,
        now: TimeSpec,
    ) -> Result<Self> {
        match times_access(caller, ownership.uid, accessed, modified) {
            TimesAccess::Permitted => {}
            TimesAccess::NeedsWrite => {
                if ownership.permissions(caller.uid, caller.gid) & 0o2 == 0 {
                    return Err(Error::new(EACCES));
                }
            }
            TimesAccess::NotOwner => return Err(Error::new(EPERM)),
        }
        if (accessed, modified) == (TimeChange::Omit, TimeChange::Omit) {
            return Ok(self);
        }
        let time = |change, current| match change {
            TimeChange::Now => now,
            TimeChange::Omit => current,
            TimeChange::At {
                seconds,
                nanoseconds,
            } => TimeSpec {
                tv_sec: seconds,
                // Below one second, so it fits.
                tv_nsec: nanoseconds as i32,
            },
        };
        Ok(Self {
            atime: time(accessed, self.atime),
            mtime: time(modified, self.mtime),
            ctime: now,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::mem::ManuallyDrop;

    use ferric_abi::id::{Caller, UNCHANGED};
    use ferric_abi::time::TimeChange;
    use syscall::{TimeSpec, EACCES, EPERM, MODE_DIR, MODE_FILE};

    use super::{File, FileData, Inode, Ownership, Times};

    const ROOT: Caller = Caller { uid: 0, gid: 0 };
    const USER: Caller = Caller {
        uid: 1000,
        gid: 1000,
    };

    fn file(mode: u16, uid: u32, gid: u32) -> Ownership {
        Ownership { mode, uid, gid }
    }

    /// #246: a process that can open a file it does not own cannot give it to itself or
    /// change its mode.
    #[test]
    fn a_file_another_user_owns_cannot_be_taken_or_rechmoded() {
        let theirs = file(MODE_FILE | 0o666, 0, 0);
        for (uid, gid) in [(1000, UNCHANGED), (UNCHANGED, 1000), (UNCHANGED, UNCHANGED)] {
            assert_eq!(theirs.chown_by(USER, uid, gid).unwrap_err().errno, EPERM);
        }
        assert_eq!(theirs.chmod_by(USER, 0o777).unwrap_err().errno, EPERM);
    }

    #[test]
    fn the_owner_may_regroup_and_chmod_its_own_file() {
        let mine = file(MODE_FILE | 0o644, 1000, 50);
        let regrouped = mine.chown_by(USER, UNCHANGED, 1000).unwrap();
        assert_eq!(regrouped, file(MODE_FILE | 0o644, 1000, 1000));
        let private = regrouped.chmod_by(USER, 0o600).unwrap();
        assert_eq!(private, file(MODE_FILE | 0o600, 1000, 1000));
        // But not give it away.
        assert_eq!(mine.chown_by(USER, 0, UNCHANGED).unwrap_err().errno, EPERM);
    }

    #[test]
    fn root_may_change_any_file() {
        let theirs = file(MODE_FILE | 0o644, 1000, 1000);
        let taken = theirs.chown_by(ROOT, 0, 0).unwrap();
        let setuid = taken.chmod_by(ROOT, 0o4755).unwrap();
        assert_eq!(setuid, file(MODE_FILE | 0o4755, 0, 0));
    }

    /// An unprivileged regroup of a set-ID executable clears its set-ID bits; chmod keeps
    /// the file type and drops bits outside the permissions.
    #[test]
    fn set_id_bits_and_the_file_type_are_handled_as_posix_says() {
        let program = file(MODE_FILE | 0o6755, 1000, 1000);
        assert_eq!(
            program.chown_by(USER, UNCHANGED, 1000).unwrap().mode,
            MODE_FILE | 0o0755
        );

        let dir = file(MODE_DIR | 0o755, 1000, 1000);
        assert_eq!(
            dir.chmod_by(USER, MODE_FILE | 0o2700).unwrap().mode,
            MODE_DIR | 0o2700
        );

        // Set-group-ID needs the file's group.
        let other_group = file(MODE_FILE | 0o755, 1000, 50);
        assert_eq!(
            other_group.chmod_by(USER, 0o2755).unwrap().mode,
            MODE_FILE | 0o0755
        );
    }

    const BEFORE: TimeSpec = TimeSpec {
        tv_sec: 10,
        tv_nsec: 1,
    };
    const UNTOUCHED: Times = Times {
        atime: BEFORE,
        mtime: BEFORE,
        ctime: BEFORE,
    };
    const NOW: TimeSpec = TimeSpec {
        tv_sec: 99,
        tv_nsec: 9,
    };
    const THEN: TimeChange = TimeChange::At {
        seconds: 1_000_000,
        nanoseconds: 500,
    };
    const THEN_SPEC: TimeSpec = TimeSpec {
        tv_sec: 1_000_000,
        tv_nsec: 500,
    };

    fn touch(
        caller: Caller,
        ownership: Ownership,
        accessed: TimeChange,
        modified: TimeChange,
    ) -> syscall::Result<Times> {
        UNTOUCHED.after_futimens(caller, ownership, accessed, modified, NOW)
    }

    /// #375: someone else's file keeps its times unless the caller only touches it and may
    /// write it.
    #[test]
    fn another_users_file_may_only_be_touched_when_writable() {
        let now = TimeChange::Now;
        let writable = file(MODE_FILE | 0o666, 0, 0);
        for (accessed, modified) in [
            (THEN, THEN),
            (THEN, TimeChange::Omit),
            (now, TimeChange::Omit),
        ] {
            let err = touch(USER, writable, accessed, modified).unwrap_err();
            assert_eq!(err.errno, EPERM, "{accessed:?} {modified:?}");
        }
        let read_only = file(MODE_FILE | 0o644, 0, 0);
        assert_eq!(touch(USER, read_only, now, now).unwrap_err().errno, EACCES);

        let touched = touch(USER, writable, now, now).unwrap();
        assert_eq!(
            touched,
            Times {
                atime: NOW,
                mtime: NOW,
                ctime: NOW
            }
        );
    }

    /// The group's write bit counts for a caller in the file's group, and only it.
    #[test]
    fn touching_follows_the_callers_class() {
        let now = TimeChange::Now;
        let group_writable = file(MODE_FILE | 0o664, 0, USER.gid);
        touch(USER, group_writable, now, now).unwrap();
        let others_writable_only = file(MODE_FILE | 0o646, 0, USER.gid);
        let err = touch(USER, others_writable_only, now, now).unwrap_err();
        assert_eq!(err.errno, EACCES);
    }

    /// The `UTIME_OMIT` and `UTIME_NOW` sentinels are honoured, not stored as times.
    #[test]
    fn the_owner_sets_omits_and_nows() {
        let mine = file(MODE_FILE | 0o400, USER.uid, USER.gid);
        let set = touch(USER, mine, THEN, TimeChange::Omit).unwrap();
        assert_eq!(
            set,
            Times {
                atime: THEN_SPEC,
                mtime: BEFORE,
                ctime: NOW
            }
        );

        let later = TimeSpec {
            tv_sec: 100,
            tv_nsec: 0,
        };
        let set = set
            .after_futimens(USER, mine, TimeChange::Omit, TimeChange::Now, later)
            .unwrap();
        assert_eq!(
            set,
            Times {
                atime: THEN_SPEC,
                mtime: later,
                ctime: later
            }
        );
    }

    /// Two omits change nothing, not even the change time, and need no permission.
    #[test]
    fn omitting_both_changes_nothing() {
        let theirs = file(MODE_FILE | 0o600, 0, 0);
        let omit = TimeChange::Omit;
        assert_eq!(touch(USER, theirs, omit, omit).unwrap(), UNTOUCHED);
    }

    #[test]
    fn root_may_set_any_files_times() {
        let theirs = file(MODE_FILE, 1000, 1000);
        assert_eq!(
            touch(ROOT, theirs, THEN, THEN).unwrap(),
            Times {
                atime: THEN_SPEC,
                mtime: THEN_SPEC,
                ctime: NOW
            }
        );
    }

    #[test]
    fn permissions_come_from_the_callers_class() {
        let ownership = file(MODE_FILE | 0o741, 1000, 50);
        assert_eq!(ownership.permissions(0, 0), 0o7);
        assert_eq!(ownership.permissions(1000, 0), 0o7);
        assert_eq!(ownership.permissions(1001, 50), 0o4);
        assert_eq!(ownership.permissions(1001, 51), 0o1);
        // The owner's class wins even when it grants less than the group's.
        assert_eq!(file(MODE_FILE | 0o070, 1000, 50).permissions(1000, 50), 0);
    }

    /// Never dropped: dropping a `FileData` may close a socket `Fd`, which needs the
    /// libredox runtime and does not link on the host.
    fn regular(bytes: &[u8]) -> ManuallyDrop<File> {
        ManuallyDrop::new(File {
            mode: MODE_FILE | 0o644,
            uid: 0,
            gid: 0,
            nlink: 1,
            parent: Inode(1),
            open_handles: 1,
            atime: TimeSpec::default(),
            ctime: TimeSpec::default(),
            mtime: TimeSpec::default(),
            data: FileData::File(bytes.to_vec()),
        })
    }

    fn contents(file: &File) -> &[u8] {
        match &file.data {
            FileData::File(bytes) => bytes,
            other => panic!("not a regular file: {:?}", other),
        }
    }

    /// #244: an append goes to the end whatever the description's cursor, and the size
    /// afterwards (what `fsize` reports, and where the kernel puts the cursor) is its end.
    #[test]
    fn appends_write_at_the_end_and_the_size_follows() {
        let mut file = regular(b"0123456789abcdef");
        assert_eq!(file.append(b"more").unwrap(), 4);
        assert_eq!(file.data.size(), 20);
        assert_eq!(contents(&file), b"0123456789abcdefmore");
        let mut rest = [0; 4];
        assert_eq!(file.read(20, &mut rest).unwrap(), 0);
        // A positioned write still uses its offset.
        assert_eq!(file.write(4, b"WXYZ").unwrap(), 4);
        assert_eq!(contents(&file), b"0123WXYZ89abcdefmore");
    }
}
