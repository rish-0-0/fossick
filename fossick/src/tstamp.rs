//! Hand-rolled timestamp extraction and epoch/civil conversion.
//!
//! Formats recognized (scanned in the first 128 bytes of a line, in order):
//! ISO-8601, common-log `[15/Sep/2026:10:11:12 +0000]`, syslog `Sep 15 10:11:12`
//! (year inferred from receipt time), and a 10/13-digit epoch at line start.
//! Naive timestamps (no zone) are treated as UTC. Results more than ±10 years
//! from receipt time are rejected.

const SCAN: usize = 128;
const MS_10Y: u64 = 10 * 365 * 86_400_000;
const MONTHS: [&[u8; 3]; 12] =
    [b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec"];

/// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Inverse of days_from_civil.
pub fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// Date components as individual args beat a one-off struct for a single internal caller.
#[allow(clippy::too_many_arguments)]
fn to_ms(y: i64, mo: u64, d: u64, h: u64, mi: u64, s: u64, ms: u64, tz_off_min: i64) -> Option<u64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + s) as i64 - tz_off_min * 60;
    u64::try_from(secs * 1000 + ms as i64).ok()
}

/// Render epoch ms as ISO-8601 UTC, e.g. `2026-09-15T10:11:12.345Z`.
pub fn to_iso(ms: u64) -> String {
    let (days, rem) = (ms / 86_400_000, ms % 86_400_000);
    let (y, mo, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

fn digits(b: &[u8], n: usize) -> Option<u64> {
    if b.len() < n || !b[..n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b[..n].iter().fold(0, |a, c| a * 10 + (c - b'0') as u64))
}

/// `.fff` fraction → (ms, bytes consumed).
fn frac(b: &[u8]) -> (u64, usize) {
    if b.first() != Some(&b'.') {
        return (0, 0);
    }
    let n = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 {
        return (0, 0);
    }
    let mut ms = 0;
    for i in 0..3 {
        ms = ms * 10 + if i < n { (b[1 + i] - b'0') as u64 } else { 0 };
    }
    (ms, 1 + n)
}

/// `Z`, `+hh:mm`, `+hhmm` → (offset minutes, bytes consumed). None = no zone.
fn zone(b: &[u8]) -> Option<(i64, usize)> {
    match b.first() {
        Some(b'Z') => Some((0, 1)),
        Some(&s @ (b'+' | b'-')) => {
            let sign = if s == b'+' { 1 } else { -1 };
            let h = digits(&b[1..], 2)?;
            let (m, len) = if b.get(3) == Some(&b':') { (digits(&b[4..], 2)?, 6) } else { (digits(&b[3..], 2)?, 5) };
            (h <= 14 && m <= 59).then_some((sign * (h * 60 + m) as i64, len))
        }
        _ => None,
    }
}

fn month_no(b: &[u8]) -> Option<u64> {
    MONTHS.iter().position(|m| b.starts_with(*m)).map(|i| i as u64 + 1)
}

/// `yyyy-mm-dd[T ]hh:mm:ss(.fff)?(Z|±hh:mm)?` starting at `b`.
fn try_iso(b: &[u8]) -> Option<u64> {
    let y = digits(b, 4)? as i64;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let (mo, d) = (digits(&b[5..], 2)?, digits(&b[8..], 2)?);
    if !matches!(b.get(10), Some(b'T' | b' ')) || b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
        return None;
    }
    let (h, mi, s) = (digits(&b[11..], 2)?, digits(&b[14..], 2)?, digits(&b[17..], 2)?);
    let (ms, fl) = frac(&b[19..]);
    let tz = zone(&b[19 + fl..]).map(|(t, _)| t).unwrap_or(0);
    to_ms(y, mo, d, h, mi, s, ms, tz)
}

/// `dd/Mon/yyyy:hh:mm:ss ±zzzz` starting at `b` (common/combined log format).
fn try_common_log(b: &[u8]) -> Option<u64> {
    let d = digits(b, 2)?;
    if b.get(2) != Some(&b'/') || b.get(6) != Some(&b'/') {
        return None;
    }
    let mo = month_no(&b[3..])?;
    let y = digits(&b[7..], 4)? as i64;
    if b.get(11) != Some(&b':') || b.get(14) != Some(&b':') || b.get(17) != Some(&b':') {
        return None;
    }
    let (h, mi, s) = (digits(&b[12..], 2)?, digits(&b[15..], 2)?, digits(&b[18..], 2)?);
    let tz = if b.get(20) == Some(&b' ') { zone(&b[21..]).map(|(t, _)| t).unwrap_or(0) } else { 0 };
    to_ms(y, mo, d, h, mi, s, 0, tz)
}

/// `Mon dd hh:mm:ss` (RFC 3164 syslog) starting at `b`; year from receipt time.
fn try_syslog(b: &[u8], now_ms: u64) -> Option<u64> {
    let mo = month_no(b)?;
    if b.get(3) != Some(&b' ') {
        return None;
    }
    let d = match (b.get(4), b.get(5)) {
        (Some(b' '), Some(c)) if c.is_ascii_digit() => (c - b'0') as u64,
        _ => digits(&b[4..], 2)?,
    };
    if b.get(6) != Some(&b' ') || b.get(9) != Some(&b':') || b.get(12) != Some(&b':') {
        return None;
    }
    let (h, mi, s) = (digits(&b[7..], 2)?, digits(&b[10..], 2)?, digits(&b[13..], 2)?);
    let (now_y, _, _) = civil_from_days((now_ms / 86_400_000) as i64);
    let t = to_ms(now_y, mo, d, h, mi, s, 0, 0)?;
    // A timestamp more than a week in the future means it was from last year.
    if t > now_ms + 7 * 86_400_000 {
        return to_ms(now_y - 1, mo, d, h, mi, s, 0, 0);
    }
    Some(t)
}

/// 10-digit (seconds) or 13-digit (ms) epoch at line start.
fn try_epoch(b: &[u8]) -> Option<u64> {
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    match n {
        10 => digits(b, 10).map(|s| s * 1000),
        13 => digits(b, 13),
        _ => None,
    }
}

fn sane(t: u64, now_ms: u64) -> Option<u64> {
    (t + MS_10Y > now_ms && t < now_ms + MS_10Y).then_some(t)
}

/// Extract a timestamp from a log line; `None` means "use receipt time".
pub fn extract(line: &[u8], now_ms: u64) -> Option<u64> {
    let b = &line[..line.len().min(SCAN)];
    for i in 0..b.len() {
        if b[i].is_ascii_digit() {
            if let Some(t) = try_iso(&b[i..]).and_then(|t| sane(t, now_ms)) {
                return Some(t);
            }
        }
        if b[i] == b'[' {
            if let Some(t) = try_common_log(&b[i + 1..]).and_then(|t| sane(t, now_ms)) {
                return Some(t);
            }
        }
    }
    if let Some(t) = try_syslog(b, now_ms).and_then(|t| sane(t, now_ms)) {
        return Some(t);
    }
    try_epoch(b).and_then(|t| sane(t, now_ms))
}

/// Parse an earliest=/latest= value: `now`, `-15m/-4h/-7d/-30s`, epoch s/ms, or ISO.
pub fn parse_timespec(s: &str, now_ms: u64) -> Option<u64> {
    if s == "now" {
        return Some(now_ms);
    }
    if let Some(rest) = s.strip_prefix('-') {
        let unit = rest.chars().last()?;
        let n: u64 = rest[..rest.len() - 1].parse().ok()?;
        let ms = n * match unit {
            's' => 1000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            _ => return None,
        };
        return Some(now_ms.saturating_sub(ms));
    }
    if let Some(t) = try_iso(s.as_bytes()) {
        return Some(t);
    }
    try_epoch(s.as_bytes())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-09-15T12:00:00Z
    const NOW: u64 = 1_789_473_600_000;

    #[test]
    fn civil_roundtrip() {
        for days in [-141_427, -1, 0, 1, 18_993, 20_713, 40_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn iso_variants() {
        let cases = [
            ("2026-09-15T10:11:12Z", 1_789_467_072_000),
            ("2026-09-15 10:11:12", 1_789_467_072_000), // naive → UTC
            ("2026-09-15T10:11:12.345Z", 1_789_467_072_345),
            ("2026-09-15T10:11:12+05:30", 1_789_467_072_000 - 19_800_000),
            ("2026-09-15T10:11:12-0700", 1_789_467_072_000 + 25_200_000),
            ("level=info ts=2026-09-15T10:11:12Z msg=hi", 1_789_467_072_000), // mid-line
        ];
        for (s, want) in cases {
            assert_eq!(extract(s.as_bytes(), NOW), Some(want), "{s}");
        }
    }

    #[test]
    fn common_log() {
        let line = br#"127.0.0.1 - - [15/Sep/2026:10:11:12 +0000] "GET / HTTP/1.1" 200 5"#;
        assert_eq!(extract(line, NOW), Some(1_789_467_072_000));
        let est = br#"[15/Sep/2026:10:11:12 -0500] x"#;
        assert_eq!(extract(est, NOW), Some(1_789_467_072_000 + 18_000_000));
    }

    #[test]
    fn syslog_infers_year() {
        assert_eq!(extract(b"Sep 15 10:11:12 web01 sshd[1]: ok", NOW), Some(1_789_467_072_000));
        // Single-digit, space-padded day.
        assert_eq!(extract(b"Sep  5 00:00:00 h x", NOW), Some(1_788_566_400_000));
        // A December date seen in September is from last year.
        let dec = extract(b"Dec 31 23:59:59 h x", NOW).unwrap();
        assert!(dec < NOW);
    }

    #[test]
    fn epoch_at_start() {
        assert_eq!(extract(b"1789467072 hello", NOW), Some(1_789_467_072_000));
        assert_eq!(extract(b"1789467072345 hello", NOW), Some(1_789_467_072_345));
        assert_eq!(extract(b"12345 not-an-epoch", NOW), None); // wrong digit count
    }

    #[test]
    fn garbage_and_out_of_range_fall_back() {
        assert_eq!(extract(b"no timestamp here", NOW), None);
        assert_eq!(extract(b"1990-01-01T00:00:00Z too old", NOW), None);
        assert_eq!(extract(b"2026-13-40T99:99:99Z", NOW), None);
        assert_eq!(extract(b"", NOW), None);
    }

    #[test]
    fn timespecs() {
        assert_eq!(parse_timespec("now", NOW), Some(NOW));
        assert_eq!(parse_timespec("-15m", NOW), Some(NOW - 900_000));
        assert_eq!(parse_timespec("-7d", NOW), Some(NOW - 7 * 86_400_000));
        assert_eq!(parse_timespec("2026-09-15T00:00:00Z", NOW), Some(1_789_430_400_000));
        assert_eq!(parse_timespec("1789467072", NOW), Some(1_789_467_072_000));
        assert_eq!(parse_timespec("junk", NOW), None);
    }

    #[test]
    fn to_iso_formats() {
        assert_eq!(to_iso(1_789_467_072_345), "2026-09-15T10:11:12.345Z");
        assert_eq!(to_iso(0), "1970-01-01T00:00:00.000Z");
    }
}
