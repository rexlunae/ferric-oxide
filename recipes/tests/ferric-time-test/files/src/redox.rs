//! Exercise timer ownership among live driver timers, and observe real expiry.

use std::{
    error::Error,
    fmt,
    fs::{File, OpenOptions},
    io::Write,
    num::ParseIntError,
    os::fd::AsRawFd,
    process::ExitCode,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use libredox::{
    Fd,
    flag::{O_CLOEXEC, O_NONBLOCK, O_RDWR},
};
use syscall::{CLOCK_MONOTONIC, CLOCK_REALTIME, EAGAIN, EBADF, EVENT_READ, Event, TimeSpec};

const FAR: i64 = i64::MAX / 4;
const FLOOR: u128 = FAR as u128 * 1_000_000_000 / 4;

#[derive(Debug)]
enum Failure {
    Io(std::io::Error),
    Kernel(libredox::error::Error),
    StatInteger(ParseIntError),
    StatMissing,
    DeadlineRange { phase: &'static str, latest: u128 },
    DeadlinePresence,
    WorkerPanicked,
    ExpiryTimedOut,
    ShortWrite { expected: usize, actual: usize },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("time-scheme I/O failed"),
            Self::Kernel(_) => f.write_str("racing timer write failed"),
            Self::StatInteger(_) => f.write_str("invalid timeout count"),
            Self::StatMissing => f.write_str("timeout fields missing from sys/stat"),
            Self::DeadlineRange { phase, latest } => {
                write!(f, "{phase}: unexpected latest deadline {latest}")
            }
            Self::DeadlinePresence => f.write_str("next deadline disagrees with the pending count"),
            Self::WorkerPanicked => f.write_str("racing writer panicked"),
            Self::ExpiryTimedOut => f.write_str("due timers did not expire in five seconds"),
            Self::ShortWrite { expected, actual } => {
                write!(f, "timer write: expected {expected} bytes, got {actual}")
            }
        }
    }
}

impl Error for Failure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Kernel(source) => Some(source),
            Self::StatInteger(source) => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

fn latest() -> Result<u128, Failure> {
    let stat = std::fs::read_to_string("/scheme/sys/stat")?;
    let value = |key| stat.lines().find_map(|line| line.strip_prefix(key));
    let count: usize = value("timeout_subscribers: ")
        .ok_or(Failure::StatMissing)?
        .parse()
        .map_err(Failure::StatInteger)?;
    let next = value("timeout_next_monotonic_ns: ").ok_or(Failure::StatMissing)?;
    if (count == 0) != (next == "none") {
        return Err(Failure::DeadlinePresence);
    }
    let latest = value("timeout_latest_monotonic_ns: ").ok_or(Failure::StatMissing)?;
    if latest == "none" {
        Ok(0)
    } else {
        latest.parse().map_err(Failure::StatInteger)
    }
}

fn expect_latest(range: std::ops::Range<u128>, phase: &'static str) -> Result<(), Failure> {
    let latest = latest()?;
    if !range.contains(&latest) {
        return Err(Failure::DeadlineRange { phase, latest });
    }
    Ok(())
}

fn open_timer(clock: usize) -> Result<File, Failure> {
    Ok(OpenOptions::new()
        .write(true)
        .open(format!("/scheme/time/{clock}/timer"))?)
}

fn dates(seconds: i64, count: usize) -> Vec<u8> {
    // Use the native ABI definition, leaving padding initialized rather than
    // exposing TimeSpec's uninitialized struct padding.
    let mut bytes = vec![0; size_of::<TimeSpec>() * count];
    for spec in bytes.chunks_exact_mut(size_of::<TimeSpec>()) {
        let at = std::mem::offset_of!(TimeSpec, tv_sec);
        spec[at..at + size_of::<i64>()].copy_from_slice(&seconds.to_ne_bytes());
    }
    bytes
}

fn owner_close(clock: usize) -> Result<(), Failure> {
    let mut first = open_timer(clock)?;
    let mut second = open_timer(clock)?;
    first.write_all(&dates(FAR, 8))?;
    second.write_all(&dates(FAR / 2, 8))?;
    expect_latest(3 * FLOOR..u128::MAX, "two owners")?;
    let alias = first.try_clone()?;
    drop(first);
    expect_latest(3 * FLOOR..u128::MAX, "non-final close")?;
    drop(alias);
    expect_latest(FLOOR..3 * FLOOR, "first final close")?;
    drop(second);
    expect_latest(0..FLOOR, "second final close")
}

fn close_race(clock: usize, seconds: i64) -> Result<(), Failure> {
    for round in 0..64 {
        let timer = open_timer(clock)?;
        let fd = timer.as_raw_fd() as usize;
        let bytes = dates(seconds, 128);
        let expected = bytes.len();
        let barrier = Arc::new(Barrier::new(2));
        let peer = Arc::clone(&barrier);
        let writer = thread::spawn(move || {
            peer.wait();
            libredox::call::write(fd, &bytes)
        });
        barrier.wait();
        for _ in 0..round % 4 {
            thread::yield_now();
        }
        drop(timer);
        // Do not open another fd before join: a late raw write must see EBADF,
        // not an unrelated descriptor reusing its numeric fd.
        match writer.join().map_err(|_| Failure::WorkerPanicked)? {
            Ok(actual) if actual == expected => (),
            Ok(actual) => return Err(Failure::ShortWrite { expected, actual }),
            Err(error) if error.errno() == EBADF => (),
            Err(source) => return Err(Failure::Kernel(source)),
        }
        expect_latest(0..FLOOR, "post-close resources")?;
    }
    Ok(())
}

fn expiry(clock: usize) -> Result<(), Failure> {
    let mut timer = open_timer(clock)?;
    let queue =
        Fd::open("/scheme/event/", O_CLOEXEC | O_NONBLOCK | O_RDWR, 0).map_err(Failure::Kernel)?;
    let subscription = Event {
        id: timer.as_raw_fd() as usize,
        flags: EVENT_READ,
        data: 42,
    };
    let written = queue.write(&subscription).map_err(Failure::Kernel)?;
    if written != size_of::<Event>() {
        return Err(Failure::ShortWrite {
            expected: size_of::<Event>(),
            actual: written,
        });
    }
    timer.write_all(&dates(0, 512))?;
    let limit = Instant::now() + Duration::from_secs(5);
    loop {
        let mut event = Event::default();
        match queue.read(&mut event) {
            Ok(n)
                if n == size_of::<Event>()
                    && event.id == subscription.id
                    && event.data == 42
                    && event.flags.contains(EVENT_READ) =>
            {
                break;
            }
            Ok(_) => return Err(Failure::DeadlinePresence),
            Err(error) if error.errno() == EAGAIN => (),
            Err(source) => return Err(Failure::Kernel(source)),
        }
        if Instant::now() >= limit {
            return Err(Failure::ExpiryTimedOut);
        }
        thread::yield_now();
    }
    drop(timer);
    expect_latest(0..FLOOR, "post-expiry resources")
}

fn run() -> Result<(), Failure> {
    expect_latest(0..FLOOR, "post-close resources")?;
    for clock in [CLOCK_MONOTONIC, CLOCK_REALTIME] {
        owner_close(clock)?;
        close_race(clock, FAR)?;
        close_race(clock, 0)?;
        expiry(clock)?;
    }
    Ok(())
}

pub(super) fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("FERRIC_TEST_PASS time-ownership");
            println!("FERRIC_TEST_PASS time-close-race");
            println!("FERRIC_TEST_PASS time-expiry");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("FERRIC_TEST_FAIL time: {error}");
            let mut source = error.source();
            while let Some(error) = source {
                eprintln!("  caused by: {error}");
                source = error.source();
            }
            ExitCode::FAILURE
        }
    }
}
