//! The test on Redox; see the crate documentation.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use redox_scheme::scheme::{SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{CallerCtx, OpenResult, RequestKind, SignalBehavior, Socket};
use syscall::error::{EBADF, ENOENT, Error, Result as SysResult};
use syscall::schemev2::NewFdFlags;

/// The scheme's name.
const SCHEME: &str = "scheme-self-test";
/// The one file it serves, and its contents.
const FILE_PATH: &str = "file";
const CONTENTS: &[u8] = b"served by the other thread";
const ROOT: usize = 0;
const FILE: usize = 1;
/// How long the open and the read may take. A regression may stop this thread from running
/// too (it shares the stuck CPU's queue), in which case the boot's timeout fails the run.
const WATCHDOG: Duration = Duration::from_secs(30);

/// A scheme with one read-only file.
struct OneFile;

impl SchemeSync for OneFile {
    fn scheme_root(&mut self) -> SysResult<usize> {
        Ok(ROOT)
    }

    fn openat(
        &mut self,
        fd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> SysResult<OpenResult> {
        if fd != ROOT || path.trim_matches('/') != FILE_PATH {
            return Err(Error::new(ENOENT));
        }
        Ok(OpenResult::ThisScheme {
            number: FILE,
            flags: NewFdFlags::POSITIONED,
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> SysResult<usize> {
        if id != FILE {
            return Err(Error::new(EBADF));
        }
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(CONTENTS.len());
        let bytes = &CONTENTS[start..];
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }
}

/// Serves the scheme until the process exits.
fn serve(socket: Socket, mut scheme: OneFile) {
    let mut state = SchemeState::new();
    while let Ok(Some(request)) = socket.next_request(SignalBehavior::Restart) {
        let RequestKind::Call(call) = request.kind() else {
            continue;
        };
        let response = call.handle_sync(&mut scheme, &mut state);
        if socket
            .write_response(response, SignalBehavior::Restart)
            .is_err()
        {
            return;
        }
    }
}

pub fn main() -> ExitCode {
    thread::spawn(|| {
        thread::sleep(WATCHDOG);
        println!("FERRIC_TEST_FAIL scheme-self-open: no answer after {WATCHDOG:?}");
        std::process::exit(1);
    });
    match run() {
        Ok(()) => {
            println!("FERRIC_TEST_PASS scheme-self-open");
            ExitCode::SUCCESS
        }
        Err(why) => {
            println!("FERRIC_TEST_FAIL scheme-self-open: {why}");
            ExitCode::FAILURE
        }
    }
}

/// Why the test failed.
#[derive(Debug)]
enum Failure {
    /// Creating or registering the scheme failed.
    Scheme { what: &'static str, errno: i32 },
    /// Opening or reading the file failed.
    Io {
        what: &'static str,
        source: std::io::Error,
    },
    /// The read returned other bytes than the scheme serves.
    Contents(Vec<u8>),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheme { what, errno } => write!(f, "{what} failed with errno {errno}"),
            Self::Io { what, source } => write!(f, "{what}: {source}"),
            Self::Contents(got) => write!(f, "read {got:?}, expected {CONTENTS:?}"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Scheme { .. } | Self::Contents(_) => None,
        }
    }
}

fn run() -> Result<(), Failure> {
    let scheme_err = |what| {
        move |err: Error| Failure::Scheme {
            what,
            errno: err.errno,
        }
    };
    let io_err = |what| move |source| Failure::Io { what, source };
    // Created and registered on this thread, so this thread is the scheme's handler context.
    let socket = Socket::create().map_err(scheme_err("create the scheme socket"))?;
    let mut scheme = OneFile;
    register_sync_scheme(&socket, SCHEME, &mut scheme)
        .map_err(scheme_err("register the scheme"))?;
    thread::spawn(move || serve(socket, scheme));
    let mut file =
        File::open(format!("/scheme/{SCHEME}/{FILE_PATH}")).map_err(io_err("open the file"))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(io_err("read the file"))?;
    if contents != CONTENTS {
        return Err(Failure::Contents(contents));
    }
    Ok(())
}
