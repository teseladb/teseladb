//! `Date` (ECMA-262 §21.4) with the local time zone fixed at UTC, as the reference server 9.0
//! evaluates it (016 FR-010): `new Date(3600000).toString()` is `Thu Jan 01 1970 01:00:00 GMT+0000
//! (UTC)` on a reference router 9.0.4. `Date.now()` reads the cluster time the runtime gives the
//! invocation.

use crate::builtins::method;
use crate::interp::{Interp, R};
use crate::value::*;

const MS_PER_DAY: f64 = 86_400_000.0;
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn arg(args: &[Val], i: usize) -> Val {
    args.get(i).cloned().unwrap_or(Val::Undef)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian; `m` from 0).
pub fn days_from_civil(y: f64, m: f64, d: f64) -> f64 {
    let y = y + (m / 12.0).floor();
    let m = m.rem_euclid(12.0) + 1.0;
    let (y, m) = (y as i64 - i64::from(m <= 2.0), m as i64);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as f64 + d - 1.0
}

/// Year, month (from 0) and day of the day number `z`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m - 1, d)
}

/// MakeDate from the parts; NaN when one is not finite.
fn make(parts: &[f64]) -> f64 {
    if parts.iter().any(|x| !x.is_finite()) {
        return f64::NAN;
    }
    let get = |i: usize, d: f64| parts.get(i).copied().unwrap_or(d).trunc();
    let days = days_from_civil(get(0, 1970.0), get(1, 0.0), get(2, 1.0));
    let ms =
        get(3, 0.0) * 3_600_000.0 + get(4, 0.0) * 60_000.0 + get(5, 0.0) * 1000.0 + get(6, 0.0);
    time_clip(days * MS_PER_DAY + ms)
}

fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8.64e15 { f64::NAN } else { t.trunc() + 0.0 }
}

/// The fields of time `t`: year, month, day, hours, minutes, seconds, milliseconds, weekday.
fn fields(t: f64) -> (i64, u32, u32, u32, u32, u32, u32, usize) {
    let day = (t / MS_PER_DAY).floor();
    let ms_in_day = (t - day * MS_PER_DAY) as u64;
    let (y, m, d) = civil_from_days(day as i64);
    let wd = (day as i64 + 4).rem_euclid(7) as usize;
    (
        y,
        m,
        d,
        (ms_in_day / 3_600_000) as u32,
        (ms_in_day / 60_000 % 60) as u32,
        (ms_in_day / 1000 % 60) as u32,
        (ms_in_day % 1000) as u32,
        wd,
    )
}

fn year_text(y: i64) -> String {
    if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    }
}

pub fn iso(t: f64) -> String {
    let (y, m, d, h, mi, s, ms, _) = fields(t);
    format!("{}-{:02}-{:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z", year_text(y), m + 1, d)
}

fn to_string_utc(t: f64) -> String {
    let (y, m, d, h, mi, s, _, wd) = fields(t);
    format!(
        "{} {} {d:02} {} {h:02}:{mi:02}:{s:02} GMT+0000 (UTC)",
        DAYS[wd],
        MONTHS[m as usize],
        year_text(y)
    )
}

/// `Date.parse` of the ISO forms and the forms `toString` and `toUTCString` write.
pub fn parse(s: &str) -> f64 {
    let s = s.trim();
    if let Some(t) = parse_iso(s) {
        return t;
    }
    parse_text(s).unwrap_or(f64::NAN)
}

fn parse_iso(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<f64> {
        let t = s.get(from..from + len)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse::<f64>().ok()
    };
    let (year, mut i) = match b.first()? {
        b'+' | b'-' => {
            let y = num(1, 6)?;
            (if b[0] == b'-' { -y } else { y }, 7)
        }
        _ => (num(0, 4)?, 4),
    };
    let mut parts = vec![year, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
    if b.get(i) == Some(&b'-') {
        parts[1] = num(i + 1, 2)? - 1.0;
        i += 3;
        if b.get(i) == Some(&b'-') {
            parts[2] = num(i + 1, 2)?;
            i += 3;
        }
    }
    let mut date_only = true;
    if matches!(b.get(i), Some(b'T' | b't' | b' ')) {
        date_only = false;
        parts[3] = num(i + 1, 2)?;
        if b.get(i + 3) != Some(&b':') {
            return None;
        }
        parts[4] = num(i + 4, 2)?;
        i += 6;
        if b.get(i) == Some(&b':') {
            parts[5] = num(i + 1, 2)?;
            i += 3;
            if b.get(i) == Some(&b'.') {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                let frac = s.get(start..j)?;
                let ms: String = frac.chars().chain("000".chars()).take(3).collect();
                parts[6] = ms.parse().ok()?;
                i = j;
            }
        }
    }
    let mut offset = 0.0;
    match b.get(i) {
        None => {}
        Some(b'Z') if i + 1 == b.len() => {}
        Some(c @ (b'+' | b'-')) => {
            let h = num(i + 1, 2)?;
            let m = if b.get(i + 3) == Some(&b':') { num(i + 4, 2)? } else { num(i + 3, 2)? };
            // Measured on a reference router 9.0.4 on 02-11: `+24:00` is no offset.
            if h > 23.0 || m > 59.0 {
                return None;
            }
            offset = (h * 60.0 + m) * 60_000.0 * if *c == b'+' { 1.0 } else { -1.0 };
        }
        _ => return None,
    }
    let _ = date_only;
    if parts[1] > 11.0
        || parts[2] < 1.0
        || parts[2] > 31.0
        || parts[3] > 24.0
        || parts[3] == 24.0 && parts[4..].iter().any(|x| *x != 0.0)
        || parts[4] > 59.0
        || parts[5] > 59.0
    {
        // Out of range: the legacy parser reads it (measured: `2020-02-00` is January 31).
        return None;
    }
    Some(time_clip(make(&parts) - offset))
}

/// The zones a date's text may name, with their offset in hours.
const ZONES: [(&str, f64); 11] = [
    ("UTC", 0.0),
    ("UT", 0.0),
    ("GMT", 0.0),
    ("EST", -5.0),
    ("EDT", -4.0),
    ("CST", -6.0),
    ("CDT", -5.0),
    ("MST", -7.0),
    ("MDT", -6.0),
    ("PST", -8.0),
    ("PDT", -7.0),
];

/// `s` without its comments: a `(` that starts a word opens one, which ends at its matching `)`
/// or at the end (measured on a reference router 9.0.4 on 03-11: `Jan 2 2020 (a (b) c) 10:00` is a
/// date, `Jan(a)2 2020` is not).
fn without_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut prev = ' ';
    for c in s.chars() {
        if depth > 0 {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            // The comment is a space between the words around it.
            prev = ' ';
            if depth == 0 {
                out.push(' ');
            }
            continue;
        }
        if c == '(' && matches!(prev, ' ' | ',') {
            depth = 1;
            continue;
        }
        out.push(c);
        prev = c;
    }
    out
}

/// `Thu Jan 01 1970 01:00:00 GMT+0000`, `Thu, 01 Jan 1970 01:00:00 GMT`, `Jan 1, 1970` and the
/// like: words for the month, numbers for the rest.
fn parse_text(s: &str) -> Option<f64> {
    let mut month = None;
    let mut nums: Vec<f64> = Vec::new();
    let mut time: Vec<f64> = Vec::new();
    let mut offset = 0.0;
    let mut pm = None;
    // `2020-02-00` is the day before February 1st; `Feb 0 2020` is no date (measured).
    let mut numeric = false;
    let s = without_comments(s);
    for tok in s.split([' ', ',']).filter(|t| !t.is_empty()) {
        // Measured on a reference router 9.0.4 on 02-11: `1/2/2020`, `2020/1/2` and `2020-1-1` are
        // dates (month first unless the year is), `AM`/`PM` move the hour, and the zones of North
        // America have their offsets.
        // `2020.01.02` too, when the token is no time.
        let parts: Vec<&str> =
            if tok.contains(':') { vec![tok] } else { tok.split(['/', '-', '.']).collect() };
        if parts.len() == 3
            && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
        {
            let v: Vec<f64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
            let (y, m, d) = if v[0] > 31.0 { (v[0], v[1], v[2]) } else { (v[2], v[0], v[1]) };
            if !(1.0..=12.0).contains(&m) {
                return None;
            }
            month = Some(m - 1.0);
            nums = vec![d, y];
            numeric = true;
            continue;
        }
        // `1//2`, `/2020/`, `1..2`: numbers between slashes, dashes and dots (measured on 03-11).
        if tok.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'/' | b'-' | b'.'))
            && (tok.contains('/') || tok[1..].contains('-') || tok.contains(".."))
        {
            for p in tok.split(['/', '-', '.']).filter(|p| !p.is_empty()) {
                nums.push(p.parse().ok()?);
            }
            continue;
        }
        if let Some(h) = ZONES.iter().find(|(z, _)| tok.eq_ignore_ascii_case(z)).map(|(_, h)| *h) {
            offset = h * 3_600_000.0;
            continue;
        }
        if tok.eq_ignore_ascii_case("AM") || tok.eq_ignore_ascii_case("PM") {
            pm = Some(tok.eq_ignore_ascii_case("PM"));
            continue;
        }
        if let Some(m) =
            MONTHS.iter().position(|m| tok.len() >= 3 && tok[..3].eq_ignore_ascii_case(m))
        {
            month = Some(m as f64);
        } else if tok.contains(':') {
            for p in tok.split(':') {
                time.push(p.parse().ok()?);
            }
        } else if let Some(rest) = tok.strip_prefix("GMT").or_else(|| tok.strip_prefix("UTC")) {
            if let Some(sign @ ('+' | '-')) = rest.chars().next() {
                let v: f64 = rest[1..].parse().ok()?;
                // `GMT-5` is hours, `GMT+0130` hours and minutes (measured).
                let (h, m) =
                    if rest.len() <= 3 { (v, 0.0) } else { ((v / 100.0).trunc(), v % 100.0) };
                offset = (h * 60.0 + m) * 60_000.0 * if sign == '+' { 1.0 } else { -1.0 };
            }
        } else if DAYS.iter().any(|d| tok.len() >= 3 && tok[..3].eq_ignore_ascii_case(d)) {
        } else if let Ok(n) = tok.parse::<f64>() {
            nums.push(n);
        } else if !tok.eq_ignore_ascii_case("Z") {
            return None;
        }
    }
    let (month, day, year) = match (month, nums.as_slice()) {
        (Some(m), [a, b]) if *a > 31.0 => (m, *b, *a),
        (Some(m), [a, b]) => (m, *a, *b),
        // Three numbers are the year first when it cannot be a month or a day, else month, day
        // and year; one number is a month of 2001, or a year when it cannot be a day (measured
        // on a reference router 9.0.4 on 03-11: `5 1 2` is May 1 2002 and `5` May 1 2001).
        (None, [y, m, d]) if *y > 31.0 => (*m - 1.0, *d, *y),
        (None, [m, d, y]) => (*m - 1.0, *d, *y),
        (None, [m]) if (1.0..=12.0).contains(m) => (*m - 1.0, 1.0, 2001.0),
        (None, [y]) if !(1.0..=31.0).contains(y) => (0.0, 1.0, *y),
        _ => return None,
    };
    if !(0.0..12.0).contains(&month) {
        return None;
    }
    if day > 31.0 || (day < 1.0 && !numeric) {
        return None;
    }
    let get = |i: usize| time.get(i).copied().unwrap_or(0.0);
    // `13:00 PM` is no date; a year under 100 is 2000 to 2049 or 1950 to 1999 (measured).
    if pm.is_some() && get(0) > 12.0 {
        return None;
    }
    let year = if (0.0..100.0).contains(&year) {
        year + if year < 50.0 { 2000.0 } else { 1900.0 }
    } else {
        year
    };
    let hour = match pm {
        Some(true) if get(0) < 12.0 => get(0) + 12.0,
        Some(false) if get(0) == 12.0 => 0.0,
        _ => get(0),
    };
    Some(make(&[year, month, day, hour, get(1), get(2), 0.0]) - offset)
}

/// `Date.prototype.getTime called on incompatible Object`, with the method being called.
fn incompatible_date(it: &Interp, this: &Val) -> crate::interp::Ctrl {
    let class = crate::builtins::class_name(this);
    it.error("TypeError", &format!("Date.prototype.{} called on incompatible {class}", it.native))
}

fn this_time(it: &Interp, this: &Val) -> R<f64> {
    match this {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Date(t) => Ok(*t),
            _ => Err(incompatible_date(it, this)),
        },
        _ => Err(incompatible_date(it, this)),
    }
}

pub fn make_date(it: &Interp, t: f64) -> R {
    Ok(Val::Obj(it.alloc(Some(it.realm.date.clone()), Kind::Date(time_clip(t)))?))
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.date.clone();
    let c = it.native("Date", date_ctor, 7)?;
    it.define(&c, "prototype", Prop { writable: false, ..Prop::hidden(Val::Obj(proto.clone())) })?;
    it.define(&proto, "constructor", Prop::hidden(Val::Obj(c.clone())))?;
    it.define(g, "Date", Prop::hidden(Val::Obj(c.clone())))?;
    method(it, &c, "now", |it, _, _| Ok(Val::Num(it.now_ms)), 0)?;
    method(
        it,
        &c,
        "UTC",
        |it, _, a| {
            if a.is_empty() {
                return Ok(Val::Num(f64::NAN));
            }
            let mut parts = Vec::new();
            for x in a.iter().take(7) {
                parts.push(it.number_of(x)?);
            }
            if parts.len() == 1 {
                parts.push(0.0);
            }
            // A year from 0 to 99 is 1900 to 1999, as in the constructor.
            if parts[0].is_finite() && (0.0..=99.0).contains(&parts[0].trunc()) {
                parts[0] = 1900.0 + parts[0].trunc();
            }
            Ok(Val::Num(make(&parts)))
        },
        7,
    )?;
    method(
        it,
        &c,
        "parse",
        |it, _, a| {
            let s = it.str_of(&arg(a, 0))?;
            Ok(Val::Num(parse(&s)))
        },
        1,
    )?;
    type Field = fn((i64, u32, u32, u32, u32, u32, u32, usize)) -> f64;
    let getters: [(&str, Field); 8] = [
        ("FullYear", |f| f.0 as f64),
        ("Month", |f| f64::from(f.1)),
        ("Date", |f| f64::from(f.2)),
        ("Hours", |f| f64::from(f.3)),
        ("Minutes", |f| f64::from(f.4)),
        ("Seconds", |f| f64::from(f.5)),
        ("Milliseconds", |f| f64::from(f.6)),
        ("Day", |f| f.7 as f64),
    ];
    for (i, (name, _)) in getters.iter().enumerate() {
        // Local time is UTC: `getHours` and `getUTCHours` are the same.
        let f: NativeFn = match i {
            0 => |it, t, _| get_field(it, t, 0),
            1 => |it, t, _| get_field(it, t, 1),
            2 => |it, t, _| get_field(it, t, 2),
            3 => |it, t, _| get_field(it, t, 3),
            4 => |it, t, _| get_field(it, t, 4),
            5 => |it, t, _| get_field(it, t, 5),
            6 => |it, t, _| get_field(it, t, 6),
            _ => |it, t, _| get_field(it, t, 7),
        };
        method(it, &proto, &format!("get{name}"), f, 0)?;
        method(it, &proto, &format!("getUTC{name}"), f, 0)?;
    }
    let setters: [(&str, NativeFn, usize); 7] = [
        ("FullYear", |it, t, a| set_fields(it, t, a, 0), 3),
        ("Month", |it, t, a| set_fields(it, t, a, 1), 2),
        ("Date", |it, t, a| set_fields(it, t, a, 2), 1),
        ("Hours", |it, t, a| set_fields(it, t, a, 3), 4),
        ("Minutes", |it, t, a| set_fields(it, t, a, 4), 3),
        ("Seconds", |it, t, a| set_fields(it, t, a, 5), 2),
        ("Milliseconds", |it, t, a| set_fields(it, t, a, 6), 1),
    ];
    for (name, f, n) in setters {
        method(it, &proto, &format!("set{name}"), f, n)?;
        method(it, &proto, &format!("setUTC{name}"), f, n)?;
    }
    for (name, f, n) in [
        ("getTime", (|it, t, _| Ok(Val::Num(this_time(it, t)?))) as NativeFn, 0),
        ("valueOf", |it, t, _| Ok(Val::Num(this_time(it, t)?)), 0),
        (
            "getTimezoneOffset",
            |it, t, _| {
                let x = this_time(it, t)?;
                Ok(Val::Num(if x.is_nan() { f64::NAN } else { 0.0 }))
            },
            0,
        ),
        (
            "setTime",
            |it, t, a| {
                let n = it.number_of(&arg(a, 0))?;
                set_time(it, t, time_clip(n))
            },
            1,
        ),
        (
            "toISOString",
            |it, t, _| {
                let x = this_time(it, t)?;
                if x.is_nan() {
                    return Err(it.error("RangeError", "invalid date"));
                }
                it.string(iso(x))
            },
            0,
        ),
        (
            "toJSON",
            |it, t, _| {
                let x = this_time(it, t)?;
                if x.is_nan() { Ok(Val::Null) } else { it.string(iso(x)) }
            },
            1,
        ),
        ("toString", date_to_string, 0),
        (
            "toUTCString",
            |it, t, _| {
                let x = this_time(it, t)?;
                if x.is_nan() {
                    return Ok(Val::str("Invalid Date"));
                }
                let (y, m, d, h, mi, s, _, wd) = fields(x);
                it.string(format!(
                    "{}, {d:02} {} {} {h:02}:{mi:02}:{s:02} GMT",
                    DAYS[wd],
                    MONTHS[m as usize],
                    year_text(y)
                ))
            },
            0,
        ),
        (
            "toDateString",
            |it, t, _| {
                let x = this_time(it, t)?;
                if x.is_nan() {
                    return Ok(Val::str("Invalid Date"));
                }
                let (y, m, d, _, _, _, _, wd) = fields(x);
                it.string(format!("{} {} {d:02} {}", DAYS[wd], MONTHS[m as usize], year_text(y)))
            },
            0,
        ),
        ("toLocaleString", |it, t, _| local_text(it, t, LocalPart::Both), 0),
        ("toLocaleDateString", |it, t, _| local_text(it, t, LocalPart::Date), 0),
        ("toLocaleTimeString", |it, t, _| local_text(it, t, LocalPart::Time), 0),
        ("toTimeString", to_time_string, 0),
        ("getYear", get_year, 0),
        ("setYear", set_year, 1),
    ] {
        method(it, &proto, name, f, n)?;
    }
    // `toGMTString` is the same function as `toUTCString`.
    let utc = proto.borrow().get_own("toUTCString").map(|p| p.value.clone()).unwrap_or(Val::Undef);
    it.define(&proto, "toGMTString", Prop::hidden(utc))?;
    Ok(())
}

/// What the locale methods write: the engine of the reference router 9.0.4 has no ICU and uses the
/// C locale (measured on 02-11): `Thu Jan  1 00:00:00 1970`, `01/01/1970`, `00:00:00`, and
/// `InvalidDate` for an invalid date.
enum LocalPart {
    Both,
    Date,
    Time,
}

fn local_text(it: &mut Interp, this: &Val, part: LocalPart) -> R {
    let x = this_time(it, this)?;
    if x.is_nan() {
        return Ok(Val::str("InvalidDate"));
    }
    let (y, m, d, h, mi, s, _, wd) = fields(x);
    it.string(match part {
        LocalPart::Both => {
            format!("{} {} {d:>2} {h:02}:{mi:02}:{s:02} {y}", DAYS[wd], MONTHS[m as usize])
        }
        LocalPart::Date => format!("{:02}/{d:02}/{y}", m + 1),
        LocalPart::Time => format!("{h:02}:{mi:02}:{s:02}"),
    })
}

fn to_time_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let x = this_time(it, this)?;
    if x.is_nan() {
        return Ok(Val::str("Invalid Date"));
    }
    let (_, _, _, h, mi, s, _, _) = fields(x);
    it.string(format!("{h:02}:{mi:02}:{s:02} GMT+0000 (UTC)"))
}

/// `getYear()`: the year minus 1900 (Annex B).
fn get_year(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let x = this_time(it, this)?;
    if x.is_nan() {
        return Ok(Val::Num(f64::NAN));
    }
    Ok(Val::Num(fields(x).0 as f64 - 1900.0))
}

/// `setYear(y)`: a year from 0 to 99 is 1900 to 1999; an invalid date starts from 0 (Annex B).
fn set_year(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let t = this_time(it, this)?;
    let y = it.number_of(&arg(a, 0))?;
    if y.is_nan() {
        return set_time(it, this, f64::NAN);
    }
    let y = y.trunc();
    let y = if (0.0..=99.0).contains(&y) { 1900.0 + y } else { y };
    let t = if t.is_nan() { 0.0 } else { t };
    let f = fields(t);
    let parts = [
        y,
        f64::from(f.1),
        f64::from(f.2),
        f64::from(f.3),
        f64::from(f.4),
        f64::from(f.5),
        f64::from(f.6),
    ];
    let nt = time_clip(make(&parts));
    set_time(it, this, nt)
}

fn date_to_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let x = this_time(it, this)?;
    if x.is_nan() {
        return Ok(Val::str("Invalid Date"));
    }
    it.string(to_string_utc(x))
}

fn get_field(it: &mut Interp, this: &Val, i: usize) -> R {
    let t = this_time(it, this)?;
    if t.is_nan() {
        return Ok(Val::Num(f64::NAN));
    }
    let f = fields(t);
    Ok(Val::Num(match i {
        0 => f.0 as f64,
        1 => f64::from(f.1),
        2 => f64::from(f.2),
        3 => f64::from(f.3),
        4 => f64::from(f.4),
        5 => f64::from(f.5),
        6 => f64::from(f.6),
        _ => f.7 as f64,
    }))
}

fn set_time(it: &mut Interp, this: &Val, t: f64) -> R {
    if let Val::Obj(o) = this
        && let Kind::Date(x) = &mut o.borrow_mut().kind
    {
        *x = t;
        return Ok(Val::Num(t));
    }
    Err(incompatible_date(it, this))
}

/// `setFullYear`, `setMonth`…: the arguments replace the fields from `first` on.
fn set_fields(it: &mut Interp, this: &Val, a: &[Val], first: usize) -> R {
    let t = this_time(it, this)?;
    let t = if t.is_nan() && first == 0 { 0.0 } else { t };
    let f = fields(t);
    let mut parts = [
        f.0 as f64,
        f64::from(f.1),
        f64::from(f.2),
        f64::from(f.3),
        f64::from(f.4),
        f64::from(f.5),
        f64::from(f.6),
    ];
    let limit = match first {
        0 => 3,
        1 => 2,
        2 => 1,
        3 => 4,
        4 => 3,
        5 => 2,
        _ => 1,
    };
    for (k, x) in a.iter().take(limit).enumerate() {
        parts[first + k] = it.number_of(x)?;
    }
    let nt = if t.is_nan() { f64::NAN } else { make(&parts) };
    set_time(it, this, nt)
}

fn date_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let is_new = matches!(this, Val::Obj(o) if o.borrow().get_own("@@new").is_some());
    if !is_new {
        return it.string(to_string_utc(it.now_ms));
    }
    let t = match a.len() {
        0 => it.now_ms,
        1 => {
            let v = match &a[0] {
                Val::Obj(o) => match &o.borrow().kind {
                    Kind::Date(t) => Val::Num(*t),
                    _ => Val::Obj(o.clone()),
                },
                v => v.clone(),
            };
            let p = it.primitive(&v, "default")?;
            match p {
                Val::Str(s) => parse(&s),
                p => it.number_of(&p)?,
            }
        }
        _ => {
            let mut parts = Vec::new();
            for x in a.iter().take(7) {
                parts.push(it.number_of(x)?);
            }
            if (0.0..=99.0).contains(&parts[0].trunc()) {
                parts[0] = 1900.0 + parts[0].trunc();
            }
            make(&parts)
        }
    };
    make_date(it, t)
}
