//! Firmware wall-clock conversion and the ARM64 boot environment handoff.
//!
//! Implements `EFI_TIME` semantics from UEFI 2.10 section 8.3.1:
//! <https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#gettime>.
//! A sample pairs Unix time with the ARM generic physical counter, which keeps
//! running across the firmware/kernel transition. The handoff contains their
//! difference, so boot delays are counted exactly once. No firmware runtime
//! calls are needed after entering the kernel.
#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use core::{fmt, str::FromStr};
use time::{Date, Month, Time};

const NANOS_PER_SEC: u128 = 1_000_000_000;
const MAX_UNIX_NS: u128 = 253_402_300_800 * NANOS_PER_SEC - 1;

/// Boot environment key for a hexadecimal Unix-minus-monotonic offset in ns.
pub const ENV_KEY: &str = "BOOT_TIME_OFFSET_NS";

/// Calendar fields returned by UEFI `GetTime`, without firmware ABI padding.
#[derive(Clone, Copy, Debug)]
pub struct FirmwareTime {
    /// Gregorian year, 1900 through 9999.
    pub year: u16,
    /// Month, 1 through 12.
    pub month: u8,
    /// Day of the month, validated against the year and month.
    pub day: u8,
    /// Hour, 0 through 23.
    pub hour: u8,
    /// Minute, 0 through 59.
    pub minute: u8,
    /// Second, 0 through 59.
    pub second: u8,
    /// Fractional nanoseconds, less than one billion.
    pub nanosecond: u32,
    /// Minutes added to local time to obtain UTC, or 2047 if unspecified.
    pub timezone: i16,
    /// UEFI daylight flags (bits 0 and 1); the timezone already includes DST.
    pub daylight: u8,
}

impl FirmwareTime {
    /// Converts a firmware date to nanoseconds since the Unix epoch.
    ///
    /// An unspecified timezone is treated as UTC, matching the OS convention
    /// that an RTC without timezone metadata stores UTC. No additional DST
    /// adjustment is applied: UEFI specifies that it is included in `timezone`.
    ///
    /// # Errors
    /// Returns an error for invalid fields or UTC dates outside 1970..=9999.
    pub fn unix_nanos(self) -> Result<u128, Error> {
        if !(1900..=9999).contains(&self.year) {
            return Err(Error::OutOfRange);
        }
        if self.daylight & !3 != 0 {
            return Err(Error::InvalidDaylight);
        }
        let timezone = match self.timezone {
            2047 => 0,
            -1440..=1440 => self.timezone,
            _ => return Err(Error::InvalidTimezone),
        };
        let month = Month::try_from(self.month).map_err(Error::InvalidCalendar)?;
        let date = Date::from_calendar_date(i32::from(self.year), month, self.day)
            .map_err(Error::InvalidCalendar)?;
        let time = Time::from_hms_nano(self.hour, self.minute, self.second, self.nanosecond)
            .map_err(Error::InvalidCalendar)?;
        // UEFI uses Local = UTC - TimeZone, opposite to an ISO 8601 offset.
        let nanos = date.with_time(time).assume_utc().unix_timestamp_nanos()
            + i128::from(timezone) * 60 * NANOS_PER_SEC as i128;
        let nanos = u128::try_from(nanos).map_err(|_| Error::OutOfRange)?;
        if nanos > MAX_UNIX_NS {
            return Err(Error::OutOfRange);
        }
        Ok(nanos)
    }
}

/// A validated Unix-minus-monotonic offset, shared by bootloader and kernel.
///
/// ```
/// use ferric_boot_time::BootTime;
/// let clock = BootTime::from_sample(1_700_000_000_000_000_000, 10_000_000_000)?;
/// // Five seconds later, realtime advances by five seconds, not fifteen.
/// assert_eq!(clock.offset_ns() + 15_000_000_000, 1_700_000_005_000_000_000);
/// # Ok::<(), ferric_boot_time::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootTime(u128);

impl BootTime {
    /// Anchors a Unix time to a contemporaneous monotonic counter, both in ns.
    ///
    /// # Errors
    /// Rejects dates past year 9999 and samples requiring a negative offset,
    /// which the kernel's unsigned realtime representation cannot express.
    pub fn from_sample(unix_ns: u128, monotonic_ns: u128) -> Result<Self, Error> {
        if unix_ns > MAX_UNIX_NS {
            return Err(Error::OutOfRange);
        }
        unix_ns
            .checked_sub(monotonic_ns)
            .map(Self)
            .ok_or(Error::OutOfRange)
    }

    /// Nanoseconds to add to the monotonic counter to obtain Unix time.
    pub fn offset_ns(self) -> u128 {
        self.0
    }

    /// Reads the optional clock handoff from newline-separated boot variables.
    ///
    /// # Errors
    /// Rejects non-UTF-8 data, malformed values and duplicate clock variables.
    pub fn from_env(env: &[u8]) -> Result<Option<Self>, Error> {
        let env = core::str::from_utf8(env).map_err(|_| Error::InvalidHandoff)?;
        let mut result = None;
        for line in env.lines() {
            if let Some((ENV_KEY, value)) = line.split_once('=') {
                if result.is_some() {
                    return Err(Error::InvalidHandoff);
                }
                result = Some(value.parse()?);
            }
        }
        Ok(result)
    }
}

impl fmt::Display for BootTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl FromStr for BootTime {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidHandoff);
        }
        let ns = u128::from_str_radix(s, 16).map_err(|_| Error::InvalidHandoff)?;
        Self::from_sample(ns, 0)
    }
}

/// A firmware time or boot handoff validation failure.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// A date or time component is invalid.
    InvalidCalendar(time::error::ComponentRange),
    /// The timezone is outside the UEFI range and is not unspecified.
    InvalidTimezone,
    /// Reserved daylight flag bits are set.
    InvalidDaylight,
    /// The clock cannot be represented by the kernel's unsigned epoch offset.
    OutOfRange,
    /// The boot environment clock value is malformed or duplicated.
    InvalidHandoff,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCalendar(e) => write!(f, "invalid firmware calendar: {e}"),
            Self::InvalidTimezone => f.write_str("invalid firmware timezone"),
            Self::InvalidDaylight => f.write_str("invalid firmware daylight flags"),
            Self::OutOfRange => f.write_str("boot clock offset outside supported range"),
            Self::InvalidHandoff => f.write_str("invalid boot clock handoff"),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InvalidCalendar(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
