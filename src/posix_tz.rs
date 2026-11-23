//! POSIX `TZ` strings the way chrono 0.4.45 parses them.
//!
//! Derived from chrono 0.4.45 `src/offset/local/tz_info/rule.rs` (`TransitionRule::from_tz_string`
//! with `use_string_extensions = false`, `RuleDay`, `AlternateTime`) and the `Cursor` helpers that
//! parser uses. Chrono is dual-licensed MIT OR Apache-2.0. octorus is MIT, so this port is taken
//! under the MIT grant. The copyright notice and permission notice below are part of that grant.
//!
//! Copyright (c) 2014--2026, Kang Seonghoon and contributors.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in
//! all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
//! THE SOFTWARE.

use std::num::ParseIntError;
use std::str::{FromStr, Utf8Error};

/// A parsed POSIX `TZ` rule. The offset is seconds east of UTC, matching `LocalTimeType::ut_offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PosixZone {
    Fixed(i32),
    Alternate(AlternateTime),
}

impl PosixZone {
    /// Offset at `unix_time`, or `None` when the instant is outside chrono's rule range.
    pub(crate) fn offset_at(self, unix_time: i64) -> Option<i32> {
        match self {
            PosixZone::Fixed(offset) => Some(offset),
            PosixZone::Alternate(rule) => rule.find_offset(unix_time),
        }
    }
}

/// Parse `tz` with chrono's `from_tz_string(..., false)`. Extensions (`/-1`, `/25`, hour 24) are rejected.
pub(crate) fn parse(tz: &str) -> Option<PosixZone> {
    parse_tz(tz.as_bytes()).ok()
}

struct TzError;

impl From<std::io::Error> for TzError {
    fn from(_: std::io::Error) -> Self {
        TzError
    }
}

impl From<ParseIntError> for TzError {
    fn from(_: ParseIntError) -> Self {
        TzError
    }
}

impl From<Utf8Error> for TzError {
    fn from(_: Utf8Error) -> Self {
        TzError
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AlternateTime {
    std_offset: i32,
    dst_offset: i32,
    dst_start: RuleDay,
    dst_start_time: i32,
    dst_end: RuleDay,
    dst_end_time: i32,
}

impl AlternateTime {
    const fn new(
        std_offset: i32,
        dst_offset: i32,
        dst_start: RuleDay,
        dst_start_time: i32,
        dst_end: RuleDay,
        dst_end_time: i32,
    ) -> Result<Self, TzError> {
        if !((dst_start_time as i64).abs() < SECONDS_PER_WEEK
            && (dst_end_time as i64).abs() < SECONDS_PER_WEEK)
        {
            return Err(TzError);
        }
        Ok(Self {
            std_offset,
            dst_offset,
            dst_start,
            dst_start_time,
            dst_end,
            dst_end_time,
        })
    }

    fn find_offset(self, unix_time: i64) -> Option<i32> {
        let dst_start_time_in_utc = i64::from(self.dst_start_time) - i64::from(self.std_offset);
        let dst_end_time_in_utc = i64::from(self.dst_end_time) - i64::from(self.dst_offset);
        let current_year = UtcDateTime::from_timespec(unix_time).ok()?.year;
        if !(i32::MIN + 2..=i32::MAX - 2).contains(&current_year) {
            return None;
        }

        let current_year_dst_start_unix_time = self
            .dst_start
            .unix_time(current_year, dst_start_time_in_utc);
        let current_year_dst_end_unix_time =
            self.dst_end.unix_time(current_year, dst_end_time_in_utc);

        let is_dst = match Ord::cmp(
            &current_year_dst_start_unix_time,
            &current_year_dst_end_unix_time,
        ) {
            std::cmp::Ordering::Less | std::cmp::Ordering::Equal => {
                if unix_time < current_year_dst_start_unix_time {
                    let previous_year_dst_end_unix_time = self
                        .dst_end
                        .unix_time(current_year - 1, dst_end_time_in_utc);
                    if unix_time < previous_year_dst_end_unix_time {
                        let previous_year_dst_start_unix_time = self
                            .dst_start
                            .unix_time(current_year - 1, dst_start_time_in_utc);
                        previous_year_dst_start_unix_time <= unix_time
                    } else {
                        false
                    }
                } else if unix_time < current_year_dst_end_unix_time {
                    true
                } else {
                    let next_year_dst_start_unix_time = self
                        .dst_start
                        .unix_time(current_year + 1, dst_start_time_in_utc);
                    if next_year_dst_start_unix_time <= unix_time {
                        let next_year_dst_end_unix_time = self
                            .dst_end
                            .unix_time(current_year + 1, dst_end_time_in_utc);
                        unix_time < next_year_dst_end_unix_time
                    } else {
                        false
                    }
                }
            }
            std::cmp::Ordering::Greater => {
                if unix_time < current_year_dst_end_unix_time {
                    let previous_year_dst_start_unix_time = self
                        .dst_start
                        .unix_time(current_year - 1, dst_start_time_in_utc);
                    if unix_time < previous_year_dst_start_unix_time {
                        let previous_year_dst_end_unix_time = self
                            .dst_end
                            .unix_time(current_year - 1, dst_end_time_in_utc);
                        unix_time < previous_year_dst_end_unix_time
                    } else {
                        true
                    }
                } else if unix_time < current_year_dst_start_unix_time {
                    false
                } else {
                    let next_year_dst_end_unix_time = self
                        .dst_end
                        .unix_time(current_year + 1, dst_end_time_in_utc);
                    if next_year_dst_end_unix_time <= unix_time {
                        let next_year_dst_start_unix_time = self
                            .dst_start
                            .unix_time(current_year + 1, dst_start_time_in_utc);
                        next_year_dst_start_unix_time <= unix_time
                    } else {
                        true
                    }
                }
            }
        };
        Some(if is_dst {
            self.dst_offset
        } else {
            self.std_offset
        })
    }
}

fn parse_tz(bytes: &[u8]) -> Result<PosixZone, TzError> {
    let mut cursor = Cursor::new(bytes);
    let std_name = parse_name(&mut cursor)?;
    validate_name(std_name)?;
    let std_offset = parse_offset(&mut cursor)?;
    if cursor.is_empty() {
        return Ok(PosixZone::Fixed(-std_offset));
    }
    let dst_name = parse_name(&mut cursor)?;
    validate_name(dst_name)?;
    let dst_offset = match cursor.peek() {
        Some(&b',') => std_offset - 3600,
        Some(_) => parse_offset(&mut cursor)?,
        None => return Err(TzError),
    };
    if cursor.is_empty() {
        return Err(TzError);
    }
    cursor.read_tag(b",")?;
    let (dst_start, dst_start_time) = RuleDay::parse(&mut cursor)?;
    cursor.read_tag(b",")?;
    let (dst_end, dst_end_time) = RuleDay::parse(&mut cursor)?;
    if !cursor.is_empty() {
        return Err(TzError);
    }
    Ok(PosixZone::Alternate(AlternateTime::new(
        -std_offset,
        -dst_offset,
        dst_start,
        dst_start_time,
        dst_end,
        dst_end_time,
    )?))
}

fn validate_name(name: &[u8]) -> Result<(), TzError> {
    if !(3..=7).contains(&name.len()) {
        return Err(TzError);
    }
    if name
        .iter()
        .any(|byte| !matches!(byte, b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'+' | b'-'))
    {
        return Err(TzError);
    }
    Ok(())
}

fn parse_name<'a>(cursor: &mut Cursor<'a>) -> Result<&'a [u8], TzError> {
    if !matches!(cursor.peek(), Some(b'<')) {
        return Ok(cursor.read_while(u8::is_ascii_alphabetic)?);
    }
    cursor.read_exact(1)?;
    let unquoted = cursor.read_until(|&byte| byte == b'>')?;
    cursor.read_exact(1)?;
    Ok(unquoted)
}

fn parse_offset(cursor: &mut Cursor<'_>) -> Result<i32, TzError> {
    let (sign, hour, minute, second) = parse_signed_hhmmss(cursor)?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return Err(TzError);
    }
    Ok(sign * (hour * 3600 + minute * 60 + second))
}

fn parse_rule_time(cursor: &mut Cursor<'_>) -> Result<i32, TzError> {
    let (hour, minute, second) = parse_hhmmss(cursor)?;
    if !(0..=24).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return Err(TzError);
    }
    Ok(hour * 3600 + minute * 60 + second)
}

fn parse_hhmmss(cursor: &mut Cursor<'_>) -> Result<(i32, i32, i32), TzError> {
    let hour = cursor.read_int()?;
    let mut minute = 0;
    let mut second = 0;
    if cursor.read_optional_tag(b":")? {
        minute = cursor.read_int()?;
        if cursor.read_optional_tag(b":")? {
            second = cursor.read_int()?;
        }
    }
    Ok((hour, minute, second))
}

fn parse_signed_hhmmss(cursor: &mut Cursor<'_>) -> Result<(i32, i32, i32, i32), TzError> {
    let mut sign = 1;
    if let Some(&byte) = cursor.peek() {
        if byte == b'+' || byte == b'-' {
            cursor.read_exact(1)?;
            if byte == b'-' {
                sign = -1;
            }
        }
    }
    let (hour, minute, second) = parse_hhmmss(cursor)?;
    Ok((sign, hour, minute, second))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleDay {
    Julian1WithoutLeap(u16),
    Julian0WithLeap(u16),
    MonthWeekday { month: u8, week: u8, week_day: u8 },
}

impl RuleDay {
    fn parse(cursor: &mut Cursor<'_>) -> Result<(Self, i32), TzError> {
        let date = match cursor.peek() {
            Some(b'M') => {
                cursor.read_exact(1)?;
                let month = cursor.read_int()?;
                cursor.read_tag(b".")?;
                let week = cursor.read_int()?;
                cursor.read_tag(b".")?;
                let week_day = cursor.read_int()?;
                RuleDay::month_weekday(month, week, week_day)?
            }
            Some(b'J') => {
                cursor.read_exact(1)?;
                RuleDay::julian_1(cursor.read_int()?)?
            }
            _ => RuleDay::julian_0(cursor.read_int()?)?,
        };
        let time = if cursor.read_optional_tag(b"/")? {
            parse_rule_time(cursor)?
        } else {
            2 * 3600
        };
        Ok((date, time))
    }

    fn julian_1(julian_day_1: u16) -> Result<Self, TzError> {
        if !(1..=365).contains(&julian_day_1) {
            return Err(TzError);
        }
        Ok(RuleDay::Julian1WithoutLeap(julian_day_1))
    }

    const fn julian_0(julian_day_0: u16) -> Result<Self, TzError> {
        if julian_day_0 > 365 {
            return Err(TzError);
        }
        Ok(RuleDay::Julian0WithLeap(julian_day_0))
    }

    fn month_weekday(month: u8, week: u8, week_day: u8) -> Result<Self, TzError> {
        if !(1..=12).contains(&month) || !(1..=5).contains(&week) || week_day > 6 {
            return Err(TzError);
        }
        Ok(RuleDay::MonthWeekday {
            month,
            week,
            week_day,
        })
    }

    fn transition_date(self, year: i32) -> (usize, i64) {
        match self {
            RuleDay::Julian1WithoutLeap(year_day) => {
                let year_day = i64::from(year_day);
                let month = match CUMUL_DAY_IN_MONTHS_NORMAL_YEAR.binary_search(&(year_day - 1)) {
                    Ok(index) => index + 1,
                    Err(index) => index,
                };
                let month_day = year_day - CUMUL_DAY_IN_MONTHS_NORMAL_YEAR[month - 1];
                (month, month_day)
            }
            RuleDay::Julian0WithLeap(year_day) => {
                let leap = i64::from(is_leap_year(year));
                let cumul_day_in_months = [
                    0,
                    31,
                    59 + leap,
                    90 + leap,
                    120 + leap,
                    151 + leap,
                    181 + leap,
                    212 + leap,
                    243 + leap,
                    273 + leap,
                    304 + leap,
                    334 + leap,
                ];
                let year_day = i64::from(year_day);
                let month = match cumul_day_in_months.binary_search(&year_day) {
                    Ok(index) => index + 1,
                    Err(index) => index,
                };
                let month_day = 1 + year_day - cumul_day_in_months[month - 1];
                (month, month_day)
            }
            RuleDay::MonthWeekday {
                month: rule_month,
                week,
                week_day,
            } => {
                let leap = i64::from(is_leap_year(year));
                let month = usize::from(rule_month);
                let mut day_in_month = DAY_IN_MONTHS_NORMAL_YEAR[month - 1];
                if month == 2 {
                    day_in_month += leap;
                }
                let week_day_of_first_month_day =
                    (4 + days_since_unix_epoch(year, month, 1)).rem_euclid(DAYS_PER_WEEK);
                let first_week_day_occurrence_in_month = 1
                    + (i64::from(week_day) - week_day_of_first_month_day).rem_euclid(DAYS_PER_WEEK);
                let mut month_day =
                    first_week_day_occurrence_in_month + (i64::from(week) - 1) * DAYS_PER_WEEK;
                if month_day > day_in_month {
                    month_day -= DAYS_PER_WEEK;
                }
                (month, month_day)
            }
        }
    }

    fn unix_time(self, year: i32, day_time_in_utc: i64) -> i64 {
        let (month, month_day) = self.transition_date(year);
        days_since_unix_epoch(year, month, month_day) * SECONDS_PER_DAY + day_time_in_utc
    }
}

struct UtcDateTime {
    year: i32,
}

impl UtcDateTime {
    fn from_timespec(unix_time: i64) -> Result<Self, TzError> {
        let seconds = unix_time.checked_sub(UNIX_OFFSET_SECS).ok_or(TzError)?;
        let mut remaining_days = seconds / SECONDS_PER_DAY;
        let remaining_seconds = seconds % SECONDS_PER_DAY;
        if remaining_seconds < 0 {
            remaining_days -= 1;
        }
        let mut cycles_400_years = remaining_days / DAYS_PER_400_YEARS;
        remaining_days %= DAYS_PER_400_YEARS;
        if remaining_days < 0 {
            remaining_days += DAYS_PER_400_YEARS;
            cycles_400_years -= 1;
        }
        let cycles_100_years = Ord::min(remaining_days / DAYS_PER_100_YEARS, 3);
        remaining_days -= cycles_100_years * DAYS_PER_100_YEARS;
        let cycles_4_years = Ord::min(remaining_days / DAYS_PER_4_YEARS, 24);
        remaining_days -= cycles_4_years * DAYS_PER_4_YEARS;
        let remaining_years = Ord::min(remaining_days / DAYS_PER_NORMAL_YEAR, 3);
        remaining_days -= remaining_years * DAYS_PER_NORMAL_YEAR;
        let mut year = OFFSET_YEAR
            + remaining_years
            + cycles_4_years * 4
            + cycles_100_years * 100
            + cycles_400_years * 400;
        let mut month = 0;
        while month < DAY_IN_MONTHS_LEAP_YEAR_FROM_MARCH.len() {
            let days = DAY_IN_MONTHS_LEAP_YEAR_FROM_MARCH[month];
            if remaining_days < days {
                break;
            }
            remaining_days -= days;
            month += 1;
        }
        month += 2;
        if month >= MONTHS_PER_YEAR as usize {
            year += 1;
        }
        let year = i32::try_from(year).map_err(|_| TzError)?;
        Ok(Self { year })
    }
}

struct Cursor<'a> {
    remaining: &'a [u8],
}

impl<'a> Cursor<'a> {
    const fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    const fn peek(&self) -> Option<&u8> {
        self.remaining.first()
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    fn read_exact(&mut self, count: usize) -> Result<&'a [u8], std::io::Error> {
        match (self.remaining.get(..count), self.remaining.get(count..)) {
            (Some(result), Some(remaining)) => {
                self.remaining = remaining;
                Ok(result)
            }
            _ => Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
        }
    }

    fn read_tag(&mut self, tag: &[u8]) -> Result<(), std::io::Error> {
        if self.read_exact(tag.len())? == tag {
            Ok(())
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::InvalidData))
        }
    }

    fn read_optional_tag(&mut self, tag: &[u8]) -> Result<bool, std::io::Error> {
        if self.remaining.starts_with(tag) {
            self.read_exact(tag.len())?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn read_while(&mut self, predicate: impl Fn(&u8) -> bool) -> Result<&'a [u8], std::io::Error> {
        match self.remaining.iter().position(|byte| !predicate(byte)) {
            None => self.read_exact(self.remaining.len()),
            Some(position) => self.read_exact(position),
        }
    }

    fn read_until(&mut self, predicate: impl Fn(&u8) -> bool) -> Result<&'a [u8], std::io::Error> {
        match self.remaining.iter().position(predicate) {
            None => self.read_exact(self.remaining.len()),
            Some(position) => self.read_exact(position),
        }
    }

    fn read_int<T: FromStr<Err = ParseIntError>>(&mut self) -> Result<T, TzError> {
        let bytes = self.read_while(u8::is_ascii_digit)?;
        Ok(std::str::from_utf8(bytes)?.parse()?)
    }
}

const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_DAY: i64 = SECONDS_PER_HOUR * 24;
const DAYS_PER_WEEK: i64 = 7;
const SECONDS_PER_WEEK: i64 = SECONDS_PER_DAY * DAYS_PER_WEEK;
const DAY_IN_MONTHS_NORMAL_YEAR: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const CUMUL_DAY_IN_MONTHS_NORMAL_YEAR: [i64; 12] =
    [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
const MONTHS_PER_YEAR: i64 = 12;
const DAYS_PER_NORMAL_YEAR: i64 = 365;
const DAYS_PER_4_YEARS: i64 = DAYS_PER_NORMAL_YEAR * 4 + 1;
const DAYS_PER_100_YEARS: i64 = DAYS_PER_NORMAL_YEAR * 100 + 24;
const DAYS_PER_400_YEARS: i64 = DAYS_PER_NORMAL_YEAR * 400 + 97;
const UNIX_OFFSET_SECS: i64 = 951_868_800;
const OFFSET_YEAR: i64 = 2000;
const DAY_IN_MONTHS_LEAP_YEAR_FROM_MARCH: [i64; 12] =
    [31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 31, 29];

const fn is_leap_year(year: i32) -> bool {
    year % 400 == 0 || (year % 4 == 0 && year % 100 != 0)
}

const fn days_since_unix_epoch(year: i32, month: usize, month_day: i64) -> i64 {
    let leap = is_leap_year(year);
    let year = year as i64;
    let mut result = (year - 1970) * 365;
    if year >= 1970 {
        result += (year - 1968) / 4;
        result -= (year - 1900) / 100;
        result += (year - 1600) / 400;
        if leap && month < 3 {
            result -= 1;
        }
    } else {
        result += (year - 1972) / 4;
        result -= (year - 2000) / 100;
        result += (year - 2000) / 400;
        if leap && month >= 3 {
            result += 1;
        }
    }
    result += CUMUL_DAY_IN_MONTHS_NORMAL_YEAR[month - 1] + month_day - 1;
    result
}

#[cfg(test)]
mod tests {
    use super::parse;

    /// Instants used when the offsets below were captured from chrono 0.4.45.
    const INSTANTS: [i64; 6] = [
        0,             // 1970-01-01T00:00:00Z
        951_825_600,   // 2000-02-29T12:00:00Z
        1_705_312_800, // 2024-01-15T10:00:00Z
        1_718_452_800, // 2024-06-15T12:00:00Z
        1_704_067_199, // 2023-12-31T23:59:59Z
        1_735_689_599, // 2024-12-31T23:59:59Z
    ];

    #[test]
    fn golden_named_posix_rules_match_chrono() {
        // Captured from chrono 0.4.45 `Local` with the rule in `TZ`.
        // `None` is a rule chrono rejects (hour 24, or a negative or >24 rule time).
        check(&[
            (
                "XYZ9:0",
                Some([-32400, -32400, -32400, -32400, -32400, -32400]),
            ),
            (
                "<+0530>+12:0:0",
                Some([-43200, -43200, -43200, -43200, -43200, -43200]),
            ),
            ("<+0530>24", None),
            ("EST5EDT,M3.2.0/-1,M11.1.0", None),
            ("EST5EDT,M3.2.0/25,M11.1.0", None),
            ("EST5EDT,M3.2.0/167,M11.1.0", None),
            ("<-02>2<-01>,M3.5.0/-1,M10.5.0/0", None),
            ("EET-2EEST,M3.4.4/50,M10.4.4/50", None),
            ("IST-2IDT,M3.4.4/26,M10.5.0", None),
            (
                "ABC5DEF,365,M11.1.0",
                Some([-18000, -14400, -14400, -14400, -18000, -14400]),
            ),
            (
                "ABC5DEF,J365,M11.1.0",
                Some([-14400, -14400, -14400, -14400, -14400, -14400]),
            ),
            ("JST-9", Some([32400, 32400, 32400, 32400, 32400, 32400])),
            (
                "EST5EDT,M3.2.0,M11.1.0",
                Some([-18000, -18000, -18000, -14400, -18000, -18000]),
            ),
            (
                "<+0530>-5:30",
                Some([19800, 19800, 19800, 19800, 19800, 19800]),
            ),
        ]);
        let julian = parse("ABC5DEF,365,M11.1.0").expect("365");
        let explicit = parse("ABC5DEF,J365,M11.1.0").expect("J365");
        assert_ne!(
            julian.offset_at(1_704_067_199),
            explicit.offset_at(1_704_067_199),
            "zero-based Julian day 365 and J365 differ at the end of 2023"
        );
    }

    #[test]
    fn golden_random_posix_rules_match_chrono() {
        // 200 rules drawn with LCG seed `0x00C0_4E70_052A_D001`. Accept/reject and
        // the offsets were captured from chrono 0.4.45.
        check(&[
            ("<2789+->11:4:1FGZIP16,303/24,J210/61", None),
            ("DQPEXK14OTEZ,M12.0.3,J365/167", None),
            ("PQXU-22CBOFS5,M9.3.1/14,J138/173", None),
            (
                "BSBSDGP-7",
                Some([25200, 25200, 25200, 25200, 25200, 25200]),
            ),
            ("JSDEPKP11:2:1VYH,M10.6.5/-2,M2.4.7/24", None),
            ("ZKBW24", None),
            ("DQTMRA2:9:2<+3856->4:9:0,25/167,J365/24", None),
            ("DKZ7:4<6-05+->,81/167,J12/22:1", None),
            ("CLYP1", Some([-3600, -3600, -3600, -3600, -3600, -3600])),
            ("NOXGNM10:3:8STMJG+1,M3.1.3/0:5,M12.6.7/24", None),
            (
                "TAXUX+17RKNOBUR6:1:6,363,M3.5.6/16:1",
                Some([-21666, -21666, -21666, -61200, -21666, -21666]),
            ),
            (
                "GLIJ+21",
                Some([-75600, -75600, -75600, -75600, -75600, -75600]),
            ),
            ("VCHMJ9:0YNGD-12,365/31,J86/23", None),
            ("JYZMTYP-3:2<+38567>4:5", None),
            ("HMTCHC2WFSP6:7:0,11,J382/21", None),
            ("TKTOZ+25ZQXIH4,143/24,J365/-3", None),
            ("<056->24GZM24,M2.5.7/24,J10/127", None),
            ("XCVUR+25DGBCDW,J258/133,M7.5.5/8:5", None),
            ("<4523>24MFM7:0:5", None),
            ("XSHSXE+28QZYPGJY+5,J365/24,M7.3.2/6", None),
            ("NGHQPQ-0:5RMFGT3:5,J365/24,J365/-1", None),
            (
                "SDQRQPI8:1",
                Some([-28860, -28860, -28860, -28860, -28860, -28860]),
            ),
            (
                "NEZANGH+17",
                Some([-61200, -61200, -61200, -61200, -61200, -61200]),
            ),
            ("DODWLO6:9:8ANW,J218/165,J166/17", None),
            (
                "RSJSNQB3",
                Some([-10800, -10800, -10800, -10800, -10800, -10800]),
            ),
            ("DELOFCN24SBARKLY-7:6,365/9,J365/-3", None),
            ("WDEV24AFG44,M7.5.3/16:7,M6.2.7/24", None),
            ("JMFG4:3:0<096->8:1,M9.2.2/51,J365/167", None),
            ("NKPSXEH-9:6<012->2", None),
            (
                "<6-4563>-15",
                Some([54000, 54000, 54000, 54000, 54000, 54000]),
            ),
            (
                "XIXOHU+2UPC5,M11.5.5/10:7,365/0",
                Some([-18000, -7200, -7200, -7200, -18000, -7200]),
            ),
            ("HKHONKV+29HKL-2:9,J365/167,J52/8:7", None),
            ("QHMBCH+13XYT", None),
            (
                "<+301+3>-15",
                Some([54000, 54000, 54000, 54000, 54000, 54000]),
            ),
            ("PIF3:4:7TYL,M10.0.1/167,J284/16:5", None),
            ("<0167>36", None),
            ("HMDEXKZ24ULGDIT,365/69,M11.2.4/26:1", None),
            ("ILCDG0:1:6<0963>20,365/91,M3.1.0/18:5", None),
            ("MPITQJ24IXOFQF+4,365/91,J342/12", None),
            ("VEFMXM24JED,M8.1.1/167,J124/10:5", None),
            ("QDKNQXU14ATWB", None),
            ("WHWLOX-13FQFMJE+15,M5.2.4/27,J365/167", None),
            ("JSDU-6:3", Some([21780, 21780, 21780, 21780, 21780, 21780])),
            ("DMH-7:4ADKXKTK24,365/23,J365/-3", None),
            (
                "SRMBOJS7:9",
                Some([-25740, -25740, -25740, -25740, -25740, -25740]),
            ),
            ("XYTAB-9:8QTKD41,M8.6.5/-2,365/8", None),
            ("ZIP+9BSFOZC", None),
            ("TGBO8:5:4YTORC-9:8,301/-2,329/167", None),
            ("BSB+1VEJ+2,357/-3,365/0", None),
            (
                "TUJIZA-0:5DGZWPOJ,175/24,M7.2.1/0",
                Some([300, 300, 300, 300, 300, 300]),
            ),
            (
                "XKXW5:5",
                Some([-18300, -18300, -18300, -18300, -18300, -18300]),
            ),
            ("VEPM6WLG,J365/-3,365/4", None),
            ("BKLM14CFCRK0:4,233/167,J365/24", None),
            ("DWVO6YNIXQ-11,J365/167,J365/24", None),
            ("ZYT-15", Some([54000, 54000, 54000, 54000, 54000, 54000])),
            ("LMDYD-21JIBKDS9:6:1,J106/107,J365/167", None),
            ("LIRCDMT+13HQPWH8,365/20,J314/161", None),
            (
                "RWB3",
                Some([-10800, -10800, -10800, -10800, -10800, -10800]),
            ),
            ("DSTURS-6GXW-23,J365/-3,365/2", None),
            ("VYTULQ+12AHGTGVU-1:0", None),
            ("<0523>7:2:3FCFQL0:3:2", None),
            ("CTYJYV38BODMB-0,39/24,J365/-1", None),
            ("WXA27<416->-8,M6.5.5/-2,365/0", None),
            ("JEXSLON-21LCDYBMX10:1:4,M12.1.1/167,M9.4.2/57", None),
            (
                "TAVADE+10SNI7:0:1,M1.1.5/3,M11.4.6/2",
                Some([-36000, -25201, -25201, -25201, -36000, -36000]),
            ),
            (
                "XOZSL+5",
                Some([-18000, -18000, -18000, -18000, -18000, -18000]),
            ),
            (
                "HOFWNU1:9",
                Some([-4140, -4140, -4140, -4140, -4140, -4140]),
            ),
            ("NWXU-8GVK34,J108/10:3,365/4", None),
            ("XMZKDE6<2341+->-4:3,J60/10:9,M2.3.7/24", None),
            ("HUHMJ1FYBA,J160/4,J60/28:9", None),
            (
                "ENOD7",
                Some([-25200, -25200, -25200, -25200, -25200, -25200]),
            ),
            ("XGJMHQ2:5:8WXWJ,365/67,J214/10", None),
            ("HGFM20<+7816->", None),
            ("<0127>9:8QBOPOVY30,M5.2.4/20,J365/-2", None),
            (
                "<81+->19",
                Some([-68400, -68400, -68400, -68400, -68400, -68400]),
            ),
            ("XIJQ24", None),
            ("PAFMNMJ24<01+3>3:7,M5.1.3/4,J226/35", None),
            ("LMNU24", None),
            ("PWBCZE8UJYX,357/-3,365/4", None),
            ("NKR24<234967>24,J365/-2,M10.0.7/24", None),
            ("HWLOL+27PIFKDU5:6:1", None),
            ("TUFI37KZI,J154/111,J365/167", None),
            ("LEZIZGP30DYBGJ6", None),
            (
                "SXUVEP+23",
                Some([-82800, -82800, -82800, -82800, -82800, -82800]),
            ),
            (
                "DOVQRUZ7",
                Some([-25200, -25200, -25200, -25200, -25200, -25200]),
            ),
            ("XYH7:0:7TMF-22", None),
            ("MJIHCDK24TETADQJ-6:5", None),
            ("XUNI24", None),
            (
                "XOFULIR-3:8MZUDYD+22,323,M11.2.6/14:7",
                Some([-79200, -79200, -79200, -79200, -79200, -79200]),
            ),
            (
                "AZA6:9",
                Some([-22140, -22140, -22140, -22140, -22140, -22140]),
            ),
            ("LQL24YRSTCR6,365/0:7,17/167", None),
            ("HSBAVS41IVQ-5:2,345/167,M5.2.5/85", None),
            (
                "ZWJIXMX7",
                Some([-25200, -25200, -25200, -25200, -25200, -25200]),
            ),
            (
                "ZCPUZMP-11",
                Some([39600, 39600, 39600, 39600, 39600, 39600]),
            ),
            ("XKNKPO43<09+3>24,J386/35,J365/167", None),
            (
                "ZYXSHO+2SVCNKNM+19,J365/24,M11.3.1/6",
                Some([-7200, -68400, -68400, -68400, -7200, -7200]),
            ),
            ("VQF7VCDOH4,M2.3.1/167,J365/24", None),
            ("RAFGT12:4UZKD-0:1,J380/18:7,M8.2.7/24", None),
            ("TGTWJIH24<0563>,365/0,J365/24", None),
            (
                "HUDA+18EHAZ,3,M7.1.2/8:3",
                Some([-64800, -61200, -61200, -61200, -64800, -64800]),
            ),
            (
                "MXUZORA-10:7",
                Some([36420, 36420, 36420, 36420, 36420, 36420]),
            ),
            ("BMT11:6:1VSFAXI,J365/167,M5.2.0/125", None),
            ("TKRGXCV-11:8GXEL-10", None),
            ("CNYR44", None),
            ("XSLWR7NCFEPG1:0:9,M3.5.5/28:5,233/167", None),
            (
                "NEZ9:0:9",
                Some([-32409, -32409, -32409, -32409, -32409, -32409]),
            ),
            ("TCNQTI6CLKFQRG44", None),
            (
                "LUBGTK+22QXEPCH,365/23,M7.5.3/4",
                Some([-79200, -75600, -75600, -75600, -79200, -79200]),
            ),
            (
                "PIRCVKT1:4:5",
                Some([-3845, -3845, -3845, -3845, -3845, -3845]),
            ),
            (
                "XMNGL5:4:7",
                Some([-18247, -18247, -18247, -18247, -18247, -18247]),
            ),
            ("LGXMTGT24GDAHCJG24,M4.4.3,J142/15", None),
            ("BYFYHEV24<492->9:5", None),
            ("PQT17BGFK,M5.1.1/121,M3.4.3/6:3", None),
            ("YPQLOJA-6<016->-8:1,J365,J365/167", None),
            ("DCZAP-15", Some([54000, 54000, 54000, 54000, 54000, 54000])),
            (
                "DKNQLY+18<0963>4,365/14,M11.4.5/8",
                Some([-64800, -14400, -14400, -14400, -64800, -64800]),
            ),
            ("ZKPS-14EVYV,365/24:7,M8.1.7/24", None),
            ("HKXUN+7FWRET,365/2,M9.2.6/165", None),
            ("HYDY2<+341+->24,J365/-2,M2.6.7/24", None),
            ("HMXMVW-18ANMLAB+2", None),
            ("EVWFMZ21LIHGRSF", None),
            ("WBETY31YNOPW26,M1.5.6/0,J234/41", None),
            (
                "VSTGNEJ9",
                Some([-32400, -32400, -32400, -32400, -32400, -32400]),
            ),
            (
                "NMXIHI-6:9",
                Some([22140, 22140, 22140, 22140, 22140, 22140]),
            ),
            ("VQFKBM22WRMXU5:6:9,J365/167,J196/20:7", None),
            ("<016->-7:6IPWZGJ33,323,J86/7", None),
            ("NGJEP24KFMLID24,M11.1.3/6:1,361/167", None),
            ("DQHIXE33<+78963>,M12.0.7/24,M11.1.3/0", None),
            (
                "NWZEPQX1:0:3",
                Some([-3603, -3603, -3603, -3603, -3603, -3603]),
            ),
            ("BGVWVS-6CJG", None),
            ("FCDSPQH-5:2<0163>0,151/24,J365/-1", None),
            (
                "MHK3:9",
                Some([-11340, -11340, -11340, -11340, -11340, -11340]),
            ),
            (
                "NEFW+8IRCTW,J92/8:1,365/8",
                Some([-25200, -28800, -28800, -25200, -25200, -28800]),
            ),
            ("NULYRA24", None),
            ("DUNSP-13JYNA32", None),
            ("PQJ1:0QJEZWDI-9:4,M6.3.7/24,J365/-2", None),
            (
                "<4567>7",
                Some([-25200, -25200, -25200, -25200, -25200, -25200]),
            ),
            ("NOVWZ7TEHSVU,M11.2.6/8,M5.2.0/41", None),
            (
                "ZGB+15",
                Some([-54000, -54000, -54000, -54000, -54000, -54000]),
            ),
            ("LINSTQH24<0563>4:1:6,365/8,J365/24", None),
            ("XKFO12:1HEVK24,365/0,M1.4.1/141", None),
            ("JUJ-3", Some([10800, 10800, 10800, 10800, 10800, 10800])),
            (
                "ZGVEV1:6:9",
                Some([-3969, -3969, -3969, -3969, -3969, -3969]),
            ),
            ("VAPK24", None),
            (
                "PSFY+2EFKF+22,365/6,J365/24",
                Some([-7200, -7200, -7200, -7200, -7200, -79200]),
            ),
            ("RSHIFK+22<416->,365/2,M1.2.1/111", None),
            ("DMPG29<6-4523>", None),
            (
                "MXAD15",
                Some([-54000, -54000, -54000, -54000, -54000, -54000]),
            ),
            ("NQVGB-5:8<452->8:7,J12/4:3,M8.3.7/24", None),
            ("FCZYB24ONCNCZG24", None),
            (
                "OLYNIR7",
                Some([-25200, -25200, -25200, -25200, -25200, -25200]),
            ),
            ("RUTG8YXIXY1,J365/167,M1.4.4/131", None),
            ("ZGH24KJARAB35", None),
            ("<896->24QNEHGDW+27,J365/167,J365/24", None),
            ("VWFM24", None),
            ("LGTS6<2341+->12:9,M1.3.2/85,M3.5.5/0:3", None),
            ("ATCLY-0:7DEREPA-1:8,217/167,J365/24", None),
            ("XWHGX-13HMHM1,M3.1.5/12:7,201/167", None),
            ("TWZQDM-4:1FGPAR24,M11.5.0/28:3,249/167", None),
            (
                "BILOH3",
                Some([-10800, -10800, -10800, -10800, -10800, -10800]),
            ),
            ("LWLEVIP7:0:5RIPY,J276/22:9,M2.0.7/24", None),
            ("NWJS12SNIHGT-0", None),
            ("<678563>28", None),
            ("BMB24<896->,189/-1,M10.1.7/24", None),
            ("VYLONQR7:2:7DYBAD,301/-3,365/6", None),
            ("PETSDWD1:2CPYXA9:4:3,M11.2.4/6,M5.1.6/171", None),
            ("NKNIZK27KTMTW-13,J232/8,J212/10:5", None),
            ("YDAVMX1LILWT0:7:8,365/8,M1.3.4/29", None),
            ("BYHS8:9:8IXGDAX24,M1.2.1/9,M3.2.3/6", None),
            ("RMN3:6<8967>8:7:0", None),
            ("SHE43<09+3>,365/0,J324/8:5", None),
            ("<85+7>24EVOPGJ+22,365/4:9,M4.4.7/24", None),
            ("HYZMXYL+29", None),
            ("XWVGZ-17DOLYPY30,M1.1.2/18,M11.1.6/4", None),
            ("LSB11:8ETI3:8,M6.3.1/167,J12/4:3", None),
            (
                "WRYT19",
                Some([-68400, -68400, -68400, -68400, -68400, -68400]),
            ),
            ("DGHIFO-14<8527>+6,M10.5.1/167,M1.2.2/171", None),
            (
                "HQVWV5:2:7",
                Some([-18127, -18127, -18127, -18127, -18127, -18127]),
            ),
            ("RKFS25<670923>16,365/9,J365/-3", None),
            ("IJY-2:5", Some([7500, 7500, 7500, 7500, 7500, 7500])),
            ("RIFSX0:6KXUVIV10:5:0,365/8,J365/-1", None),
            ("SPCPM-4:3DUBSZKH", None),
            ("MVULOTG-8IBG-9,J365/24,J365/-3", None),
            ("<4563>-7:8<2-0567>-4", None),
            ("ATKXQJ36", None),
            ("LMH24<89+->24,J174/23,M3.1.6/6", None),
            ("ZQV+29NITM+19,J365/-2,65/167", None),
            ("LAHA14EPAV3:9,M1.5.5/24,J365/-1", None),
            ("MBKLI33<+789+->8,M12.4.1/167,J365/24", None),
            ("PQHAR24<+3496->24,J126/10,J365/-2", None),
            ("YPWBKJC6:1:0ILQDG11,J365/167,J284/24:9", None),
            ("IHQHO25KNUJS7,M11.1.2/8,J365/24", None),
            ("TKVAP+29ZYFMFWN", None),
            ("<+785+->24SPCZKVO-3,J206/0,J365/-1", None),
            ("<812->-3", Some([10800, 10800, 10800, 10800, 10800, 10800])),
            ("VWPS-0:9ZUFQFKT2,105/167,J365/24", None),
            ("TIZWH5XYDIBSR,M6.1.5/-3,89/167", None),
            ("FIJOL-7", Some([25200, 25200, 25200, 25200, 25200, 25200])),
            (
                "XCJKLS2:9",
                Some([-7740, -7740, -7740, -7740, -7740, -7740]),
            ),
            ("TCBYP11:8OZAFQNK28,J278/10,J338/173", None),
        ]);
    }

    fn check(cases: &[(&str, Option<[i32; 6]>)]) {
        for (rule, expected) in cases {
            match (parse(rule), expected) {
                (None, None) => {}
                (Some(zone), Some(offsets)) => {
                    let got = INSTANTS.map(|instant| {
                        zone.offset_at(instant)
                            .unwrap_or_else(|| panic!("{rule} at {instant}"))
                    });
                    assert_eq!(&got, offsets, "{rule}");
                }
                (Some(_), None) => panic!("chrono rejects {rule}"),
                (None, Some(_)) => panic!("chrono accepts {rule}"),
            }
        }
    }
}
