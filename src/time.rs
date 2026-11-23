//! RFC 3339 timestamps as Unix seconds, and short human durations.
//!
//! The civil-date math is Howard Hinnant's `days_from_civil` and
//! `civil_from_days`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)` into Unix seconds.
/// The fraction is dropped, rounding toward the past.
pub fn parse(text: &str) -> Option<i64> {
    parse_millis(text).map(|millis| millis.div_euclid(1000))
}

/// Like `parse`, in milliseconds. Digits past the third are dropped,
/// rounding toward the past.
pub fn parse_millis(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(range)?;
        part.iter().all(u8::is_ascii_digit).then(|| {
            part.iter()
                .fold(0i64, |value, digit| value * 10 + i64::from(digit - b'0'))
        })
    };
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if b[at] == b'.' {
        at += 1;
        let start = at;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            if at - start < 3 {
                millis = millis * 10 + i64::from(b[at] - b'0');
            }
            at += 1;
        }
        if at == start {
            return None;
        }
        for _ in at - start..3 {
            millis *= 10;
        }
    }
    let offset = match b.get(at)? {
        b'Z' | b'z' if at + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if at + 6 == b.len() && b[at + 3] == b':' => {
            let (hours, minutes) = (digits(at + 1..at + 3)?, digits(at + 4..at + 6)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3600 + minutes * 60;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) * 1000 + millis)
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// A duration in its two largest units, as in `6d 19h`, `3h 12m`, `45m`,
/// or `30s`. Negative durations read as `0s`.
pub fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours) = (seconds / 86_400, seconds % 86_400 / 3600);
    let (minutes, rest) = (seconds % 3600 / 60, seconds % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{rest}s"),
        (0, 0, _) => format!("{minutes}m"),
        (0, _, 0) => format!("{hours}h"),
        (0, _, _) => format!("{hours}h {minutes}m"),
        (_, 0, _) => format!("{days}d"),
        _ => format!("{days}d {hours}h"),
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// jiff's whole seconds, floored as ours are. `as_second` truncates
    /// toward zero, which differs before 1970.
    fn oracle(text: &str) -> Option<i64> {
        text.parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_second() - i64::from(t.subsec_nanosecond() < 0))
    }

    #[test]
    fn provider_timestamps_parse() {
        for text in [
            "2026-11-30T09:59:59.836229+00:00",
            "2026-11-28T15:34:08Z",
            "2026-11-23T16:03:17.563Z",
            "2026-11-23T16:31:07.631392526+01:00",
            "1970-01-01T00:00:00Z",
            "2000-02-29T23:59:59-05:30",
        ] {
            assert_eq!(parse(text), oracle(text), "{text}");
            assert!(parse(text).is_some(), "{text}");
        }
    }

    #[test]
    fn bad_timestamps_are_refused() {
        for text in [
            "2026-11-30",
            "2026-11-30T09:59:59",
            "2026-02-30T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "2026-11-30T24:00:00Z",
            "2026-11-30T09:59:59.Z",
            "2026-11-30T09:59:59+0100",
            "2026-11-30T09:59:59Zjunk",
            "2026-13-01T00:00:00Z",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn durations_show_their_two_largest_units() {
        for (seconds, text) in [
            (-5, "0s"),
            (30, "30s"),
            (45 * 60 + 9, "45m"),
            (3 * 3600, "3h"),
            (3 * 3600 + 12 * 60, "3h 12m"),
            (2 * 86_400 + 59, "2d"),
            (6 * 86_400 + 19 * 3600 + 5 * 60, "6d 19h"),
        ] {
            assert_eq!(duration(seconds), text);
        }
    }

    /// Parsing and formatting agree with jiff across years 0001 to 9999,
    /// with fractions and offsets.
    #[test]
    fn parsing_and_formatting_match_jiff() {
        let mut state = 0xbb67_ae85_84ca_a73bu64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let low = jiff::Timestamp::from_second(-62_135_596_800 + 86_400 * 2)
            .unwrap()
            .as_second();
        let high = jiff::Timestamp::from_second(253_402_300_799 - 86_400 * 2)
            .unwrap()
            .as_second();
        for _ in 0..100_000 {
            let seconds = low + (next() % (high - low) as u64) as i64;
            let formatted = format(seconds);
            assert_eq!(
                formatted,
                jiff::Timestamp::from_second(seconds).unwrap().to_string()
            );
            assert_eq!(parse(&formatted), Some(seconds));

            let offset_minutes = (next() % (24 * 60 * 2 - 1)) as i64 - (24 * 60 - 1);
            let sign = if offset_minutes < 0 { '-' } else { '+' };
            let local = seconds + offset_minutes * 60;
            let fraction = match next() % 3 {
                0 => String::new(),
                1 => format!(".{}", next() % 1000),
                _ => format!(".{:09}", next() % 1_000_000_000),
            };
            let base = format(local);
            let text = format!(
                "{}{fraction}{sign}{:02}:{:02}",
                &base[..19],
                offset_minutes.abs() / 60,
                offset_minutes.abs() % 60
            );
            assert_eq!(parse(&text), oracle(&text), "{text}");
        }
    }

    /// Milliseconds agree with jiff, floored as ours are.
    #[test]
    fn milliseconds_match_jiff() {
        let millis = |text: &str| {
            let t = text.parse::<jiff::Timestamp>().unwrap();
            t.as_nanosecond().div_euclid(1_000_000) as i64
        };
        for text in [
            "2026-10-12T05:36:41.397Z",
            "2026-10-12T05:36:41.3Z",
            "2026-10-12T05:36:41.39999Z",
            "2026-10-12T05:36:41Z",
            "1969-12-31T23:59:59.999Z",
            "1960-06-01T12:00:00.001-03:00",
            "2026-11-23T16:31:07.631392526+01:00",
        ] {
            assert_eq!(parse_millis(text), Some(millis(text)), "{text}");
        }
    }

    /// Anything we accept, jiff accepts with the same value.
    #[test]
    fn whatever_we_accept_jiff_reads_the_same() {
        let mut state = 0x3c6e_f372_fe94_f82bu64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let symbols = b"0123456789-:T.Z+tz ";
        let template = b"2026-11-23T16:03:17.563Z";
        for _ in 0..100_000 {
            let mut text = template.to_vec();
            for _ in 0..1 + next() % 3 {
                let at = (next() as usize) % text.len();
                text[at] = symbols[(next() as usize) % symbols.len()];
            }
            let text = String::from_utf8(text).unwrap();
            if let Some(seconds) = parse(&text) {
                assert_eq!(Some(seconds), oracle(&text), "{text}");
            }
        }
    }
}
