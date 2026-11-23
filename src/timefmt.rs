//! Timestamp helpers that reproduce chrono 0.4.45's behavior.
//!
//! octorus used to call `chrono` for RFC 3339, local strftime, relative times,
//! and check durations. RUSTSEC-2026-0543 marks chrono unmaintained, so those
//! call sites go through jiff. The golden tests in this module pin the chrono
//! 0.4.45 results: same strings, same parse successes, same instant ordering,
//! and the same whole-second truncation (including leap seconds and a negative
//! fraction that still formats as `0s`).
//!
//! Persisted files store timestamps as strings (`to_rfc3339` wrote `+00:00`,
//! not `Z`). New writes keep that spelling. The PR cache compares `updated_at`
//! with string equality and does not parse it.

use std::cmp::Ordering;

use jiff::Timestamp;

/// Inclusive range of `chrono 0.4.45`'s `DateTime::<Utc>::from_timestamp(secs, 0)`.
///
/// Seconds outside this range made `format_relative_time_from_epoch` return
/// `"unknown"`. jiff's `Timestamp` range is smaller, so the epoch path keeps
/// the check in integer arithmetic instead of asking jiff.
const MIN_CHRONO_TIMESTAMP: i64 = -8_334_601_228_800;
const MAX_CHRONO_TIMESTAMP: i64 = 8_210_266_876_799;

/// A UTC instant, with leap seconds stored the way chrono 0.4.45 stored them.
///
/// `nanos` is normally `0..1_000_000_000`. A parsed leap second (`ss == 60`)
/// is the previous second plus `1_000_000_000` nanoseconds (plus any fraction).
/// Ordering and whole-second differences follow chrono's `NaiveDateTime`,
/// which is not the same as dropping the leap second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    secs: i64,
    nanos: u32,
}

impl Instant {
    fn from_timestamp(ts: Timestamp) -> Self {
        Self {
            secs: ts.as_second(),
            nanos: u32::try_from(ts.subsec_nanosecond()).unwrap_or(0),
        }
    }
}

/// `chrono::Utc::now().to_rfc3339()`: auto SI fractional seconds and `+00:00`.
pub fn utc_now_rfc3339() -> String {
    format_utc_rfc3339(Timestamp::now())
}

/// Format `ts` the way `DateTime<Utc>::to_rfc3339` did (`SecondsFormat::AutoSi`,
/// `use_z = false`).
pub(crate) fn format_utc_rfc3339(ts: Timestamp) -> String {
    let nanos = ts.subsec_nanosecond();
    let shown = ts.display_with_offset(jiff::tz::Offset::UTC);
    match nanos {
        0 => format!("{shown:.0}"),
        n if n % 1_000_000 == 0 => format!("{shown:.3}"),
        n if n % 1_000 == 0 => format!("{shown:.6}"),
        _ => format!("{shown:.9}"),
    }
}

/// `chrono::Local::now().format("%Y%m%d-%H%M%S")`, used for migration backups.
pub fn local_backup_timestamp() -> String {
    format_local_now("%Y%m%d-%H%M%S")
}

/// `chrono::Local::now().format("%H:%M:%S")`, used for AI Rally log lines.
pub fn local_log_timestamp() -> String {
    format_local_now("%H:%M:%S")
}

fn format_local_now(format: &str) -> String {
    Timestamp::now()
        .to_zoned_datetime(local_time_zone())
        .strftime(format)
        .to_string()
}

/// Zone chrono's `Local` would use.
///
/// A usable `TZ` wins, including the empty value that both libraries treat as
/// UTC. An unusable `TZ` (`Invalid/Zone`, `garbage`) makes jiff report
/// `Etc/Unknown`, which formats as UTC. Chrono instead falls back to
/// `/etc/localtime`, then UTC.
fn local_time_zone() -> jiff::tz::TimeZone {
    if let Ok(zone) = jiff::tz::TimeZone::try_system() {
        return zone;
    }
    fallback_system_zone()
}

fn fallback_system_zone() -> jiff::tz::TimeZone {
    #[cfg(unix)]
    {
        if let Some(zone) = zone_from_localtime_file("/etc/localtime") {
            return zone;
        }
    }
    jiff::tz::TimeZone::UTC
}

#[cfg(unix)]
fn zone_from_localtime_file(path: &str) -> Option<jiff::tz::TimeZone> {
    if let Some(zone) = zoneinfo_symlink(path) {
        return Some(zone);
    }
    let bytes = std::fs::read(path).ok()?;
    jiff::tz::TimeZone::tzif("localtime", &bytes).ok()
}

#[cfg(unix)]
fn zoneinfo_symlink(path: &str) -> Option<jiff::tz::TimeZone> {
    let target = std::fs::read_link(path).ok()?;
    let target = target.to_str()?;
    let needle = "zoneinfo/";
    let name = target.get(target.rfind(needle)? + needle.len()..)?;
    jiff::tz::TimeZone::get(name).ok()
}

/// Parse with the acceptance rules of `DateTime::parse_from_rfc3339` in chrono 0.4.45.
///
/// jiff's own parser is wider in some places (`+00`, `+0000`, `+24:00`) and
/// narrower in others (a 10th fractional digit, U+2212 minus, the last
/// nanosecond of year 9999). Those differences would change relative times and
/// comment order, so this parser follows chrono.
pub fn parse_rfc3339(input: &str) -> Option<Instant> {
    let bytes = input.as_bytes();
    if bytes.len() < 19 || !bytes[..19].is_ascii() {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !matches!(bytes[10], b'T' | b't' | b' ')
    {
        return None;
    }

    let year = four_digits(bytes, 0)?;
    let month = two_digits(bytes, 5)?;
    let day = two_digits(bytes, 8)?;
    let hour = two_digits(bytes, 11)?;
    let minute = two_digits(bytes, 14)?;
    let mut second = two_digits(bytes, 17)?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut extra_nanos = 0u32;
    if second == 60 {
        second = 59;
        extra_nanos = 1_000_000_000;
    }

    let mut rest = &input[19..];
    let fraction = if let Some(digits) = rest.strip_prefix('.') {
        let (next, nanos) = take_fraction(digits)?;
        rest = next;
        nanos
    } else {
        0
    };
    let offset = take_offset(rest)?;
    let nanos = extra_nanos.checked_add(fraction)?;
    if nanos > 1_999_999_999 {
        return None;
    }
    let unix = civil_unix_seconds(year, month, day, hour, minute, second)?;
    Some(Instant {
        secs: unix - i64::from(offset),
        nanos,
    })
}

/// Chrono `DateTime<FixedOffset>` ordering, including unparseable values (`None < Some`).
pub fn cmp_rfc3339(left: &str, right: &str) -> Ordering {
    parse_rfc3339(left).cmp(&parse_rfc3339(right))
}

/// The PR cache reopen check. Spelling matters: `Z` and `+00:00` are not equal.
pub(crate) fn pr_updated_at_unchanged(cached: &str, fresh: &str) -> bool {
    cached == fresh
}

pub(crate) fn format_relative_time(iso_date: &str) -> String {
    format_relative_time_at(iso_date, Instant::from_timestamp(Timestamp::now()))
}

pub(crate) fn format_relative_time_at(iso_date: &str, now: Instant) -> String {
    let Some(dt) = parse_rfc3339(iso_date) else {
        return iso_date.to_string();
    };
    render_relative(whole_seconds(now, dt))
}

pub(crate) fn format_relative_time_from_epoch(epoch_seconds: i64) -> String {
    format_relative_epoch_at(epoch_seconds, Instant::from_timestamp(Timestamp::now()))
}

pub(crate) fn format_relative_epoch_at(epoch_seconds: i64, now: Instant) -> String {
    if !(MIN_CHRONO_TIMESTAMP..=MAX_CHRONO_TIMESTAMP).contains(&epoch_seconds) {
        return "unknown".to_string();
    }
    render_relative(whole_seconds(
        now,
        Instant {
            secs: epoch_seconds,
            nanos: 0,
        },
    ))
}

/// Whole seconds between two check timestamps, rendered as the checks list does.
pub(crate) fn format_check_duration(started: &str, completed: &str) -> String {
    let (Some(start), Some(end)) = (parse_rfc3339(started), parse_rfc3339(completed)) else {
        return "-".to_string();
    };
    let secs = whole_seconds(end, start);
    if secs < 0 {
        return "-".to_string();
    }
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let remaining_secs = secs % 60;
        format!("{mins}m {remaining_secs:02}s")
    }
}

/// Whole seconds of `later - earlier`, matching `TimeDelta::num_seconds`.
///
/// Truncates toward zero. A negative fraction whose magnitude is under one
/// second therefore reports `0`, which the checks list renders as `0s`.
///
/// A leap second (`nanos >= 1_000_000_000`) is counted from the UTC
/// time-of-day, the way `NaiveTime::signed_duration_since` does. A `:60` that
/// is not `23:59:60` still shifts the result by one second when the
/// time-of-day order says so.
fn whole_seconds(later: Instant, earlier: Instant) -> i64 {
    let mut secs = later.secs - earlier.secs;
    let mut frac = i64::from(later.nanos) - i64::from(earlier.nanos);
    let later_tod = later.secs.rem_euclid(86_400);
    let earlier_tod = earlier.secs.rem_euclid(86_400);
    if later_tod > earlier_tod && earlier.nanos >= 1_000_000_000 {
        secs += 1;
    } else if later_tod < earlier_tod && later.nanos >= 1_000_000_000 {
        secs -= 1;
    }
    secs += frac.div_euclid(1_000_000_000);
    frac = frac.rem_euclid(1_000_000_000);
    if secs < 0 && frac > 0 {
        secs + 1
    } else {
        secs
    }
}

fn render_relative(seconds: i64) -> String {
    if seconds < 0 {
        return "just now".to_string();
    }
    let minutes = seconds / 60;
    let hours = seconds / 3_600;
    let days = seconds / 86_400;
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;
    if seconds < 60 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else if days < 7 {
        format!("{days}d ago")
    } else if weeks < 5 {
        format!("{weeks}w ago")
    } else if months < 12 {
        format!("{months}mo ago")
    } else {
        format!("{years}y ago")
    }
}

fn civil_unix_seconds(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    let date = jiff::civil::Date::new(
        i16::try_from(year).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
    )
    .ok()?;
    let epoch = jiff::civil::Date::new(1970, 1, 1).expect("unix epoch is a valid date");
    let days = i64::from(date.since((jiff::Unit::Day, epoch)).ok()?.get_days());
    Some(days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))
}

fn four_digits(bytes: &[u8], index: usize) -> Option<u32> {
    Some(
        digit(bytes[index])? * 1000
            + digit(bytes[index + 1])? * 100
            + digit(bytes[index + 2])? * 10
            + digit(bytes[index + 3])?,
    )
}

fn two_digits(bytes: &[u8], index: usize) -> Option<u32> {
    Some(digit(bytes[index])? * 10 + digit(bytes[index + 1])?)
}

fn digit(byte: u8) -> Option<u32> {
    byte.is_ascii_digit().then_some(u32::from(byte - b'0'))
}

/// `digits` is the text after the decimal point. Returns the remainder and nanos.
fn take_fraction(digits: &str) -> Option<(&str, u32)> {
    let mut value: u32 = 0;
    let mut counted = 0usize;
    let mut consumed = 0usize;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            break;
        }
        if counted < 9 {
            value = value * 10 + u32::from(byte - b'0');
            counted += 1;
        }
        consumed += 1;
    }
    if counted == 0 {
        return None;
    }
    const SCALE: [u32; 10] = [
        0,
        100_000_000,
        10_000_000,
        1_000_000,
        100_000,
        10_000,
        1_000,
        100,
        10,
        1,
    ];
    Some((&digits[consumed..], value * SCALE[counted]))
}

/// Chrono accepts `Z`/`z`, `±HH:MM`, and U+2212 as the minus sign.
/// The offset must be strictly inside ±24 hours, and nothing may follow it.
fn take_offset(rest: &str) -> Option<i32> {
    if rest == "Z" || rest == "z" {
        return Some(0);
    }
    let mut chars = rest.chars();
    let sign = match chars.next()? {
        '+' => 1i32,
        '-' | '\u{2212}' => -1,
        _ => return None,
    };
    let body = chars.as_str().as_bytes();
    if body.len() != 5 || body[2] != b':' {
        return None;
    }
    let hours = two_digits(body, 0)?;
    let minutes = two_digits(body, 3)?;
    if minutes > 59 {
        return None;
    }
    let seconds = hours * 3_600 + minutes * 60;
    if seconds >= 86_400 {
        return None;
    }
    Some(sign * i32::try_from(seconds).ok()?)
}

/// `Timestamp::now()` minus a signed duration, for tests that used
/// `Utc::now() - chrono::Duration`.
#[cfg(test)]
pub(crate) fn unix_seconds_before_now(duration: jiff::SignedDuration) -> i64 {
    (Timestamp::now() - duration).as_second()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `2024-06-15T12:00:00Z`, the fixed "now" the chrono probe used.
    const NOW: Instant = Instant {
        secs: 1_718_452_800,
        nanos: 0,
    };

    fn rfc3339_of(secs: i64, nanos: i32) -> String {
        format_utc_rfc3339(Timestamp::new(secs, nanos).expect("timestamp in jiff range"))
    }

    #[test]
    fn golden_rfc3339_matches_chrono_to_rfc3339() {
        // Captured from chrono 0.4.45 `DateTime<Utc>::to_rfc3339`.
        let cases = [
            (0, 0, "1970-01-01T00:00:00+00:00"),
            (1, 0, "1970-01-01T00:00:01+00:00"),
            (-1, 0, "1969-12-31T23:59:59+00:00"),
            (-1, 1, "1969-12-31T23:59:59.000000001+00:00"),
            (-1, 500_000_000, "1969-12-31T23:59:59.500+00:00"),
            (1_700_000_000, 0, "2023-11-14T22:13:20+00:00"),
            (1_700_000_000, 100_000_000, "2023-11-14T22:13:20.100+00:00"),
            (1_700_000_000, 120_000_000, "2023-11-14T22:13:20.120+00:00"),
            (1_700_000_000, 123_000_000, "2023-11-14T22:13:20.123+00:00"),
            (
                1_700_000_000,
                123_400_000,
                "2023-11-14T22:13:20.123400+00:00",
            ),
            (
                1_700_000_000,
                123_456_000,
                "2023-11-14T22:13:20.123456+00:00",
            ),
            (
                1_700_000_000,
                123_456_789,
                "2023-11-14T22:13:20.123456789+00:00",
            ),
            (1_700_000_000, 1_000, "2023-11-14T22:13:20.000001+00:00"),
            (1_700_000_000, 1, "2023-11-14T22:13:20.000000001+00:00"),
            (1_700_000_000, 999_000_000, "2023-11-14T22:13:20.999+00:00"),
            (
                1_700_000_000,
                999_999_000,
                "2023-11-14T22:13:20.999999+00:00",
            ),
            (
                1_700_000_000,
                999_999_999,
                "2023-11-14T22:13:20.999999999+00:00",
            ),
            (1_700_000_000, 1_000_000, "2023-11-14T22:13:20.001+00:00"),
            (1_700_000_000, 999_000, "2023-11-14T22:13:20.000999+00:00"),
            (86_400, 0, "1970-01-02T00:00:00+00:00"),
            (-86_400, 0, "1969-12-31T00:00:00+00:00"),
            (-62_167_219_200, 0, "0000-01-01T00:00:00+00:00"),
        ];
        for (secs, nanos, expected) in cases {
            assert_eq!(rfc3339_of(secs, nanos), expected, "{secs},{nanos}");
        }
    }

    #[test]
    fn golden_parse_matches_chrono_rfc3339() {
        // `(timestamp, subsec nanos)` from chrono 0.4.45 `parse_from_rfc3339`.
        // Leap seconds keep nanos >= 1_000_000_000.
        let ok = [
            ("2024-01-01T00:00:00Z", 1_704_067_200, 0),
            ("2024-01-01T00:00:00z", 1_704_067_200, 0),
            ("2024-01-01t00:00:00Z", 1_704_067_200, 0),
            ("2024-01-01 00:00:00Z", 1_704_067_200, 0),
            ("2024-01-01T00:00:00+00:00", 1_704_067_200, 0),
            ("2024-01-01T00:00:00-00:00", 1_704_067_200, 0),
            ("2024-01-01T00:00:00+09:00", 1_704_034_800, 0),
            ("2024-01-01T00:00:00-05:00", 1_704_085_200, 0),
            ("2024-01-01T00:00:00+05:30", 1_704_047_400, 0),
            ("2024-01-01T00:00:00.1Z", 1_704_067_200, 100_000_000),
            ("2024-01-01T00:00:00.12Z", 1_704_067_200, 120_000_000),
            ("2024-01-01T00:00:00.123Z", 1_704_067_200, 123_000_000),
            ("2024-01-01T00:00:00.1234Z", 1_704_067_200, 123_400_000),
            ("2024-01-01T00:00:00.123456Z", 1_704_067_200, 123_456_000),
            ("2024-01-01T00:00:00.123456789Z", 1_704_067_200, 123_456_789),
            (
                "2024-01-01T00:00:00.1234567891Z",
                1_704_067_200,
                123_456_789,
            ),
            (
                "2024-01-01T00:00:00.1234567899Z",
                1_704_067_200,
                123_456_789,
            ),
            ("2024-01-01T00:00:00.000Z", 1_704_067_200, 0),
            ("2016-12-31T23:59:60Z", 1_483_228_799, 1_000_000_000),
            ("2016-12-31T23:59:60.5Z", 1_483_228_799, 1_500_000_000),
            (
                "2016-12-31T23:59:60.123456789Z",
                1_483_228_799,
                1_123_456_789,
            ),
            ("2015-06-30T23:59:60Z", 1_435_708_799, 1_000_000_000),
            ("2024-01-01T00:00:60Z", 1_704_067_259, 1_000_000_000),
            ("2024-01-01T00:00:00+23:59", 1_703_980_860, 0),
            ("2024-01-01T00:00:00-23:59", 1_704_153_540, 0),
            ("0000-01-01T00:00:00Z", -62_167_219_200, 0),
            (
                "9999-12-31T23:59:59.999999999Z",
                253_402_300_799,
                999_999_999,
            ),
            ("2024-01-01T00:00:00−05:00", 1_704_085_200, 0),
            ("2024-01-01T00:00:00.0000000000Z", 1_704_067_200, 0),
            ("1970-01-01T00:00:00.000000001Z", 0, 1),
            ("1969-12-31T23:59:59.999999999Z", -1, 999_999_999),
            (
                "2024-01-01T00:00:00.123456789+09:00",
                1_704_034_800,
                123_456_789,
            ),
            ("0001-01-01T00:00:00+00:00", -62_135_596_800, 0),
        ];
        for (input, secs, nanos) in ok {
            let parsed = parse_rfc3339(input).unwrap_or_else(|| panic!("expected ok: {input}"));
            assert_eq!(parsed.secs, secs, "{input} secs");
            assert_eq!(parsed.nanos, nanos, "{input} nanos");
        }

        let err = [
            "2024-01-01T00:00:61Z",
            "2024-13-01T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "2024-00-01T00:00:00Z",
            "2024-01-00T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:60:00Z",
            "2024-01-01T00:00:00",
            "2024-01-01",
            "not-a-date",
            "",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00+99:00",
            "2024-01-01T00:00:00+0000",
            "2024-01-01T00:00:00+00",
            "2024-01-01T00:00:00+00:00:00",
            "2024-01-01T00:00:00+00:00:01",
            " 2024-01-01T00:00:00Z",
            "2024-01-01T00:00:00Z ",
            "2024-1-01T00:00:00Z",
            "999-01-01T00:00:00Z",
            "+2024-01-01T00:00:00Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00+05:30 extra",
            "2024/01/01T00:00:00Z",
            "2024-01-01T00:00:00+23:60",
            "2024-01-01T00:00:00+23:59:59",
        ];
        for input in err {
            assert!(parse_rfc3339(input).is_none(), "expected err: {input}");
        }
    }

    #[test]
    fn golden_relative_time_at_fixed_now() {
        let cases = [
            ("2024-06-15T12:00:00Z", "just now"),
            ("2024-06-15T11:59:59Z", "just now"),
            ("2024-06-15T11:59:00Z", "1m ago"),
            ("2024-06-15T11:59:00.1Z", "just now"),
            ("2024-06-15T11:00:00.999999999Z", "59m ago"),
            ("2024-06-15T11:00:01Z", "59m ago"),
            ("2024-06-15T11:00:00Z", "1h ago"),
            ("2024-06-15T12:00:00.000000001Z", "just now"),
            ("2024-06-15T12:00:01Z", "just now"),
            ("2024-06-15T13:00:00Z", "just now"),
            ("2024-06-14T12:00:00Z", "1d ago"),
            ("2024-06-14T12:00:01Z", "23h ago"),
            ("2024-06-08T12:00:00Z", "1w ago"),
            ("2024-06-08T12:00:01Z", "6d ago"),
            ("2024-06-09T12:00:00Z", "6d ago"),
            ("2024-05-11T12:00:00Z", "1mo ago"),
            ("2024-05-12T12:00:00Z", "4w ago"),
            ("2024-06-15T11:00:00.000000001+09:00", "9h ago"),
            ("2023-06-15T12:00:00Z", "1y ago"),
            ("2023-06-16T12:00:00Z", "1y ago"),
            ("2023-06-20T12:00:00Z", "0y ago"),
            ("2023-06-21T12:00:00Z", "0y ago"),
            ("2014-06-15T12:00:00Z", "10y ago"),
            ("2024-06-15T12:00:00+09:00", "9h ago"),
            ("2024-06-15T03:00:00-09:00", "just now"),
            ("not-a-date", "not-a-date"),
            ("2024-06-15T12:00:00", "2024-06-15T12:00:00"),
            ("2016-12-31T23:59:60Z", "7y ago"),
            ("1970-01-01T00:00:00Z", "54y ago"),
            ("0001-01-01T00:00:00Z", "2024y ago"),
            ("9999-12-31T23:59:59Z", "just now"),
            ("2024-06-15T11:59:00.999Z", "just now"),
            ("2024-06-15T10:59:59.000000001Z", "1h ago"),
            ("2024-06-14T12:00:00.000000001Z", "23h ago"),
            ("2024-06-08T11:59:59.000000001Z", "1w ago"),
            ("2023-06-21T11:59:59.000000001Z", "0y ago"),
            ("2024-06-15 12:00:00Z", "just now"),
            ("", ""),
            ("2024-06-15T12:00:00.9Z", "just now"),
        ];
        for (input, expected) in cases {
            assert_eq!(format_relative_time_at(input, NOW), expected, "{input}");
        }

        // `(now, then, expected)`. The first row is `2024-01-01T00:00:60Z`
        // viewed 3659s after midnight (`01:00:59Z`): chrono counts the leap
        // and reports 3600s (`1h ago`). The last row is the other direction,
        // where the viewer itself is the leap second.
        let leap_away_from_midnight = [
            ("2024-01-01T01:00:59Z", "2024-01-01T00:00:60Z", "1h ago"),
            ("2024-06-15T16:30:59Z", "2024-06-15T15:30:60Z", "1h ago"),
            ("2024-06-15T16:31:00Z", "2024-06-15T15:30:60.5Z", "1h ago"),
            ("2024-01-02T00:00:60Z", "2024-01-01T23:01:00Z", "59m ago"),
            ("2024-06-16T00:00:60Z", "2024-06-15T16:30:01Z", "7h ago"),
            ("2024-01-01T12:00:00Z", "2024-01-01T00:00:60Z", "11h ago"),
        ];
        for (now, then, expected) in leap_away_from_midnight {
            let now = parse_rfc3339(now).unwrap_or_else(|| panic!("now {now}"));
            assert_eq!(
                format_relative_time_at(then, now),
                expected,
                "{then} @ {now:?}"
            );
        }
    }

    #[test]
    fn golden_epoch_relative_time_and_chrono_range() {
        let cases = [
            (NOW.secs, "just now"),
            (NOW.secs - 1, "just now"),
            (NOW.secs - 59, "just now"),
            (NOW.secs - 60, "1m ago"),
            (NOW.secs - 3_599, "59m ago"),
            (NOW.secs - 3_600, "1h ago"),
            (NOW.secs - 86_399, "23h ago"),
            (NOW.secs - 86_400, "1d ago"),
            (NOW.secs - 6 * 86_400, "6d ago"),
            (NOW.secs - 7 * 86_400, "1w ago"),
            (NOW.secs - 34 * 86_400, "4w ago"),
            (NOW.secs - 35 * 86_400, "1mo ago"),
            (NOW.secs - 364 * 86_400, "0y ago"),
            (NOW.secs - 365 * 86_400, "1y ago"),
            (NOW.secs + 1, "just now"),
            (NOW.secs + 10, "just now"),
            (0, "54y ago"),
            (-1, "54y ago"),
            (i64::MAX, "unknown"),
            (i64::MIN, "unknown"),
            (i64::MAX - 1, "unknown"),
            (253_402_300_799, "just now"),
            (-62_167_219_200, "2025y ago"),
            (8_210_298_412_799, "unknown"),
            (-8_334_632_851_200, "unknown"),
            (MIN_CHRONO_TIMESTAMP, "264342y ago"),
            (MAX_CHRONO_TIMESTAMP, "just now"),
            (MIN_CHRONO_TIMESTAMP - 1, "unknown"),
            (MAX_CHRONO_TIMESTAMP + 1, "unknown"),
        ];
        for (epoch, expected) in cases {
            assert_eq!(format_relative_epoch_at(epoch, NOW), expected, "{epoch}");
        }
    }

    #[test]
    fn golden_check_duration_matches_chrono() {
        let cases = [
            ("2024-01-01T00:00:00Z", "2024-01-01T00:03:12Z", "3m 12s"),
            ("2024-01-01T00:00:00Z", "2024-01-01T00:00:45Z", "45s"),
            ("2024-01-01T00:00:00Z", "2024-01-01T00:00:00Z", "0s"),
            ("2024-01-01T00:00:00Z", "2024-01-01T00:00:59Z", "59s"),
            ("2024-01-01T00:00:00Z", "2024-01-01T00:01:00Z", "1m 00s"),
            ("2024-01-01T00:00:00Z", "2024-01-01T02:00:00Z", "120m 00s"),
            ("2024-01-01T00:00:00.9Z", "2024-01-01T00:00:01.1Z", "0s"),
            ("2024-01-01T00:00:00.2Z", "2024-01-01T00:00:00.1Z", "0s"),
            ("2024-01-01T00:00:01Z", "2024-01-01T00:00:00Z", "-"),
            (
                "2024-01-01T00:00:00+09:00",
                "2024-01-01T00:00:00Z",
                "540m 00s",
            ),
            ("2024-01-01T09:00:00+09:00", "2024-01-01T00:00:30Z", "30s"),
            ("2016-12-31T23:59:59Z", "2016-12-31T23:59:60Z", "1s"),
            ("2016-12-31T23:59:60Z", "2017-01-01T00:00:00Z", "0s"),
            // `:60` away from 23:59. Chrono adds a second when the later
            // time-of-day is greater, and subtracts one when the later
            // instant is the leap and its time-of-day is smaller.
            ("2024-01-01T00:00:60Z", "2024-01-01T12:00:00Z", "719m 01s"),
            ("2024-01-01T00:00:60Z", "2024-01-01T00:01:00Z", "1s"),
            ("2024-01-01T00:00:60.5Z", "2024-01-01T00:01:00.5Z", "1s"),
            ("2024-06-15T15:30:60Z", "2024-06-15T16:30:00Z", "59m 01s"),
            ("2024-01-01T23:00:00Z", "2024-01-02T00:00:60Z", "60m 59s"),
            ("2024-06-15T16:30:00Z", "2024-06-16T00:00:60Z", "450m 59s"),
            ("2024-01-01T12:00:00Z", "2024-01-01T00:00:60Z", "-"),
            ("bad", "2024-01-01T00:00:00Z", "-"),
            ("2024-01-01T00:00:00Z", "bad", "-"),
            (
                "2024-01-01T00:00:00.000000001Z",
                "2024-01-01T00:00:01.000000000Z",
                "0s",
            ),
            (
                "2024-01-01T00:00:00.000000001Z",
                "2024-01-01T00:00:00.999999999Z",
                "0s",
            ),
            ("1969-12-31T23:59:59Z", "1970-01-01T00:00:01Z", "2s"),
            ("2024-01-01T00:00:00-05:00", "2024-01-01T00:00:30Z", "-"),
            ("0001-01-01T00:00:00Z", "0001-01-01T00:00:59.9Z", "59s"),
            (
                "9999-12-31T23:00:00Z",
                "9999-12-31T23:59:59.999999999Z",
                "59m 59s",
            ),
        ];
        for (start, end, expected) in cases {
            assert_eq!(
                format_check_duration(start, end),
                expected,
                "{start} -> {end}"
            );
        }
    }

    #[test]
    fn golden_instant_ordering_matches_chrono_datetime() {
        let cases = [
            (
                "2024-01-01T00:00:00Z",
                "2024-01-01T00:00:00+00:00",
                Ordering::Equal,
            ),
            (
                "2024-01-01T00:00:00Z",
                "2024-01-01T09:00:00+09:00",
                Ordering::Equal,
            ),
            (
                "2024-01-01T00:00:00Z",
                "2024-01-01T00:00:00−05:00",
                Ordering::Less,
            ),
            (
                "2024-01-01T00:00:00Z",
                "2024-01-01T00:00:00.000000001Z",
                Ordering::Less,
            ),
            (
                "2024-01-01T00:00:00Z",
                "2023-12-31T23:59:59.999999999Z",
                Ordering::Greater,
            ),
            (
                "2016-12-31T23:59:60Z",
                "2016-12-31T23:59:59Z",
                Ordering::Greater,
            ),
            (
                "2016-12-31T23:59:60Z",
                "2017-01-01T00:00:00Z",
                Ordering::Less,
            ),
            (
                "2016-12-31T23:59:60.5Z",
                "2017-01-01T00:00:00Z",
                Ordering::Less,
            ),
            (
                "2024-06-15T12:00:00+05:30",
                "2024-06-15T06:30:00Z",
                Ordering::Equal,
            ),
            ("not-a-date", "2024-01-01T00:00:00Z", Ordering::Less),
            ("", "0001-01-01T00:00:00Z", Ordering::Less),
            ("not-a-date", "", Ordering::Equal),
            ("2024-01-01T00:00:00", "not-a-date", Ordering::Equal),
            (
                "0001-01-01T00:00:00Z",
                "9999-12-31T23:59:59Z",
                Ordering::Less,
            ),
            (
                "2024-01-01 00:00:00Z",
                "2024-01-01T00:00:00Z",
                Ordering::Equal,
            ),
        ];
        for (left, right, expected) in cases {
            assert_eq!(cmp_rfc3339(left, right), expected, "{left} ? {right}");
        }
    }

    #[test]
    fn golden_comment_sort_uses_instant_order_then_id() {
        // Newest parsed instant first. Equal instants break ties by id descending.
        // Unparseable timestamps sort last, same as `Option<DateTime>` (`None < Some`)
        // under the reversed comparison local comments use.
        let mut comments = vec![
            (1u64, "2016-12-31T23:59:59Z"),
            (2, "2016-12-31T23:59:60Z"),
            (3, "2017-01-01T00:00:00Z"),
            (4, "2024-06-15T03:00:00Z"),
            (9, "2024-06-15T12:00:00+09:00"),
            (5, "2024-06-15T12:00:00Z"),
            (6, "not-a-date"),
            (8, ""),
            (7, "0001-01-01T00:00:00Z"),
            (10, "9999-12-31T23:59:59Z"),
        ];
        comments.sort_by(|a, b| cmp_rfc3339(b.1, a.1).then_with(|| b.0.cmp(&a.0)));
        let ids: Vec<u64> = comments.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![10, 5, 9, 4, 3, 2, 1, 7, 8, 6]);
    }

    #[test]
    fn golden_cache_updated_at_comparison_is_string_equality() {
        // The reopen check does not parse. Same instant, different spelling, is a miss.
        let cases = [
            ("2024-01-01T00:00:00Z", "2024-01-01T00:00:00Z", true),
            ("2024-01-01T00:00:00Z", "2024-01-01T00:00:00+00:00", false),
            (
                "2024-01-01T00:00:00+00:00",
                "2024-01-01T00:00:00+00:00",
                true,
            ),
            ("2024-01-01T00:00:00Z", "2024-01-02T00:00:00Z", false),
            (
                "2024-01-01T00:00:00.100+00:00",
                "2024-01-01T00:00:00.100+00:00",
                true,
            ),
            (
                "2024-01-01T00:00:00.100+00:00",
                "2024-01-01T00:00:00.1+00:00",
                false,
            ),
            ("", "", true),
            ("2024-01-01T00:00:00Z", "", false),
        ];
        for (cached, fresh, equal) in cases {
            assert_eq!(
                pr_updated_at_unchanged(cached, fresh),
                equal,
                "{cached} vs {fresh}"
            );
            if equal {
                assert_eq!(cmp_rfc3339(cached, fresh), Ordering::Equal);
            }
        }
        assert_eq!(
            cmp_rfc3339("2024-01-01T00:00:00Z", "2024-01-01T00:00:00+00:00"),
            Ordering::Equal
        );
        assert!(!pr_updated_at_unchanged(
            "2024-01-01T00:00:00Z",
            "2024-01-01T00:00:00+00:00"
        ));
    }

    #[test]
    fn golden_persisted_timestamp_strings_round_trip_through_serde() {
        let started = "2024-06-01T12:34:56.789+00:00";
        let updated = "2024-06-01T12:34:56.789012+00:00";
        let session_json = format!(
            r#"{{
              "repo": "owner/repo",
              "pr_number": 42,
              "iteration": 1,
              "state": "Initializing",
              "started_at": "{started}",
              "updated_at": "{updated}"
            }}"#
        );
        let session: crate::ai::session::RallySession =
            serde_json::from_str(&session_json).expect("session file");
        assert_eq!(session.started_at, started);
        assert_eq!(session.updated_at, updated);
        let rewritten = serde_json::to_string(&session).expect("rewrite session");
        let again: crate::ai::session::RallySession =
            serde_json::from_str(&rewritten).expect("session rewrite");
        assert_eq!(again.started_at, started);
        assert_eq!(again.updated_at, updated);
        assert!(pr_updated_at_unchanged(
            &session.updated_at,
            &again.updated_at
        ));

        let created = "2024-01-01T00:00:00.100+00:00";
        let resolved = "2024-01-01T00:03:12+00:00";
        let comment_json = format!(
            r#"{{
              "id": 7,
              "path": "src/lib.rs",
              "line": 3,
              "body": "nit",
              "user": {{"login": "ada"}},
              "created_at": "{created}",
              "is_resolved": true,
              "resolved_at": "{resolved}"
            }}"#
        );
        let comment: crate::cache::LocalReviewComment =
            serde_json::from_str(&comment_json).expect("local comment file");
        assert_eq!(comment.comment.created_at, created);
        assert_eq!(comment.meta.resolved_at.as_deref(), Some(resolved));
        let comment_again: crate::cache::LocalReviewComment =
            serde_json::from_str(&serde_json::to_string(&comment).expect("rewrite comment"))
                .expect("comment rewrite");
        assert_eq!(comment_again.comment.created_at, created);
        assert_eq!(comment_again.meta.resolved_at.as_deref(), Some(resolved));

        let github = r#"{"id":1,"path":"a.rs","line":1,"body":"","user":{"login":"gh"},"created_at":"2024-01-01T00:00:00Z"}"#;
        let from_github: crate::cache::LocalReviewComment =
            serde_json::from_str(github).expect("github-shaped comment");
        assert_eq!(from_github.comment.created_at, "2024-01-01T00:00:00Z");
        assert!(from_github.meta.resolved_at.is_none());
    }

    #[test]
    fn golden_local_civil_time_matches_chrono_across_zones_and_dst() {
        // `(zone, utc instant, %Y%m%d-%H%M%S, %H:%M:%S)` from chrono `Local`
        // with `TZ` set, chrono 0.4.45. Subseconds do not appear in either format.
        let cases = [
            ("UTC", "2024-03-10T07:00:00Z", "20240310-070000", "07:00:00"),
            (
                "UTC",
                "2024-06-15T12:34:56.789Z",
                "20240615-123456",
                "12:34:56",
            ),
            ("UTC", "1969-12-31T23:59:59Z", "19691231-235959", "23:59:59"),
            ("UTC", "0001-01-01T00:00:00Z", "00010101-000000", "00:00:00"),
            (
                "America/New_York",
                "2024-03-10T06:59:59Z",
                "20240310-015959",
                "01:59:59",
            ),
            (
                "America/New_York",
                "2024-03-10T07:00:00Z",
                "20240310-030000",
                "03:00:00",
            ),
            (
                "America/New_York",
                "2024-03-10T07:00:01Z",
                "20240310-030001",
                "03:00:01",
            ),
            (
                "America/New_York",
                "2024-11-03T05:30:00Z",
                "20241103-013000",
                "01:30:00",
            ),
            (
                "America/New_York",
                "2024-11-03T06:00:00Z",
                "20241103-010000",
                "01:00:00",
            ),
            (
                "America/New_York",
                "2024-11-03T06:30:00Z",
                "20241103-013000",
                "01:30:00",
            ),
            (
                "America/New_York",
                "2024-01-01T00:00:00Z",
                "20231231-190000",
                "19:00:00",
            ),
            (
                "America/New_York",
                "2024-07-01T00:00:00Z",
                "20240630-200000",
                "20:00:00",
            ),
            (
                "America/New_York",
                "1970-01-01T00:00:00Z",
                "19691231-190000",
                "19:00:00",
            ),
            (
                "America/New_York",
                "1969-12-31T23:59:59Z",
                "19691231-185959",
                "18:59:59",
            ),
            (
                "America/New_York",
                "0001-01-01T00:00:00Z",
                "00001231-190358",
                "19:03:58",
            ),
            (
                "America/New_York",
                "1899-12-31T12:00:00Z",
                "18991231-070000",
                "07:00:00",
            ),
            (
                "America/New_York",
                "2038-01-19T03:14:07Z",
                "20380118-221407",
                "22:14:07",
            ),
            (
                "Asia/Kolkata",
                "2024-01-01T00:00:00Z",
                "20240101-053000",
                "05:30:00",
            ),
            (
                "Asia/Kolkata",
                "2024-06-15T12:34:56.789Z",
                "20240615-180456",
                "18:04:56",
            ),
            (
                "Asia/Kolkata",
                "1899-12-31T12:00:00Z",
                "18991231-172110",
                "17:21:10",
            ),
            (
                "Asia/Kolkata",
                "1969-12-31T23:59:59Z",
                "19700101-052959",
                "05:29:59",
            ),
            (
                "Australia/Lord_Howe",
                "2024-04-06T14:00:00Z",
                "20240407-010000",
                "01:00:00",
            ),
            (
                "Australia/Lord_Howe",
                "2024-04-06T15:00:00Z",
                "20240407-013000",
                "01:30:00",
            ),
            (
                "Australia/Lord_Howe",
                "2024-04-07T14:00:00Z",
                "20240408-003000",
                "00:30:00",
            ),
            (
                "Australia/Lord_Howe",
                "2024-01-01T00:00:00Z",
                "20240101-110000",
                "11:00:00",
            ),
            (
                "Australia/Lord_Howe",
                "2024-07-01T00:00:00Z",
                "20240701-103000",
                "10:30:00",
            ),
            (
                "Australia/Lord_Howe",
                "2024-09-28T15:59:59Z",
                "20240929-022959",
                "02:29:59",
            ),
            (
                "Australia/Lord_Howe",
                "2024-09-28T16:00:00Z",
                "20240929-023000",
                "02:30:00",
            ),
            (
                "Pacific/Chatham",
                "2024-01-01T00:00:00Z",
                "20240101-134500",
                "13:45:00",
            ),
            (
                "Pacific/Chatham",
                "2024-07-01T00:00:00Z",
                "20240701-124500",
                "12:45:00",
            ),
            (
                "Pacific/Chatham",
                "2024-09-28T15:59:59Z",
                "20240929-054459",
                "05:44:59",
            ),
            (
                "Pacific/Chatham",
                "2024-09-28T16:00:00Z",
                "20240929-054500",
                "05:45:00",
            ),
            (
                "Pacific/Chatham",
                "2024-04-06T14:00:00Z",
                "20240407-024500",
                "02:45:00",
            ),
            (
                "Pacific/Chatham",
                "1899-12-31T12:00:00Z",
                "19000101-001500",
                "00:15:00",
            ),
            (
                "Europe/London",
                "2024-01-01T00:00:00Z",
                "20240101-000000",
                "00:00:00",
            ),
            (
                "Europe/London",
                "2024-07-01T00:00:00Z",
                "20240701-010000",
                "01:00:00",
            ),
            (
                "Europe/London",
                "0001-01-01T00:00:00Z",
                "00001231-235845",
                "23:58:45",
            ),
            (
                "Europe/London",
                "1970-01-01T00:00:00Z",
                "19700101-010000",
                "01:00:00",
            ),
            (
                "America/St_Johns",
                "2024-11-03T04:59:59Z",
                "20241103-012959",
                "01:29:59",
            ),
            (
                "America/St_Johns",
                "2024-07-01T00:00:00Z",
                "20240630-213000",
                "21:30:00",
            ),
            (
                "America/St_Johns",
                "2024-01-01T00:00:00Z",
                "20231231-203000",
                "20:30:00",
            ),
            (
                "America/St_Johns",
                "1899-12-31T12:00:00Z",
                "18991231-082908",
                "08:29:08",
            ),
            (
                "Pacific/Kiritimati",
                "1970-01-01T00:00:00Z",
                "19691231-132000",
                "13:20:00",
            ),
            (
                "Pacific/Kiritimati",
                "2024-01-01T00:00:00Z",
                "20240101-140000",
                "14:00:00",
            ),
            (
                "Pacific/Kiritimati",
                "2024-06-15T12:34:56.789Z",
                "20240616-023456",
                "02:34:56",
            ),
            (
                "Australia/Adelaide",
                "2024-01-01T00:00:00Z",
                "20240101-103000",
                "10:30:00",
            ),
            (
                "Australia/Adelaide",
                "2024-07-01T00:00:00Z",
                "20240701-093000",
                "09:30:00",
            ),
            (
                "Australia/Adelaide",
                "2024-04-06T14:00:00Z",
                "20240407-003000",
                "00:30:00",
            ),
            (
                "Australia/Adelaide",
                "2024-04-07T14:00:00Z",
                "20240407-233000",
                "23:30:00",
            ),
            (
                "Etc/GMT+12",
                "2024-06-15T12:00:00Z",
                "20240615-000000",
                "00:00:00",
            ),
        ];
        for (zone, instant, backup, log) in cases {
            let parsed = parse_rfc3339(instant).unwrap_or_else(|| panic!("parse {instant}"));
            let nanos = i32::try_from(parsed.nanos).expect("non-leap instant");
            let ts = Timestamp::new(parsed.secs, nanos)
                .unwrap_or_else(|error| panic!("jiff timestamp for {instant}: {error}"));
            let zoned = ts
                .in_tz(zone)
                .unwrap_or_else(|error| panic!("{zone} {instant}: {error}"));
            assert_eq!(
                zoned.strftime("%Y%m%d-%H%M%S").to_string(),
                backup,
                "{zone} {instant} backup"
            );
            assert_eq!(
                zoned.strftime("%H:%M:%S").to_string(),
                log,
                "{zone} {instant} log"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_tz_falls_back_to_system_zone_not_utc() {
        if let Ok(path) = std::env::var("OCTORUS_TIMEFMT_TZ_PROBE") {
            let zone = local_time_zone();
            let name = zone.iana_name().unwrap_or("unknown").to_string();
            let instant = Timestamp::new(1_718_452_800, 0).expect("fixed probe instant");
            let civil = instant
                .to_zoned_datetime(zone)
                .strftime("%Y%m%d-%H%M%S")
                .to_string();
            let jiff_fallback = jiff::tz::TimeZone::system();
            let jiff_name = jiff_fallback.iana_name().unwrap_or("unknown");
            std::fs::write(&path, format!("ours={name} civil={civil} jiff={jiff_name}"))
                .unwrap_or_else(|error| panic!("write {path}: {error}"));
            return;
        }

        let system = tz_probe("unset", None);
        let invalid = tz_probe("invalid", Some("Invalid/Zone"));
        let garbage = tz_probe("garbage", Some("garbage"));
        assert_eq!(
            (&invalid.ours, &invalid.civil),
            (&system.ours, &system.civil),
            "TZ=Invalid/Zone must follow /etc/localtime, not UTC"
        );
        assert_eq!(
            (&garbage.ours, &garbage.civil),
            (&system.ours, &system.civil),
            "TZ=garbage must follow /etc/localtime"
        );
        assert_ne!(
            invalid.ours, invalid.jiff,
            "jiff's invalid-TZ fallback is Etc/Unknown, which formats as UTC"
        );
        assert_ne!(garbage.ours, garbage.jiff);
        assert_ne!(invalid.ours, "unknown");

        let link = std::fs::read_link("/etc/localtime").expect("/etc/localtime");
        let link = link.to_str().expect("localtime symlink is utf-8");
        let suffix = link
            .rsplit_once("zoneinfo/")
            .map(|(_, name)| name)
            .expect("localtime points into zoneinfo");
        assert_eq!(system.ours, suffix);

        let utc_civil = Timestamp::new(1_718_452_800, 0)
            .expect("fixed probe instant")
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .strftime("%Y%m%d-%H%M%S")
            .to_string();
        if system.civil != utc_civil {
            assert_ne!(invalid.civil, utc_civil);
        }
    }

    #[cfg(unix)]
    #[derive(Debug)]
    struct TzProbe {
        ours: String,
        civil: String,
        jiff: String,
    }

    #[cfg(unix)]
    fn tz_probe(label: &str, tz: Option<&str>) -> TzProbe {
        let path =
            std::env::temp_dir().join(format!("octorus-tz-probe-{}-{label}", std::process::id()));
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .arg("timefmt::tests::invalid_tz_falls_back_to_system_zone_not_utc")
            .arg("--exact")
            .env("OCTORUS_TIMEFMT_TZ_PROBE", &path);
        match tz {
            Some(value) => {
                command.env("TZ", value);
            }
            None => {
                command.env_remove("TZ");
            }
        }
        let output = command.output().expect("spawn tz probe");
        let report = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(
            output.status.success(),
            "tz probe failed\nreport:\n{report}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let line = report.lines().next().unwrap_or_else(|| {
            panic!(
                "missing probe report\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        let mut ours = None;
        let mut civil = None;
        let mut jiff_name = None;
        for field in line.split_whitespace() {
            if let Some(value) = field.strip_prefix("ours=") {
                ours = Some(value.to_string());
            } else if let Some(value) = field.strip_prefix("civil=") {
                civil = Some(value.to_string());
            } else if let Some(value) = field.strip_prefix("jiff=") {
                jiff_name = Some(value.to_string());
            }
        }
        TzProbe {
            ours: ours.expect("ours"),
            civil: civil.expect("civil"),
            jiff: jiff_name.expect("jiff"),
        }
    }

    #[test]
    fn written_utc_timestamp_parses_back_to_the_same_instant() {
        let samples = [
            (1_700_000_000, 0),
            (1_700_000_000, 100_000_000),
            (1_700_000_000, 123_456_000),
            (1_700_000_000, 123_456_789),
            (0, 1),
            (-1, 999_999_999),
        ];
        for (secs, nanos) in samples {
            let written = rfc3339_of(secs, nanos);
            assert!(written.ends_with("+00:00"), "{written}");
            let parsed = parse_rfc3339(&written).expect(&written);
            assert_eq!(
                (parsed.secs, parsed.nanos),
                (secs, nanos as u32),
                "{written}"
            );
        }
    }
}
