//! FAT directory-entry dates and times as Unix times.
//!
//! The "Date and Time Formats" section of Microsoft's FAT specification ("Microsoft
//! Extensible Firmware Initiative FAT32 File System Specification", version 1.03) packs a
//! date as years since 1980, month and day, and a time as hours, minutes and two-second
//! units, with an optional count of hundredths of a second for creation times. `fatfs`
//! decodes the fields into [`fatfs::Date`] and [`fatfs::Time`]; this module turns them into
//! seconds and nanoseconds since 1970.
//!
//! The specification does not say which time zone the fields are in, and FAT records none.
//! `fatd` reads them as UTC, as the UEFI firmware and most other readers do.

/// Seconds and nanoseconds since 1970-01-01 00:00 UTC.
pub type UnixTime = (i64, u32);

/// Days from 1970-01-01 to the given proleptic Gregorian date (negative before it), for a
/// month in 1..=12. The count of days before each era of 400 years, then of the years,
/// months and days inside it, with the year starting in March so that leap days fall last.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The start of `date`, or `None` for a date the fields cannot hold (month 0 or above 12,
/// day 0): FAT writes zero for "no date", which decodes to 1980-00-00.
pub fn date(date: fatfs::Date) -> Option<UnixTime> {
    let (month, day) = (i64::from(date.month), i64::from(date.day));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(i64::from(date.year), month, day);
    Some((days * 86_400, 0))
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
