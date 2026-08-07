//! Date/time helpers shared across modules. The codebase deliberately avoids
//! a date crate; these cover everything needed (SQLite timestamps, RFC 822,
//! ISO 8601) with the Howard Hinnant civil-date algorithms.

/// Days since Unix epoch for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Convert days since Unix epoch to (year, month, day).
pub fn ymd_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Current UTC time as `YYYY-MM-DD HH:MM:SS` (SQLite datetime format).
pub fn now_sqlite() -> String {
    let secs = unix_seconds();
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = ymd_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Current UTC time as RFC 3339-ish `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso8601() -> String {
    format!("{}Z", now_sqlite().replace(' ', "T"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_roundtrip() {
        // 1970-01-01 ↔ 0
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(ymd_from_days(0), (1970, 1, 1));
        // Leap day 2024-02-29
        let days = days_from_civil(2024, 2, 29);
        assert_eq!(ymd_from_days(days), (2024, 2, 29));
        // Known epoch offset: 2026-08-07
        let days = days_from_civil(2026, 8, 7);
        assert_eq!(ymd_from_days(days), (2026, 8, 7));
        assert!(days > days_from_civil(2026, 8, 6));
    }

    #[test]
    fn now_sqlite_shape() {
        let s = now_sqlite();
        assert_eq!(s.len(), 19);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    #[test]
    fn now_iso8601_shape() {
        let s = now_iso8601();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20);
        assert_eq!(&s[10..11], "T");
    }
}
