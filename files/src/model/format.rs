//! Sizes and times as people read them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Decimal units like GNOME: "512 bytes", "2.1 kB", "98 kB", "412 GB".
pub fn human_size(bytes: u64) -> String {
    match bytes {
        0 => return "0 bytes".into(),
        1 => return "1 byte".into(),
        2..=999 => return format!("{bytes} bytes"),
        _ => {}
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 999.5 && unit < 5 {
        v /= 1000.0;
        unit += 1;
    }
    let name = ["bytes", "kB", "MB", "GB", "TB", "PB"][unit];
    if v < 9.95 {
        format!("{v:.1} {name}")
    } else {
        format!("{v:.0} {name}")
    }
}

/// Broken-down local time (the fields we use from `struct tm`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tm {
    pub year: i32,
    /// 1..=12
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

/// Mirrors the glibc/musl `struct tm` layout.
#[repr(C)]
struct RawTm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *const u8,
}

unsafe extern "C" {
    fn localtime_r(t: *const i64, out: *mut RawTm) -> *mut RawTm;
}

/// `t` in the local timezone, falling back to UTC if libc can't say.
pub(crate) fn local_tm(t: SystemTime) -> Tm {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let mut raw = RawTm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: both pointers are valid for the call; localtime_r is thread-safe.
    let ok = unsafe { !localtime_r(&secs, &mut raw).is_null() };
    if !ok {
        return utc_tm(secs);
    }
    Tm {
        year: raw.year + 1900,
        month: (raw.mon + 1) as u32,
        day: raw.mday as u32,
        hour: raw.hour as u32,
        min: raw.min as u32,
        sec: raw.sec as u32,
    }
}

fn utc_tm(secs: i64) -> Tm {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let (year, month, day) = civil_from_days(days);
    Tm {
        year,
        month,
        day,
        hour: rem / 3600,
        min: rem % 3600 / 60,
        sec: rem % 60,
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
pub(crate) fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400) as i32 + (m <= 2) as i32;
    (y, m, d)
}

/// (year, month, day) to days since 1970-01-01.
pub(crate) fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "Today 02:29", "Yesterday 23:10", "Oct 18", or "2025-03-01", in local time.
pub fn human_time(t: SystemTime, now: SystemTime) -> String {
    format_tm(local_tm(t), local_tm(now))
}

fn format_tm(t: Tm, now: Tm) -> String {
    let day = |tm: Tm| days_from_civil(tm.year, tm.month, tm.day);
    let age = day(now) - day(t);
    if age == 0 {
        format!("Today {:02}:{:02}", t.hour, t.min)
    } else if age == 1 {
        format!("Yesterday {:02}:{:02}", t.hour, t.min)
    } else if t.year == now.year {
        format!("{} {}", MONTHS[t.month as usize - 1], t.day)
    } else {
        format!("{:04}-{:02}-{:02}", t.year, t.month, t.day)
    }
}

/// Seconds since the epoch as a `SystemTime`.
pub(crate) fn from_unix(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(-secs as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tm(year: i32, month: u32, day: u32, hour: u32, min: u32) -> Tm {
        Tm {
            year,
            month,
            day,
            hour,
            min,
            sec: 0,
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1), "1 byte");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(2100), "2.1 kB");
        assert_eq!(human_size(98_000), "98 kB");
        assert_eq!(human_size(1_200_000), "1.2 MB");
        assert_eq!(human_size(412_000_000_000), "412 GB");
        assert_eq!(human_size(2_000_000_000_000), "2.0 TB");
        assert_eq!(human_size(999_999), "1.0 MB");
    }

    #[test]
    fn times() {
        let now = tm(2026, 10, 19, 3, 0);
        assert_eq!(format_tm(tm(2026, 10, 19, 2, 29), now), "Today 02:29");
        assert_eq!(format_tm(tm(2026, 10, 18, 23, 10), now), "Yesterday 23:10");
        assert_eq!(format_tm(tm(2026, 10, 1, 9, 0), now), "Oct 1");
        assert_eq!(format_tm(tm(2025, 3, 1, 9, 0), now), "2025-03-01");
        // Yesterday across a month boundary.
        let now = tm(2026, 11, 1, 1, 0);
        assert_eq!(format_tm(tm(2026, 10, 31, 22, 5), now), "Yesterday 22:05");
    }

    #[test]
    fn civil_round_trip() {
        for d in [-1000, 0, 1, 365, 20_000, 30_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn local_time_is_sane() {
        let n = SystemTime::now();
        let t = local_tm(n);
        assert!(t.year >= 2024 && (1..=12).contains(&t.month));
        assert_eq!(human_time(n, n).split(' ').next(), Some("Today"));
    }
}
