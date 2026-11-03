//! Checks that a write on an `O_APPEND` description goes to the end of the file and leaves
//! the cursor there (#244), on RedoxFS (`/tmp`) and on ramfs (`/scheme/logging`, the
//! instance every image runs).
//!
//! For each file system it writes a 16-byte file, opens it for reading and appending (the
//! cursor starts at 0), appends 4 bytes and checks that `lseek(SEEK_CUR)` reports 20 and
//! that a read finds nothing left. It then seeks back to 0, appends one more byte, checks
//! the cursor is 21 and the contents are the three writes in order. It prints
//! `FERRIC_TEST_PASS append-redoxfs` and `FERRIC_TEST_PASS append-ramfs`, or
//! `FERRIC_TEST_FAIL append-<fs>: <reason>`. Without `/scheme/logging` the ramfs step prints
//! `FERRIC_TEST_SKIP append-ramfs`.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

/// The file's first contents.
const START: &[u8; 16] = b"0123456789abcdef";

/// Why a step failed.
#[derive(Debug)]
enum Failure {
    Io {
        what: &'static str,
        source: io::Error,
    },
    Mismatch {
        what: &'static str,
        got: String,
        want: String,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { what, source } => write!(f, "{what}: {source}"),
            Self::Mismatch { what, got, want } => write!(f, "{what} is {got}, expected {want}"),
        }
    }
}

fn io(what: &'static str) -> impl FnOnce(io::Error) -> Failure {
    move |source| Failure::Io { what, source }
}

fn same<T: PartialEq + fmt::Debug>(what: &'static str, got: T, want: T) -> Result<(), Failure> {
    if got == want {
        Ok(())
    } else {
        Err(Failure::Mismatch {
            what,
            got: format!("{got:?}"),
            want: format!("{want:?}"),
        })
    }
}

/// The append checks on `path`, which this creates.
fn appends(path: &str) -> Result<(), Failure> {
    fs::write(path, START).map_err(io("write the file"))?;
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .map_err(io("open for appending"))?;
    same(
        "the cursor after open",
        file.stream_position().map_err(io("lseek"))?,
        0,
    )?;
    file.write_all(b"more").map_err(io("append"))?;
    same(
        "the cursor after appending 4 bytes",
        file.stream_position().map_err(io("lseek"))?,
        20,
    )?;
    let mut rest = Vec::new();
    file.read_to_end(&mut rest)
        .map_err(io("read after the append"))?;
    same("what a read after the append finds", rest, Vec::new())?;

    file.seek(SeekFrom::Start(0)).map_err(io("seek to 0"))?;
    file.write_all(b"!").map_err(io("append again"))?;
    same(
        "the cursor after the second append",
        file.stream_position().map_err(io("lseek"))?,
        21,
    )?;
    drop(file);
    same(
        "the contents",
        fs::read(path).map_err(io("read the file"))?,
        b"0123456789abcdefmore!".to_vec(),
    )
}

/// Runs [`appends`] on `path` in `dir`, removes the file and reports the outcome.
fn step(name: &str, dir: &str, path: &str) -> bool {
    if fs::metadata(dir).is_err() {
        println!("FERRIC_TEST_SKIP append-{name}: no {dir}");
        return true;
    }
    let result = appends(path);
    if let Err(err) = fs::remove_file(path)
        && err.kind() != ErrorKind::NotFound
    {
        eprintln!("ferric-append-test: remove {path}: {err}");
    }
    match result {
        Ok(()) => {
            println!("FERRIC_TEST_PASS append-{name}");
            true
        }
        Err(err) => {
            println!("FERRIC_TEST_FAIL append-{name}: {err}");
            false
        }
    }
}

fn main() -> ExitCode {
    let redoxfs = step("redoxfs", "/tmp", "/tmp/ferric-append-test");
    let ramfs = step(
        "ramfs",
        "/scheme/logging",
        "/scheme/logging/ferric-append-test",
    );
    if redoxfs && ramfs {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
