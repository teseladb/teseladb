//! Date text read with a `$dateFromString` format (measured on a reference router 9.0.4): each `%`
//! specifier reads its field, a literal must be there, and timelib's messages name what was
//! missing. A format that gives no year with a month and a day, a day of the year, or an ISO
//! year, gives an incomplete date.

use super::calendar::{
    MS_DAY, MS_HOUR, MS_MINUTE, MS_SECOND, days_from_civil, iso_week_one, month_len,
};
use super::date_free::{Parsed, Scan, TextZone, month_of};

/// What a format read.
#[derive(Default)]
struct Fields {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    yday: Option<i64>,
    iso_year: Option<i64>,
    iso_week: Option<i64>,
    iso_day: Option<i64>,
    ms: i64,
    zone: Option<i32>,
}

/// How a specifier reads its number, as timelib's: skipping what is no digit before it (and
/// saying so), or not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Skip {
    Loud,
    Quiet,
    No,
}

/// A number of `min` to `max` digits, or the message naming it (`A two digit minute`).
fn number(
    sc: &mut Scan<'_>,
    (min, max, skip): (usize, usize, Skip),
    what: &str,
    errors: &mut Vec<String>,
) -> Option<i64> {
    let at = sc.i;
    if skip != Skip::No && sc.peek().is_some_and(|c| !c.is_ascii_digit()) {
        if skip == Skip::Loud {
            errors.push(format!("{at}: Unexpected data found. '{}'", sc.char_at(at)));
        }
        while sc.peek().is_some_and(|c| !c.is_ascii_digit()) {
            sc.i += 1;
        }
    }
    match sc.digits(max) {
        Some((v, n)) if n >= min => Some(v),
        _ => {
            errors.push(format!("{at}: {what} could not be found '{}'", sc.char_at(at)));
            if skip == Skip::No && !sc.done() {
                sc.i += 1;
            }
            None
        }
    }
}

/// A signed offset `±HHMM`, `±HH:MM` or `±HH`.
fn offset(sc: &mut Scan<'_>) -> i32 {
    let sign = if sc.peek() == Some(b'-') { -1 } else { 1 };
    if matches!(sc.peek(), Some(b'+' | b'-')) {
        sc.i += 1;
    }
    let (h, m) = match sc.digits(4) {
        Some((v, 4)) => (v / 100, v % 100),
        Some((v, _)) => (v, if sc.eat(b':') { sc.digits(2).map_or(0, |(m, _)| m) } else { 0 }),
        None => (0, 0),
    };
    sign * (h as i32 * 3600 + m as i32 * 60)
}

/// Reads `text` with format `f`.
pub(super) fn parse_format(text: &str, f: &str) -> Result<Parsed, String> {
    let mut sc = Scan::new(text);
    let mut v = Fields::default();
    let mut errors = Vec::new();
    let mut spec = f.chars();
    while let Some(c) = spec.next() {
        if c != '%' {
            if sc.done() {
                errors.push(format!("{}: Not enough data available to satisfy format '\0'", sc.i));
                break;
            }
            if sc.peek() != Some(c as u8) {
                errors.push(format!("{}: Format literal not found '{}'", sc.i, sc.char_at(sc.i)));
            }
            sc.i += 1;
            continue;
        }
        let Some(s) = spec.next() else { break };
        if sc.done() && s != '%' {
            errors.push(format!("{}: Not enough data available to satisfy format '\0'", sc.i));
            break;
        }
        match s {
            'Y' => {
                let mut skipped = Vec::new();
                v.year = number(&mut sc, (1, 4, Skip::Loud), "A four digit year", &mut skipped);
                // A year that cannot be read stops timelib.
                if let Some(first) = skipped.into_iter().next() {
                    errors.push(first);
                    if v.year.is_none() {
                        return Err(errors.join("; "));
                    }
                }
            }
            'm' => v.month = number(&mut sc, (1, 2, Skip::Loud), "A two digit month", &mut errors),
            'd' => v.day = number(&mut sc, (1, 2, Skip::Loud), "A two digit day", &mut errors),
            'j' => {
                v.yday =
                    number(&mut sc, (1, 3, Skip::Loud), "A three digit day-of-year", &mut errors)
            }
            'G' => {
                v.iso_year =
                    number(&mut sc, (1, 4, Skip::Quiet), "A four digit ISO year", &mut errors)
            }
            'V' => {
                let at = sc.i;
                v.iso_week =
                    number(&mut sc, (1, 2, Skip::Quiet), "A two digit ISO week", &mut errors);
                if v.iso_week.is_some_and(|w| !(1..=53).contains(&w)) {
                    errors.push(format!(
                        "{at}: ISO Week must be between 1 and 53 '{}'",
                        sc.char_at(at)
                    ));
                }
            }
            'u' => {
                v.iso_day =
                    number(&mut sc, (1, 1, Skip::No), "A single digit day of week", &mut errors)
            }
            'H' => {
                v.ms += number(&mut sc, (1, 2, Skip::Loud), "A two digit hour", &mut errors)
                    .unwrap_or(0)
                    * MS_HOUR
            }
            'M' => {
                v.ms += number(&mut sc, (2, 2, Skip::Loud), "A two digit minute", &mut errors)
                    .unwrap_or(0)
                    * MS_MINUTE
            }
            'S' => {
                v.ms += number(&mut sc, (2, 2, Skip::Loud), "A two digit second", &mut errors)
                    .unwrap_or(0)
                    * MS_SECOND
            }
            'L' => {
                let start = sc.i;
                let _ = sc.digits(3);
                let frac = format!("{:0<3}", &text[start..sc.i]);
                v.ms += frac.parse::<i64>().unwrap_or(0);
            }
            'b' | 'B' => {
                let at = sc.i;
                let w = sc.word();
                v.month = month_of(&w);
                if v.month.is_none() {
                    errors.push(format!(
                        "{at}: A textual month could not be found '{}'",
                        sc.char_at(at)
                    ));
                }
            }
            'z' if !matches!(sc.peek(), Some(b'+' | b'-')) => {
                errors.push(format!(
                    "{}: passing a time zone identifier as part of the string is not allowed '{}'",
                    sc.i,
                    sc.char_at(sc.i)
                ));
                sc.i += 1;
            }
            'z' => v.zone = Some(offset(&mut sc)),
            'Z' => {
                if matches!(sc.peek(), Some(b'+' | b'-')) {
                    let sign = if sc.peek() == Some(b'-') { -1 } else { 1 };
                    sc.i += 1;
                    v.zone = Some(sign * sc.digits(4).map_or(0, |(m, _)| m as i32) * 60);
                } else {
                    errors.push(format!(
                        "{}: Invalid timezone offset in minutes '{}'",
                        sc.i,
                        sc.char_at(sc.i)
                    ));
                }
            }
            _ => {
                if sc.peek() != Some(b'%') {
                    errors.push(format!(
                        "{}: Format literal not found '{}'",
                        sc.i,
                        sc.char_at(sc.i)
                    ));
                }
                sc.i += 1;
            }
        }
    }
    if !sc.done() && errors.iter().all(|e| !e.contains("Not enough data")) {
        errors.push(format!("{}: Trailing data '{}'", sc.i, sc.char_at(sc.i)));
    }
    let incomplete = || {
        format!("an incomplete date/time string has been found, with elements missing: \"{text}\"")
    };
    let days = if let Some(iy) = v.iso_year {
        Some(iso_week_one(iy) + (v.iso_week.unwrap_or(1) - 1) * 7 + v.iso_day.unwrap_or(1) - 1)
    } else {
        match (v.year, v.yday, v.month, v.day) {
            // timelib counts `%j` from day 0.
            (Some(y), Some(j), _, _) => Some(days_from_civil(y, 1, 1) + j),
            (Some(y), None, Some(m), Some(d)) => {
                if (1..=12).contains(&m) && d >= 1 && d <= month_len(y, m) {
                    Some(days_from_civil(y, m, d))
                } else {
                    // An impossible date is one more error, after those of the reading.
                    errors.push(format!("{}: The parsed date was invalid '\0'", text.len()));
                    None
                }
            }
            _ => None,
        }
    };
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let Some(days) = days else { return Err(incomplete()) };
    let zone = v.zone.map_or(TextZone::None, TextZone::Offset);
    let local_us = i128::from(days * MS_DAY + v.ms) * 1000;
    Ok(Parsed { local_us, zone, abbreviation: String::new() })
}
