//! The `ferric:sys@1` imports and their signatures.
//!
//! Each [`Import`] is one function imported from [`MODULE`](crate::MODULE). The runtime
//! registers a host function for each, and refuses to instantiate a module that imports a
//! name from `ferric:sys@1` that is not listed, or with a different signature, so a module
//! built for a newer ABI fails at load instead of at the first call.
//!
//! Conventions (see the crate documentation): pointers and lengths are `i64` offsets into
//! memory 0, fds are `i32`, and results are `i64` encoded as in [`errno`](crate::errno).
//! A string is a `(pointer, length)` pair of UTF-8 bytes with no terminator; a list of
//! strings is one buffer of NUL-terminated strings ([`memory::split_strings`]).
//!
//! [`memory::split_strings`]: crate::memory::split_strings

use ValType::{I32, I64};

/// A wasm value type used by the imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    /// `i32`.
    I32,
    /// `i64`.
    I64,
}

macro_rules! imports {
    ($(
        $(#[$doc:meta])*
        $variant:ident = $name:literal ($($param:ident),*) -> ($($result:ident),*);
    )*) => {
        /// A `ferric:sys@1` import.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum Import {
            $($(#[$doc])* $variant,)*
        }

        impl Import {
            /// Every import, in ABI order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            /// The import's field name in the `ferric:sys@1` module.
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Its parameter types.
            pub const fn params(self) -> &'static [ValType] {
                match self {
                    $(Self::$variant => &[$($param),*],)*
                }
            }

            /// Its result types.
            pub const fn results(self) -> &'static [ValType] {
                match self {
                    $(Self::$variant => &[$($result),*],)*
                }
            }
        }
    };
}

imports! {
    /// `() -> version`: the ABI version, [`ABI_VERSION`](crate::ABI_VERSION).
    AbiVersion = "abi_version" () -> (I64);
    /// `(dirfd, path, path_len, flags, mode) -> fd`: `openat` beneath a directory fd, or
    /// in the process namespace for an absolute path. `flags` are [`OpenFlags`].
    ///
    /// [`OpenFlags`]: crate::flags::OpenFlags
    Openat = "openat" (I32, I64, I64, I32, I32) -> (I64);
    /// `(fd, buf, len) -> bytes`.
    Read = "read" (I32, I64, I64) -> (I64);
    /// `(fd, buf, len) -> bytes`.
    Write = "write" (I32, I64, I64) -> (I64);
    /// `(fd, buf, len, offset) -> bytes`.
    Pread = "pread" (I32, I64, I64, I64) -> (I64);
    /// `(fd, buf, len, offset) -> bytes`.
    Pwrite = "pwrite" (I32, I64, I64, I64) -> (I64);
    /// `(fd) -> 0`.
    Close = "close" (I32) -> (I64);
    /// `(fd, path, path_len) -> fd`: Redox `dup`, whose path names a sub-handle of a scheme
    /// (empty for a plain duplicate).
    Dup = "dup" (I32, I64, I64) -> (I64);
    /// `(fd, out) -> 0`: writes a [`Stat`](crate::layout::Stat) at `out`.
    Fstat = "fstat" (I32, I64) -> (I64);
    /// `(fd, out) -> 0`: writes a [`StatVfs`](crate::layout::StatVfs) at `out`.
    Fstatvfs = "fstatvfs" (I32, I64) -> (I64);
    /// `(fd, buf, len) -> bytes`: the descriptor's path.
    Fpath = "fpath" (I32, I64, I64) -> (I64);
    /// `(fd) -> 0`.
    Fsync = "fsync" (I32) -> (I64);
    /// `(fd, len) -> 0`.
    Ftruncate = "ftruncate" (I32, I64) -> (I64);
    /// `(fd, times) -> 0`: two [`Timespec`](crate::layout::Timespec)s (access, modification).
    Futimens = "futimens" (I32, I64) -> (I64);
    /// `(fd, mode) -> 0`.
    Fchmod = "fchmod" (I32, I32) -> (I64);
    /// `(fd, buf, len, cookie) -> bytes`: directory entries, as the scheme's `getdents`.
    Getdents = "getdents" (I32, I64, I64, I64) -> (I64);
    /// `(fd, payload, payload_len, flags, metadata, metadata_words) -> value`: `SYS_CALL`, a
    /// scheme-specific verb; `metadata` points to that many little-endian `u64` words.
    Call = "call" (I32, I64, I64, I32, I64, I64) -> (I64);
    /// `(socket, fd, flags) -> 0`: `sendfd`; the fd leaves the guest's table.
    Sendfd = "sendfd" (I32, I32, I32) -> (I64);
    /// `(fd, offset, len, prot, addr) -> addr`: maps part of `fd` over the page-aligned
    /// window at `addr` (see [`Window`](crate::memory::Window)); `prot` is [`Prot`].
    ///
    /// [`Prot`]: crate::flags::Prot
    Mmap = "mmap" (I32, I64, I64, I32, I64) -> (I64);
    /// `(addr, len) -> 0`: puts zero pages back over a window.
    Munmap = "munmap" (I64, I64) -> (I64);
    /// `(clock, out) -> 0`: writes a [`Timespec`](crate::layout::Timespec); `clock` is a
    /// [`Clock`](crate::flags::Clock).
    ClockGettime = "clock_gettime" (I32, I64) -> (I64);
    /// `(nanoseconds) -> 0`.
    Nanosleep = "nanosleep" (I64) -> (I64);
    /// `(addr, expected, timeout_ns) -> 0`: sleeps while the `u32` at `addr` equals
    /// `expected`; a negative timeout waits forever.
    FutexWait = "futex_wait" (I64, I32, I64) -> (I64);
    /// `(addr, count) -> woken`.
    FutexWake = "futex_wake" (I64, I32) -> (I64);
    /// `(path, path_len, argv, argv_len, envp, envp_len, fds, fds_count) -> pid`: starts a
    /// program with the listed guest fds (`u32` each) as its fds 0, 1, 2 ...; `argv` and
    /// `envp` are NUL-terminated string lists.
    Spawn = "spawn" (I64, I64, I64, I64, I64, I64, I64, I64) -> (I64);
    /// `(pid, status_out, options) -> pid`: writes the `i32` status at `status_out` unless it
    /// is `u64::MAX`; `options` are [`WaitFlags`](crate::flags::WaitFlags).
    Waitpid = "waitpid" (I64, I64, I32) -> (I64);
    /// `(code)`: ends the process; does not return.
    Exit = "exit" (I32) -> ();
    /// `(pid, signal) -> 0`.
    Kill = "kill" (I64, I32) -> (I64);
    /// `() -> pid`.
    Getpid = "getpid" () -> (I64);
    /// `() -> pid`: the child's pid in the parent, 0 in the child; `ENOSYS` in colocated mode.
    Fork = "fork" () -> (I64);
    /// `(signal, handler, flags) -> previous`: `handler` is an index into table 0 (a
    /// `(i32) -> ()` function), or [`SIG_DFL`](crate::flags::SIG_DFL) /
    /// [`SIG_IGN`](crate::flags::SIG_IGN). Handlers run at the next safe point.
    Sigaction = "sigaction" (I32, I64, I32) -> (I64);
    /// `(how, set, old_out) -> 0`: `set` is a 64-bit signal mask value; the previous mask is
    /// written at `old_out` unless it is `u64::MAX`.
    Sigprocmask = "sigprocmask" (I32, I64, I64) -> (I64);
    /// `(sq_entries, cq_entries, window) -> ring`: a brokered ring whose memory is the
    /// page-aligned window at `window`.
    RingSetup = "ring_setup" (I32, I32, I64) -> (I64);
    /// `(ring, to_submit, min_complete) -> submitted`: checks and forwards the window's new
    /// submissions, then waits for completions.
    RingEnter = "ring_enter" (I32, I32, I32) -> (I64);
}

impl Import {
    /// The import with this field name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|import| import.name() == name)
    }

    /// Checks a module's import of `name` with the given signature.
    ///
    /// # Errors
    ///
    /// [`ImportError::Unknown`] if `ferric:sys@1` has no such import, and
    /// [`ImportError::Signature`] if its types differ.
    pub fn check(name: &str, params: &[ValType], results: &[ValType]) -> Result<Self, ImportError> {
        let import = Self::from_name(name).ok_or(ImportError::Unknown)?;
        if import.params() == params && import.results() == results {
            Ok(import)
        } else {
            Err(ImportError::Signature { import })
        }
    }
}

/// Why a module's import is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportError {
    /// `ferric:sys@1` has no import with that name.
    Unknown,
    /// The import exists with other types.
    Signature {
        /// The import.
        import: Import,
    },
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unknown => f.write_str("no such ferric:sys import"),
            Self::Signature { import } => {
                write!(
                    f,
                    "ferric:sys import {} has another signature",
                    import.name()
                )
            }
        }
    }
}

impl core::error::Error for ImportError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_found() {
        for (index, import) in Import::ALL.iter().enumerate() {
            assert_eq!(Import::from_name(import.name()), Some(*import));
            assert!(
                Import::ALL[index + 1..]
                    .iter()
                    .all(|other| other.name() != import.name()),
                "{import:?}"
            );
        }
        assert_eq!(Import::from_name("fd_write"), None);
    }

    #[test]
    fn every_fallible_import_returns_one_i64() {
        for import in Import::ALL {
            if *import == Import::Exit {
                assert_eq!(import.results(), &[]);
            } else {
                assert_eq!(import.results(), &[ValType::I64], "{import:?}");
            }
        }
    }

    #[test]
    fn signatures_are_checked() {
        assert_eq!(
            Import::check("close", &[ValType::I32], &[ValType::I64]),
            Ok(Import::Close)
        );
        assert_eq!(
            Import::check("close", &[ValType::I64], &[ValType::I64]),
            Err(ImportError::Signature {
                import: Import::Close
            })
        );
        assert_eq!(Import::check("fork2", &[], &[]), Err(ImportError::Unknown));
    }

    #[test]
    fn the_abi_order_is_stable() {
        // Appending is fine; reordering or renaming breaks ABI version 1.
        let names: [&str; 6] = core::array::from_fn(|i| Import::ALL[i].name());
        assert_eq!(
            names,
            ["abi_version", "openat", "read", "write", "pread", "pwrite"]
        );
        assert_eq!(Import::ALL.len(), 34);
    }
}
