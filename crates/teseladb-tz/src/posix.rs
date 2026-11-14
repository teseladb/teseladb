//! POSIX TZ rules (`CET-1CEST,M3.5.0,M10.5.0/3`, `<+0330>-3:30`): the local type of instants
//! after a zone's last transition (RFC 8536, section 3.3).

use crate::Local;

/// The seconds of a day.
const DAY: i64 = 86_400;

/// When a daylight saving period starts or ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Date {
    /// `Jn`: day 1–365, February 29 never counted.
    Julian(u16),
    /// `n`: day 0–365, February 29 counted in leap years.
    Zero(u16),
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Month(u8, u8, u8),
}

/// A named offset: abbreviation and seconds east of UTC.
type Named = (&'static str, i32);
/// A moment of a year: the date and the local time in seconds.
type Moment = (Date, i32);

/// A POSIX TZ rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    std: Named,
    dst: Option<(Named, Moment, Moment)>,
}

struct Cursor {
    s: &'static str,
    i: usize,
}

impl Cursor {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let found = self.peek() == Some(c);
        if found {
            self.i += 1;
        }
        found
    }

    fn name(&mut self) -> Option<&'static str> {
        if self.eat(b'<') {
            let start = self.i;
            while self.peek()? != b'>' {
                self.i += 1;
            }
            let n = &self.s[start..self.i];
            self.i += 1;
            return Some(n);
        }
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        (self.i - start >= 3).then(|| &self.s[start..self.i])
    }

    fn number(&mut self) -> Option<i32> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.s[start..self.i].parse().ok()
    }

    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn time(&mut self) -> Option<i32> {
        let sign = if self.eat(b'-') {
            -1
        } else {
            self.eat(b'+');
            1
        };
        let mut secs = self.number()? * 3600;
        if self.eat(b':') {
            secs += self.number()? * 60;
            if self.eat(b':') {
                secs += self.number()?;
            }
        }
        Some(sign * secs)
    }

    fn date(&mut self) -> Option<Moment> {
        let d = if self.eat(b'M') {
            let m = self.number()?;
            self.eat(b'.');
            let w = self.number()?;
            self.eat(b'.');
            let d = self.number()?;
            Date::Month(u8::try_from(m).ok()?, u8::try_from(w).ok()?, u8::try_from(d).ok()?)
        } else if self.eat(b'J') {
            Date::Julian(u16::try_from(self.number()?).ok()?)
        } else {
            Date::Zero(u16::try_from(self.number()?).ok()?)
        };
        let t = if self.eat(b'/') { self.time()? } else { 7200 };
        Some((d, t))
    }
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days from 1970-01-01 to `y-m-d`.
fn days(y: i64, m: i64, d: i64) -> i64 {
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn month_len(y: i64, m: i64) -> i64 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Date {
    /// Day number (since the epoch) of this date in year `y`.
    fn day_in(self, y: i64) -> i64 {
        let jan1 = days(y, 1, 1);
        match self {
            Date::Julian(n) => {
                let n = i64::from(n);
                jan1 + n - 1 + i64::from(leap(y) && n >= 60)
            }
            Date::Zero(n) => jan1 + i64::from(n),
            Date::Month(m, w, d) => {
                let (m, w, d) = (i64::from(m), i64::from(w), i64::from(d));
                let first = days(y, m, 1);
                let first_dow = (first + 4).rem_euclid(7);
                let mut day = first + (d - first_dow).rem_euclid(7) + (w - 1) * 7;
                while day >= first + month_len(y, m) {
                    day -= 7;
                }
                day
            }
        }
    }
}

impl Rule {
    /// The rule written in `text`; `None` when it is empty or not one.
    #[must_use]
    pub fn parse(text: &'static str) -> Option<Rule> {
        let mut c = Cursor { s: text, i: 0 };
        let std_name = c.name()?;
        let std_off = -c.time()?;
        if c.peek().is_none() {
            return Some(Rule { std: (std_name, std_off), dst: None });
        }
        let dst_name = c.name()?;
        let dst_off =
            if matches!(c.peek(), Some(b',') | None) { std_off + 3600 } else { -c.time()? };
        if !c.eat(b',') {
            return None;
        }
        let start = c.date()?;
        if !c.eat(b',') {
            return None;
        }
        let end = c.date()?;
        Some(Rule { std: (std_name, std_off), dst: Some(((dst_name, dst_off), start, end)) })
    }

    /// The local type at `secs` seconds since the epoch.
    #[must_use]
    pub fn at(&self, secs: i64) -> Local {
        let std = Local { offset: self.std.1, dst: false, abbr: self.std.0 };
        let Some(((name, off), (sd, st), (ed, et))) = self.dst else {
            return std;
        };
        let dst = Local { offset: off, dst: true, abbr: name };
        // The year of the instant in standard time.
        let local_days = (secs + i64::from(self.std.1)).div_euclid(DAY);
        let year = year_of(local_days);
        let start = sd.day_in(year) * DAY + i64::from(st) - i64::from(self.std.1);
        let end = ed.day_in(year) * DAY + i64::from(et) - i64::from(off);
        let inside =
            if start < end { secs >= start && secs < end } else { secs >= start || secs < end };
        if inside { dst } else { std }
    }
}

/// The year of day number `d` since the epoch.
fn year_of(d: i64) -> i64 {
    let z = d + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i64::from(month <= 2)
}
