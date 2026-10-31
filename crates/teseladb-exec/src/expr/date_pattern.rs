//! The token patterns of date text without a format, after the grammar of timelib's "Supported
//! Date and Time Formats" (PHP manual) and measured on a `refrouter` 9.0.4: a small matcher that
//! gives every length a pattern can take at a position, so that the scanner keeps the longest
//! match and, among equally long ones, the rule listed first.

use std::sync::OnceLock;

use super::date_words::{
    DAY_NAMES, MONTH_NAMES, ORDINALS, PLURALS, RELATIVE_TEXT, ROMAN_MONTHS, UNIT_WORDS, UNITS,
};

/// The rules of the scanner, in the order that breaks ties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Now,
    Noon,
    Midnight,
    Tomorrow,
    Yesterday,
    BackOf,
    FirstDayOf,
    WeekdayOf,
    TimeTiny12,
    TimeShort12,
    TimeLong12,
    MsSqlTime,
    TimeTiny24,
    TimeShort24,
    TimeLong24,
    Iso8601Long,
    GnuNoColon,
    Iso8601NoColon,
    Americanshort,
    American,
    Iso8601DateSlash,
    DateSlash,
    Iso8601Date4,
    Iso8601DateX,
    Iso8601Date2,
    GnuDateShorter,
    GnuDateShort,
    PointedDate4,
    PointedDate2,
    DateFull,
    DateNoDay,
    DateNoDayRev,
    DateTextual,
    DateNoYear,
    DateNoYearRev,
    DateNoColon,
    DateShortWithTimeShort,
    DateShortWithTimeLong,
    DateShortWithTimeLongTz,
    DateShortWithTimeShort12,
    DateShortWithTimeLong12,
    PgYearDotDay,
    PgTextShort,
    PgTextReverse,
    Clf,
    Year4,
    IsoWeekDay,
    IsoWeek,
    Exif,
    Timestamp,
    TimestampMs,
    RelativeNumber,
    RelativeText,
    RelativeWeek,
    Ago,
    DayName,
    MonthAlone,
    TzCorrection,
    Tz,
}

/// A pattern over the bytes of the text.
enum Pat {
    /// Between `min` and `max` digits whose value is in `lo..=hi`.
    Num(usize, usize, u64, u64),
    /// A literal, ignoring ASCII case.
    Lit(&'static str),
    /// A literal with its case.
    Exact(&'static str),
    /// One of these words, ignoring ASCII case.
    Words(Vec<&'static str>),
    /// Between `min` and `max` bytes of a class.
    Class(fn(u8) -> bool, usize, usize),
    Opt(Box<Pat>),
    /// One or more of a pattern.
    Plus(Box<Pat>),
    Seq(Vec<Pat>),
    Alt(Vec<Pat>),
    /// `am`/`pm` with optional dots, then the end, a space or a tab (taken with it).
    Meridian,
    /// The end of the text.
    End,
}

const MANY: usize = usize::MAX;

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}
fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}
fn is_upper(c: u8) -> bool {
    c.is_ascii_uppercase()
}
fn is_lower(c: u8) -> bool {
    c.is_ascii_lowercase()
}
fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}
fn is_sign(c: u8) -> bool {
    c == b'+' || c == b'-'
}
fn is_t(c: u8) -> bool {
    c == b't' || c == b'T'
}
fn is_time_sep(c: u8) -> bool {
    c == b':' || c == b'.'
}
fn is_date_gap(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'.' | b'-')
}
fn is_textual_gap(c: u8) -> bool {
    matches!(c, b',' | b'.' | b's' | b't' | b'n' | b'd' | b'r' | b'h' | b'\t' | b' ')
}
fn is_zone_joint(c: u8) -> bool {
    matches!(c, b'_' | b'/' | b'-')
}
fn is_dot_tab_dash(c: u8) -> bool {
    matches!(c, b'.' | b'\t' | b'-')
}
fn is_dot_dash(c: u8) -> bool {
    c == b'.' || c == b'-'
}
fn is_dot_tab(c: u8) -> bool {
    c == b'.' || c == b'\t'
}
fn is_week_day(c: u8) -> bool {
    (b'0'..=b'7').contains(&c)
}

/// Every end of `p` matched at `i`, added to `out`.
fn ends(p: &Pat, b: &[u8], i: usize, out: &mut Vec<usize>) {
    match p {
        Pat::Num(min, max, lo, hi) => {
            let mut v: u64 = 0;
            for n in 1..=*max {
                let Some(c) = b.get(i + n - 1).copied().filter(u8::is_ascii_digit) else { break };
                v = v * 10 + u64::from(c - b'0');
                if n >= *min && (*lo..=*hi).contains(&v) {
                    out.push(i + n);
                }
            }
        }
        Pat::Lit(s) => {
            let n = s.len();
            if b.len() >= i + n && b[i..i + n].eq_ignore_ascii_case(s.as_bytes()) {
                out.push(i + n);
            }
        }
        Pat::Exact(s) => {
            if b[i..].starts_with(s.as_bytes()) {
                out.push(i + s.len());
            }
        }
        Pat::Words(ws) => {
            for w in ws {
                ends(&Pat::Lit(w), b, i, out);
            }
        }
        Pat::Class(f, min, max) => {
            let mut n = 0;
            loop {
                if n >= *min {
                    out.push(i + n);
                }
                if n == *max || !b.get(i + n).is_some_and(|c| f(*c)) {
                    break;
                }
                n += 1;
            }
        }
        Pat::Opt(q) => {
            out.push(i);
            ends(q, b, i, out);
        }
        Pat::Plus(q) => {
            let mut frontier = vec![i];
            while !frontier.is_empty() {
                let mut next = Vec::new();
                for &at in &frontier {
                    ends(q, b, at, &mut next);
                }
                next.retain(|e| !out.contains(e) && !frontier.contains(e));
                next.sort_unstable();
                next.dedup();
                out.extend(&next);
                frontier = next;
            }
        }
        Pat::Seq(ps) => {
            let mut frontier = vec![i];
            for q in ps {
                let mut next = Vec::new();
                for &at in &frontier {
                    ends(q, b, at, &mut next);
                }
                next.sort_unstable();
                next.dedup();
                frontier = next;
                if frontier.is_empty() {
                    return;
                }
            }
            out.extend(frontier);
        }
        Pat::Alt(ps) => {
            for q in ps {
                ends(q, b, i, out);
            }
        }
        Pat::Meridian => meridian(b, i, out),
        Pat::End => {
            if i == b.len() {
                out.push(i);
            }
        }
    }
}

/// `[AaPp] .? [Mm] .?` and then the end of the text or a space or tab, which is part of it.
fn meridian(b: &[u8], i: usize, out: &mut Vec<usize>) {
    if !b.get(i).is_some_and(|c| matches!(c, b'a' | b'A' | b'p' | b'P')) {
        return;
    }
    let mut j = i + 1;
    if b.get(j) == Some(&b'.') {
        j += 1;
    }
    if !b.get(j).is_some_and(|c| *c == b'm' || *c == b'M') {
        return;
    }
    j += 1;
    for k in [j, j + 1] {
        if k == j + 1 && b.get(j) != Some(&b'.') {
            continue;
        }
        match b.get(k) {
            None => out.push(k),
            Some(c) if is_space(*c) => out.push(k + 1),
            _ => {}
        }
    }
}

fn seq(ps: Vec<Pat>) -> Pat {
    Pat::Seq(ps)
}
fn opt(p: Pat) -> Pat {
    Pat::Opt(Box::new(p))
}
fn class(f: fn(u8) -> bool, min: usize, max: usize) -> Pat {
    Pat::Class(f, min, max)
}
fn space() -> Pat {
    class(is_space, 1, MANY)
}
fn space_opt() -> Pat {
    class(is_space, 0, MANY)
}
fn t_opt() -> Pat {
    class(is_t, 0, 1)
}
fn time_sep() -> Pat {
    class(is_time_sep, 1, 1)
}
fn hour24() -> Pat {
    Pat::Num(1, 2, 0, 24)
}
fn hour24lz() -> Pat {
    Pat::Num(2, 2, 0, 24)
}
fn hour12() -> Pat {
    Pat::Num(1, 2, 1, 12)
}
fn minute() -> Pat {
    Pat::Num(1, 2, 0, 59)
}
fn minutelz() -> Pat {
    Pat::Num(2, 2, 0, 59)
}
fn second() -> Pat {
    Pat::Num(1, 2, 0, 60)
}
fn secondlz() -> Pat {
    Pat::Num(2, 2, 0, 60)
}
fn frac() -> Pat {
    seq(vec![Pat::Exact("."), class(is_digit, 1, MANY)])
}
fn month() -> Pat {
    Pat::Num(1, 2, 0, 12)
}
fn monthlz() -> Pat {
    Pat::Num(2, 2, 0, 12)
}
fn day() -> Pat {
    let suffix = Pat::Alt(vec![exact("st"), exact("nd"), exact("rd"), exact("th")]);
    seq(vec![Pat::Num(1, 2, 0, 31), opt(suffix)])
}
fn daylz() -> Pat {
    Pat::Num(2, 2, 0, 31)
}
fn year() -> Pat {
    Pat::Num(1, 4, 0, 9999)
}
fn year2() -> Pat {
    Pat::Num(2, 2, 0, 99)
}
fn year4() -> Pat {
    Pat::Num(4, 4, 0, 9999)
}
fn lit(s: &'static str) -> Pat {
    Pat::Lit(s)
}
fn exact(s: &'static str) -> Pat {
    Pat::Exact(s)
}
fn month_names() -> Pat {
    Pat::Words(MONTH_NAMES.iter().map(|(w, _)| *w).collect())
}
fn month_text() -> Pat {
    let roman = ROMAN_MONTHS.iter().map(|(w, _)| Pat::Exact(w));
    Pat::Alt(std::iter::once(month_names()).chain(roman).collect())
}
fn month_abbr() -> Pat {
    Pat::Words(MONTH_NAMES.iter().filter(|(w, _)| w.len() == 3).map(|(w, _)| *w).collect())
}
fn day_names() -> Pat {
    Pat::Words(DAY_NAMES.iter().map(|(w, _)| *w).collect())
}
fn ordinals() -> Pat {
    Pat::Words(ORDINALS.iter().map(|(w, _, _)| *w).collect())
}
fn meridian_pat() -> Pat {
    Pat::Meridian
}
/// The unit words, with `week` after a number only.
fn units(with_week: bool) -> Pat {
    let mut ws: Vec<&'static str> = UNITS.iter().chain(PLURALS.iter()).map(|(w, _)| *w).collect();
    ws.extend(UNIT_WORDS.iter().map(|(w, _)| *w));
    ws.extend(DAY_NAMES.iter().map(|(w, _)| *w));
    if with_week {
        ws.push("week");
    }
    Pat::Words(ws)
}

/// `[GMT][+-]` and `H`, `H[:]MM`, `HH:MM:SS` or `HHMMSS` (a colon only before digits).
fn tz_correction() -> Pat {
    seq(vec![
        opt(exact("GMT")),
        class(is_sign, 1, 1),
        Pat::Alt(vec![
            seq(vec![hour24(), opt(seq(vec![opt(exact(":")), minute()]))]),
            seq(vec![hour24lz(), exact(":"), minutelz(), exact(":"), secondlz()]),
            seq(vec![hour24lz(), minutelz(), secondlz()]),
        ]),
    ])
}

/// A zone by name: an abbreviation (in parentheses or not) or an identifier (`Europe/Paris`).
fn zone_word() -> Pat {
    Pat::Alt(vec![
        seq(vec![opt(exact("(")), class(is_alpha, 1, 6), opt(exact(")"))]),
        seq(vec![
            class(is_upper, 1, 1),
            class(is_lower, 1, MANY),
            Pat::Plus(Box::new(seq(vec![class(is_zone_joint, 1, 1), class(is_alpha, 1, MANY)]))),
        ]),
    ])
}

/// 12-hour times take two-digit minutes and seconds (`1:2:3am` is a 24-hour time and `am`).
fn time_short12() -> Pat {
    seq(vec![hour12(), time_sep(), minutelz(), space_opt(), meridian_pat()])
}
fn time_long12() -> Pat {
    seq(vec![hour12(), time_sep(), minutelz(), time_sep(), secondlz(), space_opt(), meridian_pat()])
}

fn time_short24() -> Pat {
    seq(vec![t_opt(), hour24(), time_sep(), minute()])
}
fn time_long24() -> Pat {
    seq(vec![t_opt(), hour24(), time_sep(), minute(), time_sep(), second()])
}
fn iso8601_long() -> Pat {
    seq(vec![t_opt(), hour24(), time_sep(), minute(), time_sep(), second(), frac()])
}
/// A month and a day, which end the text or are followed by a gap (`July 3x` is none).
fn date_no_year() -> Pat {
    seq(vec![
        month_text(),
        class(is_date_gap, 0, MANY),
        day(),
        Pat::Alt(vec![class(is_textual_gap, 1, MANY), Pat::End]),
    ])
}

/// The pattern of each rule.
fn pattern(k: Kind) -> Pat {
    match k {
        Kind::Now => lit("now"),
        Kind::Noon => lit("noon"),
        Kind::Midnight => Pat::Words(vec!["midnight", "today"]),
        Kind::Tomorrow => lit("tomorrow"),
        Kind::Yesterday => lit("yesterday"),
        // These phrases take single spaces (`back of\t7` is none).
        Kind::BackOf => seq(vec![
            Pat::Words(vec!["back of ", "front of "]),
            hour24(),
            opt(seq(vec![space_opt(), meridian_pat()])),
        ]),
        Kind::FirstDayOf => Pat::Words(vec!["first day of", "last day of"]),
        Kind::WeekdayOf => seq(vec![ordinals(), space(), day_names(), space(), lit("of")]),
        Kind::TimeTiny12 => seq(vec![hour12(), space_opt(), meridian_pat()]),
        Kind::TimeShort12 => time_short12(),
        Kind::TimeLong12 => time_long12(),
        Kind::MsSqlTime => seq(vec![
            hour12(),
            exact(":"),
            minutelz(),
            exact(":"),
            secondlz(),
            time_sep(),
            class(is_digit, 1, MANY),
            meridian_pat(),
        ]),
        Kind::TimeTiny24 => seq(vec![class(is_t, 1, 1), hour24()]),
        Kind::TimeShort24 => time_short24(),
        Kind::TimeLong24 => time_long24(),
        Kind::Iso8601Long => iso8601_long(),
        Kind::GnuNoColon => seq(vec![t_opt(), hour24lz(), minutelz()]),
        Kind::Iso8601NoColon => seq(vec![t_opt(), hour24lz(), minutelz(), secondlz()]),
        Kind::Americanshort => seq(vec![month(), exact("/"), day()]),
        Kind::American => seq(vec![month(), exact("/"), day(), exact("/"), year()]),
        Kind::Iso8601DateSlash => {
            seq(vec![year4(), exact("/"), monthlz(), exact("/"), daylz(), opt(exact("/"))])
        }
        Kind::DateSlash => seq(vec![year4(), exact("/"), month(), exact("/"), day()]),
        Kind::Iso8601Date4 => {
            seq(vec![class(is_sign, 0, 1), year4(), exact("-"), monthlz(), exact("-"), daylz()])
        }
        Kind::Iso8601DateX => seq(vec![
            class(is_sign, 1, 1),
            class(is_digit, 5, 19),
            exact("-"),
            monthlz(),
            exact("-"),
            daylz(),
        ]),
        Kind::Iso8601Date2 => seq(vec![year2(), exact("-"), monthlz(), exact("-"), daylz()]),
        Kind::GnuDateShorter => seq(vec![year4(), exact("-"), month()]),
        Kind::GnuDateShort => seq(vec![year(), exact("-"), month(), exact("-"), day()]),
        Kind::PointedDate4 => seq(vec![
            day(),
            class(is_dot_tab_dash, 1, 1),
            month(),
            class(is_dot_dash, 1, 1),
            year4(),
        ]),
        Kind::PointedDate2 => {
            seq(vec![day(), class(is_dot_tab, 1, 1), month(), exact("."), year2()])
        }
        Kind::DateFull => seq(vec![
            day(),
            class(is_date_gap, 0, MANY),
            month_text(),
            class(is_date_gap, 0, MANY),
            year(),
        ]),
        Kind::DateNoDay => seq(vec![month_text(), class(is_date_gap, 0, MANY), year4()]),
        Kind::DateNoDayRev => seq(vec![year4(), class(is_date_gap, 0, MANY), month_text()]),
        Kind::DateTextual => seq(vec![
            month_text(),
            class(is_date_gap, 0, MANY),
            day(),
            class(is_textual_gap, 1, MANY),
            year(),
        ]),
        Kind::DateNoYear => date_no_year(),
        Kind::DateNoYearRev => seq(vec![day(), class(is_date_gap, 0, MANY), month_text()]),
        Kind::DateNoColon => seq(vec![year4(), monthlz(), daylz()]),
        Kind::DateShortWithTimeShort => seq(vec![date_no_year(), time_short24()]),
        Kind::DateShortWithTimeLong => seq(vec![date_no_year(), time_long24()]),
        Kind::DateShortWithTimeLongTz => seq(vec![
            date_no_year(),
            t_opt(),
            hour24(),
            time_sep(),
            minute(),
            time_sep(),
            secondlz(),
            space_opt(),
            Pat::Alt(vec![tz_correction(), zone_word()]),
        ]),
        Kind::DateShortWithTimeShort12 => seq(vec![date_no_year(), time_short12()]),
        Kind::DateShortWithTimeLong12 => seq(vec![date_no_year(), time_long12()]),
        Kind::PgYearDotDay => seq(vec![year4(), class(is_dot_dash, 0, 1), Pat::Num(3, 3, 1, 366)]),
        Kind::PgTextShort => seq(vec![month_abbr(), exact("-"), daylz(), exact("-"), year()]),
        Kind::PgTextReverse => seq(vec![year(), exact("-"), month_abbr(), exact("-"), daylz()]),
        Kind::Clf => seq(vec![
            day(),
            exact("/"),
            month_abbr(),
            exact("/"),
            year4(),
            exact(":"),
            hour24lz(),
            exact(":"),
            minutelz(),
            exact(":"),
            secondlz(),
            space(),
            tz_correction(),
        ]),
        Kind::Year4 => year4(),
        Kind::IsoWeekDay => seq(vec![
            year4(),
            opt(exact("-")),
            exact("W"),
            Pat::Num(2, 2, 1, 53),
            opt(exact("-")),
            class(is_week_day, 1, 1),
        ]),
        Kind::IsoWeek => seq(vec![year4(), opt(exact("-")), exact("W"), Pat::Num(2, 2, 1, 53)]),
        Kind::Exif => seq(vec![
            year4(),
            exact(":"),
            monthlz(),
            exact(":"),
            daylz(),
            exact(" "),
            hour24lz(),
            exact(":"),
            minutelz(),
            exact(":"),
            secondlz(),
        ]),
        Kind::Timestamp => seq(vec![exact("@"), opt(exact("-")), class(is_digit, 1, MANY)]),
        Kind::TimestampMs => seq(vec![
            exact("@"),
            opt(exact("-")),
            class(is_digit, 1, MANY),
            exact("."),
            class(is_digit, 0, 6),
        ]),
        Kind::RelativeNumber => seq(vec![
            class(is_sign, 0, MANY),
            space_opt(),
            class(is_digit, 1, MANY),
            space_opt(),
            units(true),
        ]),
        Kind::RelativeText => seq(vec![ordinals(), space(), units(false)]),
        Kind::RelativeWeek => seq(vec![Pat::Words(RELATIVE_TEXT.to_vec()), space(), lit("week")]),
        Kind::Ago => lit("ago"),
        Kind::DayName => {
            let names = DAY_NAMES.iter().map(|(w, _)| *w);
            Pat::Words(names.chain(["weekday", "weekdays"]).collect())
        }
        Kind::MonthAlone => month_names(),
        Kind::TzCorrection => tz_correction(),
        Kind::Tz => zone_word(),
    }
}

/// Every rule, in tie-breaking order.
const KINDS: [Kind; 59] = [
    Kind::Now,
    Kind::Noon,
    Kind::Midnight,
    Kind::Tomorrow,
    Kind::Yesterday,
    Kind::BackOf,
    Kind::FirstDayOf,
    Kind::WeekdayOf,
    Kind::TimeTiny12,
    Kind::TimeShort12,
    Kind::TimeLong12,
    Kind::MsSqlTime,
    Kind::TimeTiny24,
    Kind::TimeShort24,
    Kind::TimeLong24,
    Kind::Iso8601Long,
    Kind::GnuNoColon,
    Kind::Iso8601NoColon,
    Kind::Americanshort,
    Kind::American,
    Kind::Iso8601DateSlash,
    Kind::DateSlash,
    Kind::Iso8601Date4,
    Kind::Iso8601DateX,
    Kind::Iso8601Date2,
    Kind::GnuDateShorter,
    Kind::GnuDateShort,
    Kind::PointedDate4,
    Kind::PointedDate2,
    Kind::DateFull,
    Kind::DateNoDay,
    Kind::DateNoDayRev,
    Kind::DateTextual,
    Kind::DateNoYear,
    Kind::DateNoYearRev,
    Kind::DateNoColon,
    Kind::DateShortWithTimeShort,
    Kind::DateShortWithTimeLong,
    Kind::DateShortWithTimeLongTz,
    Kind::DateShortWithTimeShort12,
    Kind::DateShortWithTimeLong12,
    Kind::PgYearDotDay,
    Kind::PgTextShort,
    Kind::PgTextReverse,
    Kind::Clf,
    Kind::Year4,
    Kind::IsoWeekDay,
    Kind::IsoWeek,
    Kind::Exif,
    Kind::Timestamp,
    Kind::TimestampMs,
    Kind::RelativeNumber,
    Kind::RelativeText,
    Kind::RelativeWeek,
    Kind::Ago,
    Kind::DayName,
    Kind::MonthAlone,
    Kind::TzCorrection,
    Kind::Tz,
];

fn rules() -> &'static [(Kind, Pat)] {
    static RULES: OnceLock<Vec<(Kind, Pat)>> = OnceLock::new();
    RULES.get_or_init(|| KINDS.iter().map(|k| (*k, pattern(*k))).collect())
}

/// The rule and the end of the longest token at `i`, the first rule winning a tie.
pub(super) fn longest(b: &[u8], i: usize) -> Option<(Kind, usize)> {
    let mut best: Option<(Kind, usize)> = None;
    let mut found = Vec::new();
    for (k, p) in rules() {
        found.clear();
        ends(p, b, i, &mut found);
        if let Some(&end) = found.iter().filter(|e| **e > i).max()
            && best.is_none_or(|(_, e)| end > e)
        {
            best = Some((*k, end));
        }
    }
    best
}
