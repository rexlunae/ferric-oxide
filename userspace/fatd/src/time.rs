//! FAT directory-entry dates and times as Unix times.
//!
//! The "Date and Time Formats" section of Microsoft's FAT specification ("Microsoft
//! Extensible Firmware Initiative FAT32 File System Specification", version 1.03) packs a
//! date as years since 1980, month and day, and a time as hours, minutes and two-second
//! units, with an optional count of hundredths of a second for creation times. `fatfs`
//! decodes the fields into [`fatfs::Date`] and [`fatfs::Time`]; this module turns them into
//! seconds and nanoseconds since 1970, with the calendar arithmetic `ferric-exfat` uses for
//! exFAT's timestamps, which keep FAT's format.
//!
//! The specification does not say which time zone the fields are in, and FAT records none.
//! `fatd` reads them as UTC, as the UEFI firmware and most other readers do.

pub use ferric_exfat::time::UnixTime;

/// The start of `date`, or `None` for a date the fields cannot hold (month 0 or above 12,
/// day 0): FAT writes zero for "no date", which decodes to 1980-00-00.
pub fn date(date: fatfs::Date) -> Option<UnixTime> {
    let seconds = ferric_exfat::time::from_fields(date.year, date.month, date.day, 0, 0, 0)?;
    Some((seconds, 0))
}

/// `date_time` as a Unix time, or `None` when its date is not one ([`date`]). Times past
/// the end of a day, which the fields can hold but the specification does not allow, are
/// counted on into the next day rather than refused.
pub fn date_time(date_time: fatfs::DateTime) -> Option<UnixTime> {
    let (day, _) = date(date_time.date)?;
    let t = date_time.time;
    let seconds = i64::from(t.hour) * 3600 + i64::from(t.min) * 60 + i64::from(t.sec);
    let millis = u32::from(t.millis).min(999);
    Some((day + seconds, millis * 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: u16, month: u16, day: u16, hour: u16, min: u16, sec: u16) -> fatfs::DateTime {
        fatfs::DateTime {
            date: fatfs::Date { year, month, day },
            time: fatfs::Time {
                hour,
                min,
                sec,
                millis: 0,
            },
        }
    }

    #[test]
    fn known_dates() {
        // `date -u -d ... +%s`.
        assert_eq!(date_time(at(1980, 1, 1, 0, 0, 0)), Some((315_532_800, 0)));
        assert_eq!(date_time(at(2026, 1, 1, 0, 0, 0)), Some((1_767_225_600, 0)));
        assert_eq!(
            date_time(at(2000, 2, 29, 12, 34, 56)),
            Some((951_827_696, 0))
        );
        assert_eq!(
            date_time(at(2107, 12, 31, 23, 59, 58)),
            Some((4_354_819_198, 0))
        );
    }

    #[test]
    fn hundredths_become_nanoseconds() {
        let mut t = at(2026, 1, 1, 0, 0, 0);
        t.time.millis = 990;
        assert_eq!(date_time(t), Some((1_767_225_600, 990_000_000)));
    }

    #[test]
    fn empty_dates_are_none() {
        assert_eq!(date_time(at(1980, 0, 0, 0, 0, 0)), None);
        assert_eq!(date_time(at(1980, 13, 1, 0, 0, 0)), None);
        assert_eq!(
            date(fatfs::Date {
                year: 2026,
                month: 10,
                day: 26
            }),
            Some((1_792_972_800, 0))
        );
    }
}
