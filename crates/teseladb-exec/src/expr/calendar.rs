//! Calendar arithmetic and time zones for the date expressions: civil dates of any year, ISO
//! weeks, and the `timezone` argument (an IANA name of `teseladb-tz`, or an offset `+05:30`,
//! `-0400`, `+03`).

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_tz::Zone;

use super::error;

/// Milliseconds of a second, minute, hour and day.
pub(super) const MS_SECOND: i64 = 1000;
pub(super) const MS_MINUTE: i64 = 60 * MS_SECOND;
pub(super) const MS_HOUR: i64 = 60 * MS_MINUTE;
pub(super) const MS_DAY: i64 = 24 * MS_HOUR;

const UNKNOWN_ZONE: ErrorCode = ErrorCode { code: 40485, name: "Location40485" };

/// Days from 1970-01-01 to `y-m-d` (Howard Hinnant's algorithm, public domain).
pub(super) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The civil date `(year, month, day)` of day number `z` since 1970-01-01.
pub(super) fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

pub(super) fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub(super) fn month_len(y: i64, m: i64) -> i64 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The weekday of day number `z`: 0 for Sunday.
pub(super) fn weekday(z: i64) -> i64 {
    (z + 4).rem_euclid(7)
}

/// The ISO week-numbering year and week of day number `z`.
pub(super) fn iso_week(z: i64) -> (i64, i64) {
    let iso_dow = (weekday(z) + 6) % 7 + 1;
    // The Thursday of the same ISO week decides its year.
    let thursday = z - iso_dow + 4;
    let (year, _, _) = civil(thursday);
    let week = (thursday - days_from_civil(year, 1, 1)) / 7 + 1;
    (year, week)
}

/// Day number of the Monday of ISO week 1 of `year`.
pub(super) fn iso_week_one(year: i64) -> i64 {
    let jan4 = days_from_civil(year, 1, 4);
    jan4 - (weekday(jan4) + 6) % 7
}

/// The parts of a local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Parts {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub millisecond: i64,
    /// Day number since 1970-01-01.
    pub days: i64,
}

impl Parts {
    /// The parts of local milliseconds `local`.
    pub(super) fn of(local: i64) -> Parts {
        let days = local.div_euclid(MS_DAY);
        let rem = local.rem_euclid(MS_DAY);
        let (year, month, day) = civil(days);
        Parts {
            year,
            month,
            day,
            hour: rem / MS_HOUR,
            minute: rem / MS_MINUTE % 60,
            second: rem / MS_SECOND % 60,
            millisecond: rem % MS_SECOND,
            days,
        }
    }

    /// Day of the year, from 1.
    pub(super) fn day_of_year(&self) -> i64 {
        self.days - days_from_civil(self.year, 1, 1) + 1
    }

    /// Week of the year with weeks starting on Sunday, 0 before the first Sunday (`%U`).
    pub(super) fn week(&self) -> i64 {
        (self.day_of_year() - 1 + 7 - weekday(self.days)) / 7
    }
}

/// A time zone of the `timezone` argument.
#[derive(Clone, Copy, Debug)]
pub(super) enum Tz {
    Utc,
    Fixed(i32),
    Named(Zone),
}

impl Tz {
    /// The zone `text` names, or the reference server's 40485.
    pub(super) fn parse(text: &str) -> Result<Tz, MqlError> {
        if let Some(off) = fixed_offset(text) {
            return Ok(Tz::Fixed(off));
        }
        Zone::named(text).map(Tz::Named).ok_or_else(|| {
            error(UNKNOWN_ZONE, format!("unrecognized time zone identifier: \"{text}\""))
        })
    }

    /// The offset in seconds at UTC milliseconds `ms`.
    pub(super) fn offset_at(&self, ms: i64) -> i32 {
        match self {
            Tz::Utc => 0,
            Tz::Fixed(o) => *o,
            Tz::Named(z) => z.at(ms.div_euclid(MS_SECOND)).offset,
        }
    }

    /// Local milliseconds of UTC milliseconds `ms`.
    pub(super) fn local(&self, ms: i64) -> i64 {
        ms.saturating_add(i64::from(self.offset_at(ms)) * MS_SECOND)
    }

    /// UTC milliseconds of local milliseconds `local`, as the reference server resolves a local time: a time
    /// in a gap is read with the offset before it, one that happens twice is the later.
    pub(super) fn utc(&self, local: i64) -> i64 {
        match self {
            Tz::Utc => local,
            Tz::Fixed(o) => local - i64::from(*o) * MS_SECOND,
            Tz::Named(z) => {
                let secs = local.div_euclid(MS_SECOND);
                let ms = local.rem_euclid(MS_SECOND);
                z.utc_of(secs) * MS_SECOND + ms
            }
        }
    }
}

/// The seconds of an offset `+HH:MM`, `+HHMM` or `+HH` (`-` too).
fn fixed_offset(text: &str) -> Option<i32> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    let colon_ok = match rest.find(':') {
        None => true,
        Some(2) => rest.len() == 5,
        Some(_) => false,
    };
    if !colon_ok || !digits.bytes().all(|b| b.is_ascii_digit()) || !matches!(digits.len(), 2 | 4) {
        return None;
    }
    let h: i32 = digits[..2].parse().ok()?;
    let m: i32 = if digits.len() == 4 { digits[2..].parse().ok()? } else { 0 };
    Some(sign * (h * 3600 + m * 60))
}
