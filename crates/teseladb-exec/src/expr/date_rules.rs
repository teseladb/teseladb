//! What each token of date text without a format sets, as the reference server's timelib reads it (measured
//! on a reference router 9.0.4): the date and time fields, the zone, and the relative parts, with
//! timelib's errors for a second date, time or zone.

use super::calendar::{days_from_civil, iso_week_one};
use super::date_free::TextZone;
use super::date_pattern::Kind;
use super::date_relative::Rel;
use super::date_words::{
    ABBREVIATIONS, DAY_NAMES, MONTH_NAMES, ORDINALS, ROMAN_MONTHS, Unit, lookup, unit_of,
};

/// The fields read so far.
#[derive(Default)]
pub(super) struct State {
    pub y: Option<i64>,
    pub m: Option<i64>,
    pub d: Option<i64>,
    pub h: Option<i64>,
    pub i: Option<i64>,
    pub s: Option<i64>,
    pub us: Option<i64>,
    pub have_date: bool,
    /// How many times a time was given: an `HHMM` after one is a year, a third a second time.
    pub have_time: u8,
    /// How many zones the text gave (the first second one is a warning, the rest errors).
    pub zones: u32,
    pub zone: Option<TextZone>,
    pub abbreviation: String,
    pub rel: Rel,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// A reader over one token, as timelib reads its numbers and words.
struct Tok<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Tok<'a> {
    /// The next run of up to `max` digits after anything else, with its length.
    fn nr(&mut self, max: usize) -> Option<(i64, usize)> {
        while self.i < self.b.len() && !self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let start = self.i;
        while self.i < self.b.len() && self.i - start < max && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let n = self.i - start;
        std::str::from_utf8(&self.b[start..self.i]).ok()?.parse().ok().map(|v| (v, n))
    }

    fn num(&mut self, max: usize) -> i64 {
        self.nr(max).map_or(0, |(v, _)| v)
    }

    /// A year of up to four digits, two-digit years read as 1970–2069.
    fn year(&mut self) -> i64 {
        match self.nr(4) {
            Some((y, n)) if n < 4 && y < 100 => y + if y < 70 { 2000 } else { 1900 },
            Some((y, _)) => y,
            None => 0,
        }
    }

    /// A day, past its `st`/`nd`/`rd`/`th` (lower-case only).
    fn day(&mut self) -> i64 {
        let d = self.num(2);
        let rest = &self.b[self.i..];
        if rest.len() >= 2 && ["st", "nd", "rd", "th"].iter().any(|s| rest[..2] == *s.as_bytes()) {
            self.i += 2;
        }
        d
    }

    /// The next word of letters.
    fn word(&mut self) -> &'a str {
        while self.i < self.b.len() && !self.b[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_alphabetic() {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i]).unwrap_or_default()
    }

    /// A month by name, three letters or Roman numerals.
    fn month(&mut self) -> i64 {
        let w = self.word();
        let roman = ROMAN_MONTHS.iter().find(|(r, _)| *r == w).map(|(_, m)| *m);
        lookup(&MONTH_NAMES, w).or(roman).unwrap_or(0)
    }

    /// The fraction after the seconds, as microseconds (six digits at most).
    fn micros(&mut self) -> i64 {
        while self.i < self.b.len() && !self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let digits = std::str::from_utf8(&self.b[start..self.i]).unwrap_or_default();
        let six: String = digits.chars().chain(std::iter::repeat('0')).take(6).collect();
        six.parse().unwrap_or(0)
    }

    /// Whether the token's meridian is `pm` (`None` without one).
    fn meridian(&self) -> Option<bool> {
        self.b[self.i..]
            .iter()
            .find(|c| matches!(c, b'a' | b'A' | b'p' | b'P'))
            .map(|c| matches!(c, b'p' | b'P'))
    }
}

/// An hour of 1–12 with its meridian on a 24-hour clock.
fn with_meridian(h: i64, pm: Option<bool>) -> i64 {
    match pm {
        Some(true) if h != 12 => h + 12,
        Some(false) if h == 12 => 0,
        _ => h,
    }
}

impl State {
    fn error(&mut self, at: usize, c: u8, what: &str) {
        self.errors.push(format!("{at}: {what} '{}'", char::from(c)));
    }

    /// Whether a date may be set here; a second one is an error.
    fn take_date(&mut self, at: usize, c: u8) -> bool {
        if self.have_date {
            self.error(at, c, "Double date specification");
            return false;
        }
        self.have_date = true;
        true
    }

    /// Whether a time may be set here; a second one is an error.
    fn take_time(&mut self, at: usize, c: u8) -> bool {
        if self.have_time > 0 {
            self.error(at, c, "Double time specification");
            return false;
        }
        self.have_time = 1;
        true
    }

    /// Whether a zone may be set here: the second zone is a warning, any later one an error.
    fn take_zone(&mut self, at: usize, c: u8) -> bool {
        self.zones += 1;
        if self.zones == 1 {
            return true;
        }
        let msg = format!("{at}: Double timezone specification '{}'", char::from(c));
        if self.zones > 2 {
            self.errors.push(msg)
        } else {
            self.warnings.push(msg)
        }
        false
    }

    /// Clears the time (`tomorrow`, a weekday by name).
    fn unhave_time(&mut self) {
        self.have_time = 0;
        (self.h, self.i, self.s, self.us) = (Some(0), Some(0), Some(0), Some(0));
    }

    fn set_time(&mut self, h: i64, i: i64, s: i64, us: Option<i64>) {
        (self.h, self.i, self.s, self.us) = (Some(h), Some(i), Some(s), us);
    }

    fn set_date(&mut self, y: Option<i64>, m: i64, d: i64) {
        if y.is_some() {
            self.y = y;
        }
        (self.m, self.d) = (Some(m), Some(d));
    }

    /// Applies the token `tok` of rule `k`, which starts at `at`.
    pub(super) fn apply(&mut self, k: Kind, tok: &str, at: usize) {
        let b = tok.as_bytes();
        let c = b.first().copied().unwrap_or(0);
        let mut t = Tok { b, i: 0 };
        match k {
            Kind::Now => {}
            Kind::Noon => {
                self.unhave_time();
                self.have_time = 1;
                self.h = Some(12);
            }
            Kind::Midnight => self.unhave_time(),
            Kind::Tomorrow | Kind::Yesterday => {
                // Set, not added: `+1 day tomorrow` is tomorrow.
                self.unhave_time();
                self.rel.d = if k == Kind::Tomorrow { 1 } else { -1 };
            }
            Kind::BackOf => {
                self.unhave_time();
                self.have_time = 1;
                let h = t.num(2);
                // timelib tells them apart by a lower-case `b`: `BACK OF 7` is 6:45.
                let (h, i) = if c == b'b' { (h, 15) } else { (h - 1, 45) };
                self.set_time(with_meridian(h, t.meridian()), i, 0, Some(0));
            }
            Kind::FirstDayOf => {
                self.unhave_time();
                self.rel.first_last = if c.eq_ignore_ascii_case(&b'f') { 1 } else { 2 };
            }
            Kind::WeekdayOf => {
                // `first`–`twelfth` and `next` count from the first of the month; `last`,
                // `previous` and `this` from the first of the next.
                let (n, this) = ordinal(t.word());
                let w = lookup(&DAY_NAMES, t.word()).unwrap_or(0);
                self.rel.in_month = if n > 0 { 1 } else { 2 };
                self.relative(n, Unit::Named(w), n > 0 || this, true);
            }
            Kind::TimeTiny12 | Kind::TimeShort12 | Kind::TimeLong12 | Kind::MsSqlTime => {
                if self.take_time(at, c) {
                    self.time12(k, &mut t);
                }
            }
            Kind::TimeTiny24
            | Kind::TimeShort24
            | Kind::TimeLong24
            | Kind::Iso8601Long
            | Kind::Iso8601NoColon => {
                if self.take_time(at, c) {
                    self.time24(k, &mut t);
                }
            }
            Kind::GnuNoColon => match self.have_time {
                0 => {
                    // The microseconds stay as they were: unset, they come out as −9.999999 s.
                    self.have_time = 1;
                    let (h, i) = (t.num(2), t.num(2));
                    let us = self.us;
                    self.set_time(h, i, 0, us);
                }
                // A time is there already: the four digits are a year.
                1 => {
                    self.have_time = 2;
                    self.y = Some(t.num(4));
                }
                _ => self.error(at, c, "Double time specification"),
            },
            Kind::Year4 => self.y = Some(t.num(4)),
            Kind::Timestamp | Kind::TimestampMs => self.timestamp(&mut t, at, c),
            Kind::RelativeNumber => self.relative_number(tok, at, c),
            Kind::RelativeText => {
                let (n, this) = ordinal(t.word());
                if let Some(u) = unit_of(tok[t.i..].trim_start(), false) {
                    self.relative(n, u, this, true);
                }
            }
            Kind::RelativeWeek => {
                let (n, _) = ordinal(t.word());
                self.rel.d += 7 * n;
                if !self.rel.have_weekday {
                    self.rel.have_weekday = true;
                    self.rel.weekday = 1;
                }
                self.rel.behavior = 2;
            }
            Kind::Ago => self.rel.negate(),
            Kind::DayName => {
                self.unhave_time();
                self.rel.have_weekday = true;
                // `weekday` alone moves to a Monday, as timelib's table gives it 1.
                self.rel.weekday = lookup(&DAY_NAMES, tok).unwrap_or(1);
                if self.rel.behavior != 2 {
                    self.rel.behavior = 1;
                }
            }
            Kind::TzCorrection => {
                if self.take_zone(at, c) {
                    self.zone = Some(TextZone::Offset(correction(tok)));
                }
            }
            Kind::Tz => self.tz(tok, at, c),
            _ => self.date(k, &mut t, tok, at, c),
        }
    }

    fn time12(&mut self, k: Kind, t: &mut Tok<'_>) {
        let h = t.num(2);
        let (i, s) = match k {
            Kind::TimeTiny12 => (0, 0),
            Kind::TimeShort12 => (t.num(2), 0),
            _ => (t.num(2), t.num(2)),
        };
        let us = if k == Kind::MsSqlTime { t.micros() } else { 0 };
        self.set_time(with_meridian(h, t.meridian()), i, s, Some(us));
    }

    fn time24(&mut self, k: Kind, t: &mut Tok<'_>) {
        let h = t.num(2);
        let i = if k == Kind::TimeTiny24 { 0 } else { t.num(2) };
        let s = if matches!(k, Kind::TimeTiny24 | Kind::TimeShort24) { 0 } else { t.num(2) };
        let us = if k == Kind::Iso8601Long { t.micros() } else { 0 };
        self.set_time(h, i, s, Some(us));
    }

    /// `@seconds[.fraction]`: 1970-01-01 in UTC plus those seconds, as relative ones.
    fn timestamp(&mut self, t: &mut Tok<'_>, at: usize, c: u8) {
        self.have_date = false;
        self.have_time = 0;
        if !self.take_zone(at, c) {
            return;
        }
        let negative = t.b.get(1) == Some(&b'-');
        let start = if negative { 2 } else { 1 };
        let end =
            t.b[start..].iter().position(|c| !c.is_ascii_digit()).map_or(t.b.len(), |p| p + start);
        let Some(secs) =
            std::str::from_utf8(&t.b[start..end]).ok().and_then(|s| s.parse::<i64>().ok())
        else {
            self.error(at, c, "Number out of range");
            return;
        };
        t.i = end;
        if end + 1 == t.b.len() {
            // `@1215000000.` with no digits after the dot.
            self.error(at, c, "Found unexpected data");
        }
        let micros = if end < t.b.len() { t.micros() } else { 0 };
        let sign = if negative { -1 } else { 1 };
        self.set_date(Some(1970), 1, 1);
        self.set_time(0, 0, 0, Some(0));
        self.rel.s = self.rel.s.saturating_add(sign * secs);
        self.rel.us = self.rel.us.saturating_add(sign * micros);
        self.zone = Some(TextZone::Offset(0));
    }

    /// `[+-]* number unit`.
    fn relative_number(&mut self, tok: &str, at: usize, c: u8) {
        let minus = tok.bytes().take_while(|c| !c.is_ascii_digit()).filter(|c| *c == b'-').count();
        let start = tok.find(|c: char| c.is_ascii_digit()).unwrap_or(0);
        let end = tok[start..].find(|c: char| !c.is_ascii_digit()).map_or(tok.len(), |p| p + start);
        // A number too long for 64 bits saturates, as `strtoll` does.
        let n = tok[start..end].parse::<i64>().unwrap_or(i64::MAX);
        let n = if minus % 2 == 1 { -n } else { n };
        match unit_of(tok[end..].trim_start(), true) {
            // After a number, a named weekday counts the day itself (`2 tuesdays` on a Tuesday
            // is a week later).
            Some(u) => self.relative(n, u, true, false),
            None => self.error(at, c, "Unexpected character"),
        }
    }

    /// Adds `n` of unit `u`; `same_day` makes a named weekday count the day itself, and
    /// `clear_time` resets the time for one (`next monday` does, `+1 monday` does not).
    fn relative(&mut self, n: i64, u: Unit, same_day: bool, clear_time: bool) {
        let r = &mut self.rel;
        let add = |field: &mut i64, by: i64| *field = field.saturating_add(by);
        match u {
            Unit::Micro => add(&mut r.us, n),
            Unit::Milli => add(&mut r.us, n.saturating_mul(1000)),
            Unit::Second => add(&mut r.s, n),
            Unit::Minute => add(&mut r.i, n),
            Unit::Hour => add(&mut r.h, n),
            Unit::Day => add(&mut r.d, n),
            Unit::Week => add(&mut r.d, n.saturating_mul(7)),
            Unit::Fortnight => add(&mut r.d, n.saturating_mul(14)),
            Unit::Month => add(&mut r.m, n),
            Unit::Year => add(&mut r.y, n),
            Unit::Weekday => {
                r.have_special = true;
                add(&mut r.special, n);
            }
            Unit::Named(w) => {
                let weeks = if n > 0 { n - 1 } else { n };
                add(&mut r.d, weeks.saturating_mul(7));
                r.have_weekday = true;
                r.weekday = w;
                r.behavior = u8::from(same_day);
                if clear_time {
                    self.unhave_time();
                }
            }
        }
    }

    fn tz(&mut self, tok: &str, at: usize, c: u8) {
        if !self.take_zone(at, c) {
            return;
        }
        match zone_of(tok) {
            Some((z, word)) => {
                self.zone = Some(z);
                self.abbreviation = word.to_owned();
            }
            None => self.zone_error(at, c),
        }
    }

    /// The date rules.
    fn date(&mut self, k: Kind, t: &mut Tok<'_>, tok: &str, at: usize, c: u8) {
        if matches!(k, Kind::Clf | Kind::Exif) {
            self.stamp(k, t, tok, at, c);
            return;
        }
        if matches!(
            k,
            Kind::DateShortWithTimeShort
                | Kind::DateShortWithTimeLong
                | Kind::DateShortWithTimeLongTz
                | Kind::DateShortWithTimeShort12
                | Kind::DateShortWithTimeLong12
        ) {
            self.date_with_time(k, t, tok, at, c);
            return;
        }
        if k == Kind::MonthAlone {
            if self.take_date(at, c) {
                self.m = lookup(&MONTH_NAMES, tok);
            }
            return;
        }
        if !self.take_date(at, c) {
            return;
        }
        match k {
            Kind::Americanshort => {
                let m = t.num(2);
                let d = t.day();
                self.set_date(None, m, d);
            }
            Kind::American => {
                let (m, d) = (t.num(2), t.day());
                let y = t.year();
                self.set_date(Some(y), m, d);
            }
            Kind::Iso8601DateSlash | Kind::DateSlash | Kind::DateNoColon => {
                let (y, m) = (t.num(4), t.num(2));
                let d = t.day();
                self.set_date(Some(y), m, d);
            }
            Kind::Iso8601Date4 | Kind::Iso8601DateX => self.signed_date(t, tok, at, c),
            Kind::Iso8601Date2 | Kind::GnuDateShort => {
                let y = t.year();
                let (m, d) = (t.num(2), t.day());
                self.set_date(Some(y), m, d);
            }
            Kind::GnuDateShorter => {
                let (y, m) = (t.num(4), t.num(2));
                self.set_date(Some(y), m, 1);
            }
            Kind::PointedDate4 | Kind::PointedDate2 => {
                let (d, m) = (t.day(), t.num(2));
                let y = t.year();
                self.set_date(Some(y), m, d);
            }
            Kind::DateFull => {
                let (d, m) = (t.day(), t.month());
                let y = t.year();
                self.set_date(Some(y), m, d);
            }
            Kind::DateNoDay => {
                let (m, y) = (t.month(), t.num(4));
                self.set_date(Some(y), m, 1);
            }
            Kind::DateNoDayRev => {
                let (y, m) = (t.num(4), t.month());
                self.set_date(Some(y), m, 1);
            }
            Kind::DateTextual | Kind::PgTextShort => {
                let (m, d) = (t.month(), t.day());
                let y = t.year();
                self.set_date(Some(y), m, d);
            }
            Kind::DateNoYear => {
                // A month and day clear a year read before them (`1978, Jul 4` is
                // incomplete), as no other date without a year does.
                let (m, d) = (t.month(), t.day());
                self.y = None;
                self.set_date(None, m, d);
            }
            Kind::DateNoYearRev => {
                let (d, m) = (t.day(), t.month());
                self.set_date(None, m, d);
            }
            Kind::PgYearDotDay => {
                let (y, doy) = (t.num(4), t.num(3));
                self.set_date(Some(y), 1, doy);
            }
            Kind::PgTextReverse => {
                let y = t.year();
                let (m, d) = (t.month(), t.day());
                self.set_date(Some(y), m, d);
            }
            Kind::IsoWeekDay | Kind::IsoWeek => {
                let (y, w) = (t.num(4), t.num(2));
                let dow = if k == Kind::IsoWeekDay { t.num(1) } else { 1 };
                self.set_date(Some(y), 1, 1);
                // Set, not added: `2 days ago 2008W27` is that week's Monday.
                let day = iso_week_one(y) + (w - 1) * 7 + dow - 1;
                self.rel.d = day - days_from_civil(y, 1, 1);
            }
            _ => {}
        }
    }

    /// The common log format and EXIF, a date and a time in one token: the time is checked
    /// first, and the zone of the log format counts as no zone (a later one replaces it).
    fn stamp(&mut self, k: Kind, t: &mut Tok<'_>, tok: &str, at: usize, c: u8) {
        if !self.take_time(at, c) || !self.take_date(at, c) {
            return;
        }
        let (y, m, d) = if k == Kind::Clf {
            let (d, m) = (t.day(), t.month());
            (t.num(4), m, d)
        } else {
            (t.num(4), t.num(2), t.num(2))
        };
        self.set_date(Some(y), m, d);
        let (h, i, s) = (t.num(2), t.num(2), t.num(2));
        self.set_time(h, i, s, Some(0));
        if k == Kind::Clf {
            let zone_at = tok.rfind([' ', '\t']).map_or(0, |p| p + 1);
            self.zone = Some(TextZone::Offset(correction(&tok[zone_at..])));
        }
    }

    /// A month and day with a time (and, in the long form, a zone) in one token. Its seconds
    /// follow a colon only: after a dot the rest is read as a zone, which fails. Its zone counts
    /// as none and a later one replaces it; a zone it cannot read is an error at the token.
    fn date_with_time(&mut self, k: Kind, t: &mut Tok<'_>, tok: &str, at: usize, c: u8) {
        if !self.take_date(at, c) {
            return;
        }
        let (m, d) = (t.month(), t.day());
        self.set_date(None, m, d);
        if !self.take_time(at, c) {
            return;
        }
        let (h, i) = (t.num(2), t.num(2));
        let long = matches!(
            k,
            Kind::DateShortWithTimeLong
                | Kind::DateShortWithTimeLongTz
                | Kind::DateShortWithTimeLong12
        );
        let mut s = 0;
        if long {
            if t.b.get(t.i) != Some(&b':') {
                self.set_time(h, i, 0, Some(0));
                self.zone_error(at, c);
                return;
            }
            s = t.num(2);
        }
        let twelve = matches!(k, Kind::DateShortWithTimeShort12 | Kind::DateShortWithTimeLong12);
        let h = if twelve { with_meridian(h, t.meridian()) } else { h };
        self.set_time(h, i, s, Some(0));
        if k == Kind::DateShortWithTimeLongTz {
            let zone = tok[t.i..].trim_start_matches([' ', '\t']);
            match zone_of(zone) {
                Some((z, word)) => {
                    self.zone = Some(z);
                    self.abbreviation = word.to_owned();
                }
                None => self.zone_error(at, c),
            }
        }
    }

    fn zone_error(&mut self, at: usize, c: u8) {
        self.error(at, c, "passing a time zone identifier as part of the string is not allowed");
    }

    /// `[+-]YYYY-MM-DD` and `[+-]YYYYY…-MM-DD`.
    fn signed_date(&mut self, t: &mut Tok<'_>, tok: &str, at: usize, c: u8) {
        let negative = tok.starts_with('-');
        let start = usize::from(tok.starts_with(['+', '-']));
        let end = tok[start..].find('-').map_or(tok.len(), |p| p + start);
        let Ok(y) = tok[start..end].parse::<i64>() else {
            self.have_date = false;
            self.error(at, c, "Number out of range");
            return;
        };
        t.i = end;
        let (m, d) = (t.num(2), t.num(2));
        self.set_date(Some(if negative { -y } else { y }), m, d);
    }
}

/// The zone of an abbreviation (in parentheses or not) or a correction, with the word as
/// written; `None` for anything else (the reference server refuses zone identifiers).
fn zone_of(text: &str) -> Option<(TextZone, &str)> {
    if text.starts_with(['+', '-']) || text.starts_with("GMT+") || text.starts_with("GMT-") {
        return Some((TextZone::Offset(correction(text)), ""));
    }
    let word = text.trim_start_matches('(').trim_end_matches(')');
    let off = lookup(&ABBREVIATIONS, word)?;
    let z = word.eq_ignore_ascii_case("z");
    Some((if z { TextZone::Z } else { TextZone::Abbreviation(off) }, word))
}

/// The amount of an ordinal or relative word, and whether it is `this`.
fn ordinal(word: &str) -> (i64, bool) {
    ORDINALS
        .iter()
        .find(|(w, _, _)| w.eq_ignore_ascii_case(word))
        .map_or((0, false), |(_, n, this)| (*n, *this))
}

/// The seconds of a zone correction (`+5`, `+130`, `+0530`, `+05:30`, `+053010`, `GMT-7`).
fn correction(tok: &str) -> i32 {
    let rest = tok.strip_prefix("GMT").unwrap_or(tok);
    let sign = if rest.starts_with('-') { -1 } else { 1 };
    let body = &rest[1.min(rest.len())..];
    let num = |s: &str| s.parse::<i32>().unwrap_or(0);
    let parts: Vec<&str> = body.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (num(h), num(m), num(s)),
        [h, m] => (num(h), num(m), 0),
        _ => match body.len() {
            0..=2 => (num(body), 0, 0),
            3 => (num(&body[..1]), num(&body[1..]), 0),
            4 => (num(&body[..2]), num(&body[2..]), 0),
            _ => (num(&body[..2]), num(&body[2..4]), num(&body[4..])),
        },
    };
    sign * (h * 3600 + m * 60 + s)
}
