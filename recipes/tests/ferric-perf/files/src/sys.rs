//! The system calls the benchmarks need that `std` does not offer, behind safe wrappers:
//! `fork` and `execve`, `posix_spawn`, `waitpid`, and a read-only private file mapping.
//!
//! This is the crate's only unsafe code. relibc implements `posix_spawn` natively on Redox
//! (a new child process without copying the parent's address space), but the `libc` crate
//! does not bind it for Redox, so it is declared here with the POSIX signature.

use std::ffi::{CString, NulError, c_char, c_int, c_void};
use std::fs::File;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr::{self, NonNull};

unsafe extern "C" {
    /// POSIX `posix_spawn` (IEEE Std 1003.1-2024, `<spawn.h>`). The file actions and
    /// attributes are opaque here; the benchmarks pass none.
    fn posix_spawn(
        pid: *mut libc::pid_t,
        path: *const c_char,
        file_actions: *const c_void,
        attrp: *const c_void,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> c_int;
}

/// A program to start, with its arguments and the caller's environment, held as the
/// NUL-terminated strings and pointer arrays `execve` and `posix_spawn` take, so starting it
/// allocates nothing.
pub struct Program {
    /// The path (also `argv[0]`), the other arguments, then the environment entries. The
    /// pointer arrays point into these strings' heap buffers, which stay put while the strings
    /// live, however the vector moves.
    strings: Vec<CString>,
    argv: Vec<*mut c_char>,
    envp: Vec<*mut c_char>,
}

impl Program {
    /// `path` run with `args` after `argv[0]` (which is `path`) and this process's
    /// environment.
    ///
    /// # Errors
    ///
    /// A path, argument or environment entry that holds a NUL byte.
    pub fn new(path: &Path, args: &[&str]) -> Result<Self, NulError> {
        let mut strings = vec![CString::new(path.as_os_str().as_bytes())?];
        for arg in args {
            strings.push(CString::new(*arg)?);
        }
        let argc = strings.len();
        for (key, value) in std::env::vars_os() {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            strings.push(CString::new(entry)?);
        }
        let pointers = |strings: &[CString]| -> Vec<*mut c_char> {
            strings
                .iter()
                .map(|s| s.as_ptr().cast_mut())
                .chain([ptr::null_mut()])
                .collect()
        };
        Ok(Self {
            argv: pointers(&strings[..argc]),
            envp: pointers(&strings[argc..]),
            strings,
        })
    }

    /// The program's path.
    fn path(&self) -> *const c_char {
        self.strings[0].as_ptr()
    }

    /// Forks, runs the program in the child with `execve`, and waits for it.
    ///
    /// # Errors
    ///
    /// When `fork` or `waitpid` fails. A failed `execve` is the child's exit status 127.
    pub fn fork_exec(&self) -> io::Result<ExitStatus> {
        let (path, argv, envp) = (
            self.path(),
            self.argv.as_ptr().cast(),
            self.envp.as_ptr().cast(),
        );
        // SAFETY: `fork` has no memory preconditions. Between it and `execve` the child only
        // calls `execve` and `_exit`, which are async-signal-safe, so it is sound in a process
        // with other threads; the pointers they take were made before the fork and stay
        // valid in the child's copy of the address space.
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => Err(io::Error::last_os_error()),
            0 => {
                // SAFETY: `path`, `argv` and `envp` are a NUL-terminated string and
                // NULL-terminated arrays of them, owned by `self` (see `Program::new`).
                unsafe {
                    libc::execve(path, argv, envp);
                    libc::_exit(127)
                }
            }
            child => wait(child),
        }
    }

    /// Starts the program with `posix_spawn`, without file actions or attributes, and waits
    /// for it.
    ///
    /// # Errors
    ///
    /// When `posix_spawn` or `waitpid` fails.
    pub fn spawn(&self) -> io::Result<ExitStatus> {
        let mut pid = 0;
        // SAFETY: `pid` is a valid place for the child's id; `path`, `argv` and `envp` are as
        // in `fork_exec`; null file actions and attributes ask for the defaults.
        let error = unsafe {
            posix_spawn(
                &mut pid,
                self.path(),
                ptr::null(),
                ptr::null(),
                self.argv.as_ptr(),
                self.envp.as_ptr(),
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        wait(pid)
    }
}

/// Waits for child `pid` to exit.
fn wait(pid: libc::pid_t) -> io::Result<ExitStatus> {
    let mut status = 0;
    loop {
        // SAFETY: `status` is a valid place for the wait status.
        if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
            return Ok(ExitStatus::from_raw(status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// A read-only private mapping of a file (`PROT_READ`, `MAP_PRIVATE`), as the dynamic
/// linker maps the objects it loads. It is unmapped on drop, or by [`Mapping::unmap`], which
/// reports the error.
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    /// Maps the first `len` bytes of `file`.
    ///
    /// # Errors
    ///
    /// When `mmap` fails, and `InvalidInput` for a `len` of 0.
    pub fn new(file: &File, len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // SAFETY: a new mapping at an address the kernel picks touches no existing memory; the
        // descriptor is open for as long as the call runs.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { ptr, len })
    }

    /// Reads one byte every `stride` bytes, from the first, so each page is faulted in when
    /// `stride` is at most the page size, and returns their sum.
    pub fn touch(&self, stride: usize) -> u64 {
        let mut sum = 0u64;
        for offset in (0..self.len).step_by(stride.max(1)) {
            // SAFETY: `offset < len`, so the byte is inside the mapping, which is readable
            // until `self` is dropped or unmapped. A volatile read keeps every access, and no
            // reference to the mapped memory is ever made, so nothing assumes it cannot change.
            sum += u64::from(unsafe { self.ptr.add(offset).read_volatile() });
        }
        sum
    }

    /// Unmaps the file.
    ///
    /// # Errors
    ///
    /// When `munmap` fails; the mapping is then left in place.
    pub fn unmap(self) -> io::Result<()> {
        let this = ManuallyDrop::new(self);
        // SAFETY: the range is exactly the mapping `new` made, and `this` is never used or
        // dropped again.
        if unsafe { libc::munmap(this.ptr.as_ptr().cast(), this.len) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: as in `unmap`. A failure leaves the mapping in place, which only an error
        // path reaches.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn programs_hold_their_arguments_and_environment() {
        let program = Program::new(Path::new("/bin/sh"), &["-c", "true"]).expect("no NUL");
        assert_eq!(program.argv.len(), 4);
        assert!(program.argv[3].is_null());
        assert_eq!(program.envp.len(), std::env::vars_os().count() + 1);
        assert!(program.envp.last().expect("terminated").is_null());
        assert!(Program::new(Path::new("/bin/sh"), &["a\0b"]).is_err());
    }

    #[test]
    fn a_mapping_reads_the_file() {
        let dir = std::env::temp_dir().join(format!("ferric-perf-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("map");
        let mut file = File::create(&path).expect("create");
        file.write_all(&[3u8; 8192]).expect("write");
        let file = File::open(&path).expect("open");
        let mapping = Mapping::new(&file, 8192).expect("maps");
        assert_eq!(mapping.touch(4096), 6);
        assert_eq!(mapping.touch(1), 3 * 8192);
        mapping.unmap().expect("unmaps");
        assert!(Mapping::new(&file, 0).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
