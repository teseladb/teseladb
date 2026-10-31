//! The relative parts of date text without a format and how they move the date, as RefDb's
//! timelib applies them (measured on a `refrouter` 9.0.4): `<nth> <weekday> of` moves to the
//! first of its month; the weekday comes next, on that date; then years, months and days; then
//! `first`/`last day of`; then weekdays as business days; and the time of day on the wall
//! clock.

use super::calendar::{civil, days_from_civil, weekday};

const MICROS_SECOND: i128 = 1_000_000;
const MICROS_DAY: i128 = 86_400 * MICROS_SECOND;

/// The relative parts read so far.
#[derive(Default)]
pub(super) struct Rel {
    pub y: i64,
    pub m: i64,
    pub d: i64,
    pub h: i64,
    pub i: i64,
    pub s: i64,
    pub us: i64,
    /// A weekday to move to (0 for Sunday; negative after `ago`).
    pub have_weekday: bool,
    pub weekday: i64,
    /// 0: the weekday after the day; 1: that day or after; 2: in the week (`monday next week`).
    pub behavior: u8,
    /// Weekdays counted as business days (`+3 weekdays`).
    pub have_special: bool,
    pub special: i64,
    /// 1 for `first day of`, 2 for `last day of`.
    pub first_last: u8,
    /// `<nth> <weekday> of`: 1 counts from the first of the month, 2 from the first of the next.
    pub in_month: u8,
}

impl Rel {
    /// `ago`: every relative part so far the other way, but the microseconds.
    pub(super) fn negate(&mut self) {
        for f in [&mut self.y, &mut self.m, &mut self.d, &mut self.h, &mut self.i, &mut self.s] {
            *f = f.saturating_neg();
        }
        self.weekday = if self.weekday == 0 { -7 } else { -self.weekday };
        self.special = self.special.saturating_neg();
    }
}

/// Moves day number `z` to the weekday `rel` asks for.
fn to_weekday(z: i64, rel: &Rel) -> i64 {
    let current = weekday(z);
    let mut w = rel.weekday;
    if rel.behavior == 2 {
        // In the week from Monday to Sunday.
        if current == 0 && w != 0 {
            w -= 7;
        }
        if w == 0 && current != 0 {
            w = 7;
        }
        return z - current + w;
    }
    if w < 0 {
        return z - (7 - (w.abs() - current));
    }
    let mut diff = w - current;
    if (rel.d < 0 && diff < 0) || (rel.d >= 0 && diff <= -i64::from(rel.behavior)) {
        diff += 7;
    }
    z + diff
}

/// Day number `z` moved by `n` business days (a weekend counts as the Friday before when
/// moving on, the Monday after when moving back, and `+0` moves it to Monday).
fn business_days(z: i64, n: i64) -> i64 {
    let wd = weekday(z);
    let weekend = wd == 0 || wd == 6;
    if n == 0 {
        return if weekend { z + if wd == 6 { 2 } else { 1 } } else { z };
    }
    let (mut z, step) = if n > 0 {
        (if weekend { z - if wd == 6 { 1 } else { 2 } } else { z }, 1)
    } else {
        (if weekend { z + if wd == 6 { 2 } else { 1 } } else { z }, -1)
    };
    z = z.saturating_add((n / 5).saturating_mul(7));
    let mut left = (n % 5).abs();
    while left > 0 {
        z += step;
        if !matches!(weekday(z), 0 | 6) {
            left -= 1;
        }
    }
    z
}

/// Year and month with the month brought into 1–12.
fn normal_month(y: i64, m: i64) -> (i64, i64) {
    (y + (m - 1).div_euclid(12), (m - 1).rem_euclid(12) + 1)
}

/// The local microseconds of a complete date and time with its relative parts, or `None`
/// when they are past what a day number holds.
pub(super) fn local_micros(
    date: (i64, i64, i64),
    time: (i64, i64, i64, i64),
    rel: &Rel,
) -> Option<i128> {
    let (y, m, d) = date;
    // Past this many years, no date fits RefDb's microseconds anyway.
    const MAX_YEAR: i64 = 1_000_000_000_000;
    let limit = |v: i64| v.unsigned_abs() < MAX_YEAR.unsigned_abs();
    if !limit(y) || !limit(rel.y) || !limit(rel.m) || !limit(rel.d) {
        return None;
    }
    // `<nth> <weekday> of` starts from the first of its month, with the months added first.
    let (y, m, d, rel_m) = match rel.in_month {
        0 => (y, m, d, rel.m),
        n => {
            let (y, m) = normal_month(y, m + rel.m + i64::from(n - 1));
            (y, m, 1, 0)
        }
    };
    // `first`/`last day of` moves the date before the weekday and again after the other
    // parts, which are added without bringing the day into its month in between.
    let mut z = match rel.first_last {
        1 => days_from_civil(y, m, 1),
        2 => {
            let (ny, nm) = normal_month(y, m + 1);
            days_from_civil(ny, nm, 1) - 1
        }
        _ => days_from_civil(y, m, 1) + d - 1,
    };
    if rel.have_weekday {
        z = to_weekday(z, rel);
    }
    let (y, m, d) = civil(z);
    let (y, m, d) = match rel.first_last {
        1 => (y + rel.y, m + rel_m, 1),
        2 => (y + rel.y, m + rel_m + 1, 0),
        _ => (y + rel.y, m + rel_m, d + rel.d),
    };
    let (y, m) = normal_month(y, m);
    let mut z = days_from_civil(y, m, 1) + d - 1;
    if rel.have_special {
        z = business_days(z, rel.special);
    }
    let (h, i, s, us) = time;
    let secs = i128::from(h) * 3600
        + i128::from(rel.h) * 3600
        + i128::from(i) * 60
        + i128::from(rel.i) * 60
        + i128::from(s)
        + i128::from(rel.s);
    Some(i128::from(z) * MICROS_DAY + secs * MICROS_SECOND + i128::from(us) + i128::from(rel.us))
}
