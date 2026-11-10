pub fn days_from_civil(y: i64, m: u32, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD` (UTC midnight) to epoch seconds.
pub fn parse_ymd(s: &str) -> Option<i64> {
    let mut it = s.splitn(3, '-');
    let y = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    ((1..=12).contains(&m)).then(|| days_from_civil(y, m, d) * 86_400)
}

pub fn ymd(epoch: i64) -> String {
    let (y, m, d) = civil_from_days(epoch.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM` (UTC).
pub fn ymd_hm(epoch: i64) -> String {
    let s = epoch.rem_euclid(86_400);
    format!("{} {:02}:{:02}", ymd(epoch), s / 3600, s % 3600 / 60)
}

/// RFC 3339 UTC, e.g. `2026-10-22T15:46:21Z`.
pub fn iso(epoch: i64) -> String {
    let s = epoch.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        ymd(epoch),
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(parse_ymd("1970-01-01"), Some(0));
        assert_eq!(parse_ymd("2001-11-09"), Some(1_005_264_000));
        assert_eq!(ymd(1_005_345_232), "2001-11-09");
        assert_eq!(ymd(-1), "1969-12-31");
        assert_eq!(ymd_hm(1_005_345_232), "2001-11-09 22:33");
        assert_eq!(iso(1_005_345_232), "2001-11-09T22:33:52Z");
        assert_eq!(parse_ymd("2001-13-01"), None);
    }
}
