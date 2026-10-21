//! Guest-facing flag values.
//!
//! These are this ABI's own values, fixed for `ferric:sys@1`. They are not Redox's `O_*`
//! numbers, which may change upstream; the runtime converts them. Unknown bits are refused
//! with `EINVAL`, so a later version can give them a meaning.

use crate::errno::Errno;

macro_rules! flag_set {
    ($(#[$doc:meta])* $name:ident: $repr:ty { $($(#[$fdoc:meta])* $flag:ident = $value:expr;)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name($repr);

        impl $name {
            $($(#[$fdoc])* pub const $flag: Self = Self($value);)*

            /// Every defined bit.
            pub const ALL: Self = Self(0 $(| $value)*);

            /// The flags in `bits`.
            ///
            /// # Errors
            ///
            /// [`Errno::EINVAL`] if a bit is set that this ABI version does not define.
            pub const fn from_bits(bits: $repr) -> Result<Self, Errno> {
                if bits & !Self::ALL.0 != 0 {
                    Err(Errno::EINVAL)
                } else {
                    Ok(Self(bits))
                }
            }

            /// The raw bits.
            pub const fn bits(self) -> $repr {
                self.0
            }

            /// Whether every flag of `other` is set.
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            /// Both sets of flags.
            pub const fn union(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }
        }
    };
}

flag_set! {
    /// `openat` flags.
    OpenFlags: u32 {
        /// Open for reading.
        READ = 1 << 0;
        /// Open for writing.
        WRITE = 1 << 1;
        /// Create the file if it does not exist.
        CREATE = 1 << 2;
        /// With `CREATE`: fail if it exists.
        EXCL = 1 << 3;
        /// Truncate it to zero length.
        TRUNC = 1 << 4;
        /// Writes go to the end.
        APPEND = 1 << 5;
        /// Fail unless it is a directory.
        DIRECTORY = 1 << 6;
        /// Do not follow a final symbolic link.
        NOFOLLOW = 1 << 7;
        /// Close the descriptor across `spawn`.
        CLOEXEC = 1 << 8;
        /// Non-blocking I/O.
        NONBLOCK = 1 << 9;
    }
}

impl OpenFlags {
    /// Checks a combination: at least one of `READ`, `WRITE` or `DIRECTORY`, and `EXCL`
    /// only with `CREATE`.
    ///
    /// # Errors
    ///
    /// [`Errno::EINVAL`] for unknown bits or an inconsistent combination.
    pub const fn checked(bits: u32) -> Result<Self, Errno> {
        let flags = match Self::from_bits(bits) {
            Ok(flags) => flags,
            Err(errno) => return Err(errno),
        };
        let access = Self::READ.0 | Self::WRITE.0 | Self::DIRECTORY.0;
        if flags.0 & access == 0 || (flags.contains(Self::EXCL) && !flags.contains(Self::CREATE)) {
            return Err(Errno::EINVAL);
        }
        Ok(flags)
    }
}

flag_set! {
    /// `mmap` protection. There is no execute bit: wasm code is never mapped by a guest.
    Prot: u32 {
        /// Readable.
        READ = 1 << 0;
        /// Writable.
        WRITE = 1 << 1;
    }
}

flag_set! {
    /// `waitpid` options.
    WaitFlags: u32 {
        /// Return 0 instead of blocking.
        NOHANG = 1 << 0;
        /// Also report stopped children.
        UNTRACED = 1 << 1;
    }
}

/// `sigaction` handler value for the default action.
pub const SIG_DFL: u64 = u64::MAX;

/// `sigaction` handler value that ignores the signal.
pub const SIG_IGN: u64 = u64::MAX - 1;

/// A clock for `clock_gettime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Clock {
    /// Wall-clock time since the Unix epoch.
    Realtime,
    /// Time since an arbitrary point that never goes backwards.
    Monotonic,
}

impl Clock {
    /// The clock with this number: 0 is `Realtime`, 1 is `Monotonic`.
    ///
    /// # Errors
    ///
    /// [`Errno::EINVAL`] for any other number.
    pub const fn from_raw(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Realtime),
            1 => Ok(Self::Monotonic),
            _ => Err(Errno::EINVAL),
        }
    }
}

/// `sigprocmask`'s `how`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskHow {
    /// Add the set to the mask.
    Block,
    /// Remove the set from the mask.
    Unblock,
    /// Replace the mask.
    Set,
}

impl MaskHow {
    /// `how` from its number: 0 block, 1 unblock, 2 set.
    ///
    /// # Errors
    ///
    /// [`Errno::EINVAL`] for any other number.
    pub const fn from_raw(raw: u32) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Block),
            1 => Ok(Self::Unblock),
            2 => Ok(Self::Set),
            _ => Err(Errno::EINVAL),
        }
    }

    /// The new mask.
    pub const fn apply(self, mask: u64, set: u64) -> u64 {
        match self {
            Self::Block => mask | set,
            Self::Unblock => mask & !set,
            Self::Set => set,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_bits_and_bad_combinations_are_refused() {
        assert_eq!(OpenFlags::from_bits(1 << 10), Err(Errno::EINVAL));
        let rw = OpenFlags::READ.union(OpenFlags::WRITE);
        assert_eq!(OpenFlags::checked(rw.bits()), Ok(rw));
        assert_eq!(
            OpenFlags::checked(OpenFlags::CREATE.bits()),
            Err(Errno::EINVAL)
        );
        let excl = OpenFlags::WRITE.union(OpenFlags::EXCL);
        assert_eq!(OpenFlags::checked(excl.bits()), Err(Errno::EINVAL));
        let create = excl.union(OpenFlags::CREATE);
        assert!(OpenFlags::checked(create.bits()).is_ok());
        assert_eq!(Prot::from_bits(4), Err(Errno::EINVAL));
        assert_eq!(Prot::ALL.bits(), 3);
        assert_eq!(WaitFlags::from_bits(1), Ok(WaitFlags::NOHANG));
    }

    #[test]
    fn small_enums_decode() {
        assert_eq!(Clock::from_raw(1), Ok(Clock::Monotonic));
        assert_eq!(Clock::from_raw(2), Err(Errno::EINVAL));
        assert_eq!(
            MaskHow::from_raw(0).map(|how| how.apply(0b01, 0b10)),
            Ok(0b11)
        );
        assert_eq!(MaskHow::Unblock.apply(0b11, 0b10), 0b01);
        assert_eq!(MaskHow::Set.apply(0b11, 0b100), 0b100);
        assert_eq!(MaskHow::from_raw(3), Err(Errno::EINVAL));
        assert_ne!(SIG_DFL, SIG_IGN);
    }
}
