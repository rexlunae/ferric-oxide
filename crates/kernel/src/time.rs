use crate::{
    sync::{CleanLockToken, Mutex, RwLock, L1},
    syscall::{
        data::TimeSpec,
        error::{Error, Result, EINVAL},
    },
};

pub const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Ferric Oxide: checks that a user-supplied time has a non-negative second
/// count and nanoseconds in `0..NANOS_PER_SEC`, so `TimeSpec::to_nanos` does
/// not turn a negative value into a time that never arrives.
///
/// # Errors
///
/// Returns `EINVAL` for any other value.
pub fn check_timespec(time: &TimeSpec) -> Result<()> {
    if time.tv_sec < 0 || time.tv_nsec < 0 || time.tv_nsec as u128 >= NANOS_PER_SEC {
        return Err(Error::new(EINVAL));
    }
    Ok(())
}

// TODO: seqlock?
/// Kernel start time, measured in nanoseconds since Unix epoch
pub static START: Mutex<L1, u128> = Mutex::new(0);
/// Kernel up time, measured in nanoseconds since `START_TIME`
pub static OFFSET: RwLock<L1, u128> = RwLock::new(0);

pub fn monotonic(token: &mut CleanLockToken) -> u128 {
    crate::arch::time::monotonic_absolute(token)
}

pub fn realtime(token: &mut CleanLockToken) -> u128 {
    let start = { *START.lock(token.token()) };
    let offset = { monotonic(token) };
    start + offset
}

pub fn monotonic_resolution() -> u128 {
    crate::arch::time::monotonic_resolution()
}

pub fn realtime_resolution() -> u128 {
    monotonic_resolution()
}

pub fn sys_update_time_offset(buf: &[u8], token: &mut CleanLockToken) -> Result<usize> {
    let start = <[u8; 16]>::try_from(buf).map_err(|_| Error::new(EINVAL))?;
    *START.lock(token.token()) = u128::from_ne_bytes(start);
    Ok(16)
}
