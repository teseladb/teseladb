//! Date text without a format, as RefDb's timelib reads it (measured on a `refrouter` 9.0.4):
//! every form of PHP's "Supported Date and Time Formats" — ISO 8601, American and European
//! dates, textual months, ISO weeks, days of the year, 12- and 24-hour times, zone
//! abbreviations and offsets, `@` timestamps and relative dates (`+1 week`, `next monday`,
//! `last day of`, `2 days ago`) — scanned token by token with the longest match, with timelib's
//! errors and warnings and RefDb's refusal of an incomplete date.

use super::calendar::month_len;
use super::date_pattern::longest;
use super::date_relative::local_micros;
use super::date_rules::State;
use super::date_words::MONTH_NAMES;

/// The zone a date text carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextZone {
    None,
    Z,
    Offset(i32),
    Abbreviation(i32),
}

/// A parsed date text: the local microseconds and its zone.
pub(super) struct Parsed {
    pub local_us: i128,
    pub zone: TextZone,
    pub abbreviation: String,
}

/// Why a date text gives no date.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DateFail {
    /// timelib's errors, or RefDb's message.
    Text(String),
    /// A date past what RefDb's microseconds hold (its error 159).
    Overflow,
}

/// The year timelib leaves unset, for the length of a month without one (it is no leap year).
const UNSET_YEAR: i64 = 1;

/// The microseconds of a time timelib leaves unset (`HHMM`), which RefDb adds as they are.
const UNSET_MICROS: i64 = -9_999_999;

/// A cursor over a date text, for the formats of `$dateFromString`.
pub(super) struct Scan<'a> {
    pub s: &'a str,
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Scan<'a> {
    pub(super) fn new(s: &'a str) -> Scan<'a> {
        Scan { s, b: s.as_bytes(), i: 0 }
    }

    pub(super) fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    /// Up to `max` digits, with how many there were.
    pub(super) fn digits(&mut self, max: usize) -> Option<(i64, usize)> {
        let start = self.i;
        while self.i - start < max && self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let n = self.i - start;
        if n == 0 {
            return None;
        }
        self.s[start..self.i].parse().ok().map(|v| (v, n))
    }

    pub(super) fn eat(&mut self, c: u8) -> bool {
        let found = self.peek() == Some(c);
        if found {
            self.i += 1;
        }
        found
    }

    pub(super) fn word(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        self.s[start..self.i].to_owned()
    }

    pub(super) fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    /// The character at `at` as timelib shows it (`'\0'` past the end).
    pub(super) fn char_at(&self, at: usize) -> char {
        self.b.get(at).map_or('\0', |c| char::from(*c))
    }
}

/// The month of a name or its three letters.
pub(super) fn month_of(word: &str) -> Option<i64> {
    MONTH_NAMES.iter().find(|(w, _)| w.eq_ignore_ascii_case(word)).map(|(_, m)| *m)
}

/// The white space timelib trims and skips.
fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Reads `text` without a format.
pub(super) fn parse_free(text: &str) -> Result<Parsed, DateFail> {
    let trimmed = text.trim_matches(|c: char| c.is_ascii() && is_blank(c as u8));
    if trimmed.is_empty() {
        return Err(DateFail::Text("0: Empty string '\0'".to_owned()));
    }
    let st = scan(trimmed);
    if !st.errors.is_empty() || !st.warnings.is_empty() {
        let all: Vec<String> = st.errors.iter().chain(&st.warnings).cloned().collect();
        return Err(DateFail::Text(all.join("; ")));
    }
    let (Some(y), Some(m), Some(d)) = (st.y, st.m, st.d) else {
        return Err(DateFail::Text(format!(
            "an incomplete date/time string has been found, with elements missing: \"{text}\""
        )));
    };
    let us = match st.us {
        None if st.have_time > 0 => UNSET_MICROS,
        us => us.unwrap_or(0),
    };
    let time = (st.h.unwrap_or(0), st.i.unwrap_or(0), st.s.unwrap_or(0), us);
    let local_us = local_micros((y, m, d), time, &st.rel).ok_or(DateFail::Overflow)?;
    Ok(Parsed { local_us, zone: st.zone.unwrap_or(TextZone::None), abbreviation: st.abbreviation })
}

/// Scans the trimmed text token by token, then checks the date and time it gave.
fn scan(text: &str) -> State {
    let b = text.as_bytes();
    let mut st = State::default();
    let mut i = 0;
    while i < b.len() {
        if is_blank(b[i]) || b[i] == b',' || b[i] == b'.' {
            i += 1;
            continue;
        }
        match longest(b, i) {
            Some((k, end)) => {
                st.apply(k, &text[i..end], i);
                i = end;
            }
            None => {
                st.errors.push(format!("{i}: Unexpected character '{}'", char::from(b[i])));
                i += 1;
            }
        }
    }
    let end = b.len() + 1;
    let valid_time = |v: Option<i64>, max: i64| v.is_some_and(|v| (0..=max).contains(&v));
    // A time whose token stopped at an error has no hour, and timelib checks none then.
    let checked = st.have_time > 0 && st.h.is_some();
    if checked && !(valid_time(st.h, 23) && valid_time(st.i, 59) && valid_time(st.s, 59)) {
        st.warnings.push(format!("{end}: The parsed time was invalid '\0'"));
    }
    let valid_date = match (st.m, st.d) {
        (Some(m), Some(d)) => {
            (1..=12).contains(&m) && d >= 1 && d <= month_len(st.y.unwrap_or(UNSET_YEAR), m)
        }
        _ => false,
    };
    if st.have_date && !valid_date {
        st.warnings.push(format!("{end}: The parsed date was invalid '\0'"));
    }
    st
}
