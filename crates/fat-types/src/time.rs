//! FAT and exFAT timestamps as Unix times.
//!
//! exFAT keeps FAT's packed date and time ("Timestamp Fields" in the exFAT specification;
//! "Date and Time Formats" in Microsoft's FAT specification, version 1.03): years since
//! 1980, month, day, hours, minutes and two-second units. exFAT adds a count of 10 ms
//! increments (0 to 199) and the offset from UTC of the time zone the time was written in.
//! FAT records no zone, so FAT times, and exFAT times whose offset is marked not valid, are
//! read as UTC.

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

/// The Unix time of a date and time given by their fields, or `None` for a date the fields
/// cannot hold (month 0 or above 12, day 0 or above 31): FAT writes zero for "no date".
/// Times past the end of a day, which the fields can hold but the specifications do not
/// allow, are counted on into the next day rather than refused.
pub fn from_fields(year: u16, month: u16, day: u16, hour: u16, min: u16, sec: u16) -> Option<i64> {
    let (month, day) = (i64::from(month), i64::from(day));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    Some(days * 86_400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec))
}

/// A packed FAT date (bits 15..9 years since 1980, 8..5 month, 4..0 day) and time (bits
/// 15..11 hours, 10..5 minutes, 4..0 two-second units), as seconds since 1970 read as UTC.
pub fn from_dos(date: u16, time: u16) -> Option<i64> {
    from_fields(
        1980 + (date >> 9),
        (date >> 5) & 0xF,
        date & 0x1F,
        time >> 11,
        (time >> 5) & 0x3F,
        (time & 0x1F) * 2,
    )
}

/// An exFAT timestamp: the packed date and time, the 10 ms increments and the UTC offset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp {
    /// The date in the high 16 bits and the time in the low 16, as [`from_dos`] reads them.
    pub packed: u32,
    /// 10 ms units to add, 0 to 199 (only for creation and modification times).
    pub ten_ms: u8,
    /// Bit 7 set: bits 6..0 are the zone's offset from UTC in 15-minute units, signed.
    pub utc_offset: u8,
}

impl Timestamp {
    /// The timestamp as a Unix time, or `None` when its date is not one (see
    /// [`from_fields`]). An increment above 199 is read as 199.
    pub fn to_unix(self) -> Option<UnixTime> {
        let local = from_dos((self.packed >> 16) as u16, self.packed as u16)?;
        let ten_ms = u32::from(self.ten_ms.min(199));
        let mut secs = local + i64::from(ten_ms / 100);
        if self.utc_offset & 0x80 != 0 {
            // Sign-extend the 7-bit field; local time is UTC plus the offset.
            let quarters = i64::from(((self.utc_offset << 1) as i8) >> 1);
            secs -= quarters * 15 * 60;
        }
        Some((secs, (ten_ms % 100) * 10_000_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn packed(year: u32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> u32 {
        ((year - 1980) << 25) | (month << 21) | (day << 16) | (hour << 11) | (min << 5) | (sec / 2)
    }

    #[test]
    fn known_dates() {
        // `date -u -d ... +%s`.
        assert_eq!(from_fields(1980, 1, 1, 0, 0, 0), Some(315_532_800));
        assert_eq!(from_fields(2026, 1, 1, 0, 0, 0), Some(1_767_225_600));
        assert_eq!(from_fields(2000, 2, 29, 12, 34, 56), Some(951_827_696));
        assert_eq!(from_fields(2107, 12, 31, 23, 59, 58), Some(4_354_819_198));
        assert_eq!(from_fields(1980, 0, 0, 0, 0, 0), None);
        assert_eq!(from_fields(1980, 13, 1, 0, 0, 0), None);
        let p = packed(2026, 1, 1, 12, 34, 56);
        assert_eq!(from_dos((p >> 16) as u16, p as u16), Some(1_767_270_896));
    }

    #[test]
    fn increments_and_offsets_apply() {
        let t = Timestamp {
            packed: packed(2026, 1, 1, 12, 34, 56),
            ten_ms: 150,
            utc_offset: 0,
        };
        assert_eq!(t.to_unix(), Some((1_767_270_897, 500_000_000)));
        // UTC+1 (4 quarters): the UTC time is an hour earlier.
        let east = Timestamp {
            utc_offset: 0x80 | 4,
            ..t
        };
        assert_eq!(east.to_unix(), Some((1_767_270_897 - 3600, 500_000_000)));
        // UTC-5 (-20 quarters, 0x6C as 7-bit two's complement): five hours later.
        let west = Timestamp {
            utc_offset: 0x80 | 0x6C,
            ..t
        };
        assert_eq!(
            west.to_unix(),
            Some((1_767_270_897 + 5 * 3600, 500_000_000))
        );
        assert_eq!(Timestamp::default().to_unix(), None);
    }
}
