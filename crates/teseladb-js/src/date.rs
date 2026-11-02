//! `Date` (ECMA-262 §21.4) with the local time zone fixed at UTC, as RefDb 9.0 evaluates it
//! (016 FR-010): `new Date(3600000).toString()` is `Thu Jan 01 1970 01:00:00 GMT+0000 (UTC)`
//! on a `refrouter` 9.0.4. `Date.now()` reads the cluster time the runtime gives the invocation.

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
        Some(b'Z' | b'z') if i + 1 == b.len() => {}
        Some(c @ (b'+' | b'-')) => {
            let h = num(i + 1, 2)?;
            let m = if b.get(i + 3) == Some(&b':') { num(i + 4, 2)? } else { num(i + 3, 2)? };
            offset = (h * 60.0 + m) * 60_000.0 * if *c == b'+' { 1.0 } else { -1.0 };
        }
        _ => return None,
    }
    let _ = date_only;
    if parts[1] > 11.0
        || parts[2] < 1.0
        || parts[2] > 31.0
        || parts[3] > 24.0
        || parts[4] > 59.0
        || parts[5] > 59.0
    {
        return Some(f64::NAN);
    }
    Some(time_clip(make(&parts) - offset))
}

/// `Thu Jan 01 1970 01:00:00 GMT+0000`, `Thu, 01 Jan 1970 01:00:00 GMT`, `Jan 1, 1970` and the
/// like: words for the month, numbers for the rest.
fn parse_text(s: &str) -> Option<f64> {
    let mut month = None;
    let mut nums: Vec<f64> = Vec::new();
    let mut time: Vec<f64> = Vec::new();
    let mut offset = 0.0;
    for tok in s.split([' ', ',']).filter(|t| !t.is_empty()) {
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
                let (h, m) = ((v / 100.0).trunc(), v % 100.0);
                offset = (h * 60.0 + m) * 60_000.0 * if sign == '+' { 1.0 } else { -1.0 };
            }
        } else if tok.starts_with('(')
            || DAYS.iter().any(|d| tok.len() >= 3 && tok[..3].eq_ignore_ascii_case(d))
        {
        } else if let Ok(n) = tok.parse::<f64>() {
            nums.push(n);
        } else if tok.starts_with('(') || tok.ends_with(')') || tok == "Z" {
        } else {
            return None;
        }
    }
    let month = month?;
    let (day, year) = match nums.as_slice() {
        [a, b] if *a > 31.0 => (*b, *a),
        [a, b] => (*a, *b),
        _ => return None,
    };
    let get = |i: usize| time.get(i).copied().unwrap_or(0.0);
    Some(make(&[year, month, day, get(0), get(1), get(2), 0.0]) - offset)
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
            let mut parts = Vec::new();
            for x in a.iter().take(7) {
                parts.push(it.number_of(x)?);
            }
            if parts.len() == 1 {
                parts.push(0.0);
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
        ("toLocaleString", date_to_string, 0),
    ] {
        method(it, &proto, name, f, n)?;
    }
    Ok(())
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
