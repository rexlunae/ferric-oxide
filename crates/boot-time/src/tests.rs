use super::*;

fn leap_day() -> FirmwareTime {
    FirmwareTime {
        year: 2024,
        month: 2,
        day: 29,
        hour: 12,
        minute: 34,
        second: 56,
        nanosecond: 123_456_789,
        timezone: 0,
        daylight: 0,
    }
}

#[test]
fn known_dates_and_subseconds() {
    let date = leap_day();
    assert_eq!(date.unix_nanos().unwrap(), 1_709_210_096_123_456_789);
    assert_eq!(
        FirmwareTime {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            nanosecond: 0,
            ..date
        }
        .unix_nanos()
        .unwrap(),
        0
    );
    assert_eq!(
        FirmwareTime {
            year: 2000,
            month: 2,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
            nanosecond: 0,
            ..date
        }
        .unix_nanos()
        .unwrap(),
        951_782_400_000_000_000
    );
    assert_eq!(
        FirmwareTime {
            year: 9999,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
            nanosecond: 999_999_999,
            ..date
        }
        .unix_nanos()
        .unwrap(),
        MAX_UNIX_NS
    );
}

#[test]
fn uefi_timezone_sign_and_daylight() {
    let date = leap_day();
    let utc = date.unix_nanos().unwrap();
    // UEFI PST is +480, PDT +420, India -330 (the reverse of ISO offsets).
    for (timezone, seconds) in [
        (480, 28_800_i128),
        (420, 25_200),
        (-330, -19_800),
        (1440, 86_400),
        (-1440, -86_400),
        (2047, 0),
    ] {
        for daylight in 0..=3 {
            let value = FirmwareTime {
                timezone,
                daylight,
                ..date
            }
            .unix_nanos()
            .unwrap();
            assert_eq!(value as i128, utc as i128 + seconds * NANOS_PER_SEC as i128);
        }
    }
}

#[test]
fn rejects_invalid_firmware_fields() {
    let date = leap_day();
    for invalid in [
        FirmwareTime { year: 1899, ..date },
        FirmwareTime {
            year: 10000,
            ..date
        },
        FirmwareTime { year: 2023, ..date },
        FirmwareTime { year: 1900, ..date },
        FirmwareTime { year: 2100, ..date },
        FirmwareTime { month: 0, ..date },
        FirmwareTime { month: 13, ..date },
        FirmwareTime {
            month: 4,
            day: 31,
            ..date
        },
        FirmwareTime { day: 0, ..date },
        FirmwareTime { day: 30, ..date },
        FirmwareTime { hour: 24, ..date },
        FirmwareTime { minute: 60, ..date },
        FirmwareTime { second: 60, ..date },
        FirmwareTime {
            nanosecond: 1_000_000_000,
            ..date
        },
        FirmwareTime {
            timezone: 1441,
            ..date
        },
        FirmwareTime {
            timezone: -1441,
            ..date
        },
        FirmwareTime {
            daylight: 4,
            ..date
        },
        FirmwareTime { year: 1969, ..date },
    ] {
        assert!(invalid.unix_nanos().is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn sample_counts_boot_delay_once() {
    let wall = leap_day().unix_nanos().unwrap();
    let sample_counter = 120 * NANOS_PER_SEC;
    let clock = BootTime::from_sample(wall, sample_counter).unwrap();
    assert_eq!(clock.offset_ns() + sample_counter, wall);
    assert_eq!(
        clock.offset_ns() + sample_counter + 17 * NANOS_PER_SEC,
        wall + 17 * NANOS_PER_SEC
    );
    assert!(BootTime::from_sample(0, 1).is_err());
    assert!(BootTime::from_sample(MAX_UNIX_NS + 1, 0).is_err());
}

#[test]
fn handoff_round_trip_and_missing_fallback() {
    extern crate std;
    let clock = BootTime::from_sample(leap_day().unix_nanos().unwrap(), 123).unwrap();
    let env = std::format!("OTHER=abc\n{ENV_KEY}={clock}\nLAST=def\n");
    assert_eq!(BootTime::from_env(env.as_bytes()).unwrap(), Some(clock));
    assert_eq!(BootTime::from_env(b"OTHER=abc\n").unwrap(), None);
    assert_eq!(BootTime::from_env(b"").unwrap(), None);
}

#[test]
fn rejects_corrupted_handoff() {
    for value in [
        "",
        "+1",
        "-1",
        "0x123",
        " 1",
        "1 ",
        "xyz",
        "ffffffffffffffffffffffffffffffff",
        "100000000000000000000000000000000",
    ] {
        assert!(value.parse::<BootTime>().is_err(), "accepted {value}");
    }
    assert!(BootTime::from_env(b"BOOT_TIME_OFFSET_NS=1\nBOOT_TIME_OFFSET_NS=2\n").is_err());
    assert!(BootTime::from_env(b"BOOT_TIME_OFFSET_NS=\xff\n").is_err());
}
