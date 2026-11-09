//! Date expressions (measured on a `refrouter` 9.0.4): the field operators (`$year` … `$isoWeekYear`),
//! `$dateToString`, `$dateToParts` and `$dateFromParts`; `dates_arith` has `$dateAdd`,
//! `$dateSubtract`, `$dateDiff` and `$dateTrunc`, and `date_parse` `$dateFromString`.
//!
//! A date argument may also be a timestamp or an ObjectId (their seconds); `null` or a missing
//! date or `timezone` gives `null`.

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::arith::type_name_of;
use super::calendar::{MS_SECOND, Parts, Tz, days_from_civil, iso_week, iso_week_one, weekday};
use super::values::{display, display_bson, int64_of};
use super::{Ctx, Eval, Expr, Scope, arguments, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const NOT_A_DATE: ErrorCode = code!(16006);
const ZONE_STRING: ErrorCode = code!(40517);
const FIELD_OPTION: ErrorCode = code!(40535);
const FIELD_ARRAY: ErrorCode = code!(40536);
const FIELD_DATE: ErrorCode = code!(40539);
const TO_STRING_FORMAT: ErrorCode = code!(18533);
const TO_STRING_UNKNOWN: ErrorCode = code!(18534);
const FORMAT_PERCENT: ErrorCode = code!(18535);
const FORMAT_CHAR: ErrorCode = code!(18536);
const DATE_RANGE: ErrorCode = code!(18537);
const TO_STRING_DATE: ErrorCode = code!(18628);
const TO_STRING_OBJECT: ErrorCode = code!(18629);
const TO_PARTS_UNKNOWN: ErrorCode = code!(40520);
const TO_PARTS_ISO: ErrorCode = code!(40521);
const TO_PARTS_DATE: ErrorCode = code!(40522);
const TO_PARTS_OBJECT: ErrorCode = code!(40524);
const FROM_PARTS_MIX: ErrorCode = code!(40489);
const FROM_PARTS_INTEGER: ErrorCode = code!(40515);
const FROM_PARTS_YEAR: ErrorCode = code!(40516);
const FROM_PARTS_UNKNOWN: ErrorCode = code!(40518);
const FROM_PARTS_OBJECT: ErrorCode = code!(40519);
const FROM_PARTS_YEAR_RANGE: ErrorCode = code!(40523);
const FROM_PARTS_RANGE: ErrorCode = code!(31034);

/// The months' names.
pub(super) const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// A date field operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Year,
    Month,
    DayOfMonth,
    Hour,
    Minute,
    Second,
    Millisecond,
    DayOfYear,
    DayOfWeek,
    IsoDayOfWeek,
    Week,
    IsoWeek,
    IsoWeekYear,
}

const FIELDS: [(&str, Field); 13] = [
    ("$year", Field::Year),
    ("$month", Field::Month),
    ("$dayOfMonth", Field::DayOfMonth),
    ("$hour", Field::Hour),
    ("$minute", Field::Minute),
    ("$second", Field::Second),
    ("$millisecond", Field::Millisecond),
    ("$dayOfYear", Field::DayOfYear),
    ("$dayOfWeek", Field::DayOfWeek),
    ("$isoDayOfWeek", Field::IsoDayOfWeek),
    ("$week", Field::Week),
    ("$isoWeek", Field::IsoWeek),
    ("$isoWeekYear", Field::IsoWeekYear),
];

/// The parts `$dateFromParts` reads, in RefDb's order.
const NATURAL: [&str; 7] = ["year", "month", "day", "hour", "minute", "second", "millisecond"];
const ISO: [&str; 3] = ["isoWeekYear", "isoWeek", "isoDayOfWeek"];

/// A date expression.
#[derive(Clone, Debug, PartialEq)]
pub enum DateExpr {
    Field(Field, Expr, Option<Expr>),
    ToString { date: Expr, format: Option<Expr>, timezone: Option<Expr>, on_null: Option<Expr> },
    ToParts { date: Expr, timezone: Option<Expr>, iso: Option<Expr> },
    FromParts { parts: Vec<(&'static str, Expr)>, iso: bool, timezone: Option<Expr> },
}

/// Parses a date expression of this module; `None` for any other operator.
pub(super) fn parse(
    name: &str,
    arg: RawValue<'_>,
    ctx: &Ctx,
) -> Result<Option<DateExpr>, MqlError> {
    if let Some((_, f)) = FIELDS.iter().find(|(n, _)| *n == name) {
        return parse_field(name, *f, arg, ctx).map(Some);
    }
    Ok(Some(match name {
        "$dateToString" => parse_to_string(arg, ctx)?,
        "$dateToParts" => parse_to_parts(arg, ctx)?,
        "$dateFromParts" => parse_from_parts(arg, ctx)?,
        _ => return Ok(None),
    }))
}

fn parse_field(name: &str, f: Field, arg: RawValue<'_>, ctx: &Ctx) -> Result<DateExpr, MqlError> {
    // An object is an expression when its first field is an operator; otherwise, an empty one
    // included, it names the date and the time zone (RefDb r9.0.4,
    // `parseDateExpressionAcceptingTimeZone`).
    if let RawValue::Document(d) = arg
        && !d.iter().next().is_some_and(|(k, _)| k.starts_with('$'))
    {
        let (mut date, mut timezone) = (None, None);
        for (k, v) in d.iter() {
            match k {
                "date" => date = Some(Expr::parse_in(v, ctx)?),
                "timezone" => timezone = Some(Expr::parse_in(v, ctx)?),
                other => {
                    return Err(error(
                        FIELD_OPTION,
                        format!("unrecognized option to {name}: \"{other}\""),
                    ));
                }
            }
        }
        let date = date.ok_or_else(|| {
            error(
                FIELD_DATE,
                format!(
                    "missing 'date' argument to {name}, provided: {name}: {}",
                    display_bson(&Value::from(arg))
                ),
            )
        })?;
        return Ok(DateExpr::Field(f, date, timezone));
    }
    let args = arguments(arg);
    if let RawValue::Array(_) = arg
        && args.len() != 1
    {
        return Err(error(
            FIELD_ARRAY,
            format!(
                "{name} accepts exactly one argument if given an array, but was given {}",
                args.len()
            ),
        ));
    }
    let date = args.into_iter().next().map(|x| Expr::parse_in(x, ctx)).transpose()?;
    Ok(DateExpr::Field(f, date.unwrap_or(Expr::Literal(Value::Null)), None))
}

/// The named arguments of `arg`, or `bad` when it is not a document.
fn named(arg: RawValue<'_>, bad: MqlError) -> Result<teseladb_bson::RawDoc<'_>, MqlError> {
    match arg {
        RawValue::Document(d) => Ok(d),
        _ => Err(bad),
    }
}

fn parse_to_string(arg: RawValue<'_>, ctx: &Ctx) -> Result<DateExpr, MqlError> {
    let d = named(
        arg,
        error(TO_STRING_OBJECT, "$dateToString only supports an object as its argument"),
    )?;
    let (mut date, mut format, mut timezone, mut on_null) = (None, None, None, None);
    for (k, v) in d.iter() {
        let e = Some(Expr::parse_in(v, ctx)?);
        match k {
            "date" => date = e,
            "format" => {
                if let RawValue::String(f) = v {
                    check_format(f)?;
                } else if !matches!(v, RawValue::Document(_)) && !matches!(v, RawValue::String(_)) {
                    return Err(error(
                        TO_STRING_FORMAT,
                        format!(
                            "$dateToString requires that 'format' be a string, found: {} with value {}",
                            super::arith::raw_type_name(v),
                            display(&Value::from(v))
                        ),
                    ));
                }
                format = e;
            }
            "timezone" => timezone = e,
            "onNull" => on_null = e,
            other => {
                return Err(error(
                    TO_STRING_UNKNOWN,
                    format!("Unrecognized argument to $dateToString: {other}"),
                ));
            }
        }
    }
    let date =
        date.ok_or_else(|| error(TO_STRING_DATE, "Missing 'date' parameter to $dateToString"))?;
    Ok(DateExpr::ToString { date, format, timezone, on_null })
}

fn parse_to_parts(arg: RawValue<'_>, ctx: &Ctx) -> Result<DateExpr, MqlError> {
    let d =
        named(arg, error(TO_PARTS_OBJECT, "$dateToParts only supports an object as its argument"))?;
    let (mut date, mut timezone, mut iso) = (None, None, None);
    for (k, v) in d.iter() {
        let e = Some(Expr::parse_in(v, ctx)?);
        match k {
            "date" => date = e,
            "timezone" => timezone = e,
            "iso8601" => iso = e,
            other => {
                return Err(error(
                    TO_PARTS_UNKNOWN,
                    format!("Unrecognized argument to $dateToParts: {other}"),
                ));
            }
        }
    }
    let date =
        date.ok_or_else(|| error(TO_PARTS_DATE, "Missing 'date' parameter to $dateToParts"))?;
    Ok(DateExpr::ToParts { date, timezone, iso })
}

fn parse_from_parts(arg: RawValue<'_>, ctx: &Ctx) -> Result<DateExpr, MqlError> {
    let d = named(
        arg,
        error(FROM_PARTS_OBJECT, "$dateFromParts only supports an object as its argument"),
    )?;
    let (mut parts, mut timezone) = (Vec::new(), None);
    for (k, v) in d.iter() {
        let e = Expr::parse_in(v, ctx)?;
        match NATURAL.iter().chain(ISO.iter()).find(|n| **n == k) {
            Some(n) => parts.push((*n, e)),
            None if k == "timezone" => timezone = Some(e),
            None => {
                return Err(error(
                    FROM_PARTS_UNKNOWN,
                    format!("Unrecognized argument to $dateFromParts: {k}"),
                ));
            }
        }
    }
    let has = |names: &[&str]| parts.iter().any(|(n, _)| names.contains(n));
    let natural = has(&NATURAL);
    let iso = has(&ISO);
    if natural && iso {
        return Err(error(
            FROM_PARTS_MIX,
            "$dateFromParts does not allow mixing natural dates with ISO dates",
        ));
    }
    let leading = if iso { "isoWeekYear" } else { "year" };
    if !parts.iter().any(|(n, _)| *n == leading) {
        return Err(error(
            FROM_PARTS_YEAR,
            "$dateFromParts requires either 'year' or 'isoWeekYear' to be present",
        ));
    }
    Ok(DateExpr::FromParts { parts, iso, timezone })
}

/// Checks a format of `$dateToString` or `$dateFromString`.
pub(super) fn check_format(f: &str) -> Result<(), MqlError> {
    let mut chars = f.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        match chars.next() {
            None => return Err(error(FORMAT_PERCENT, "Unmatched '%' at end of format string")),
            Some(s) if "dGHjLmMSuVwYUzZbB%".contains(s) => {}
            Some(s) => {
                return Err(error(
                    FORMAT_CHAR,
                    format!("Invalid format character '%{s}' in format string"),
                ));
            }
        }
    }
    Ok(())
}

/// The milliseconds of a date argument, `None` for `null` or missing; `bad` names the error of
/// another type.
pub(super) fn date_ms(
    v: &Eval,
    bad: impl FnOnce(&Value) -> MqlError,
) -> Result<Option<i64>, MqlError> {
    match v {
        None | Some(Value::Null | Value::Undefined) => Ok(None),
        Some(Value::DateTime(ms)) => Ok(Some(*ms)),
        Some(Value::Timestamp { time, .. }) => Ok(Some(i64::from(*time) * MS_SECOND)),
        Some(Value::ObjectId(id)) => {
            Ok(Some(i64::from(u32::from_be_bytes([id[0], id[1], id[2], id[3]])) * MS_SECOND))
        }
        Some(other) => Err(bad(other)),
    }
}

/// The error of a value that is no date.
pub(super) fn not_a_date(v: &Value) -> MqlError {
    error(NOT_A_DATE, format!("can't convert from BSON type {} to Date", type_name_of(v)))
}

/// The `timezone` argument: `Some(None)` for `null` or missing (the result is `null`).
pub(super) fn zone(
    e: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Option<Tz>, MqlError> {
    let Some(e) = e else { return Ok(Some(Tz::Utc)) };
    match e.eval(doc, scope)? {
        None | Some(Value::Null | Value::Undefined) => Ok(None),
        Some(Value::String(s)) => Tz::parse(&s).map(Some),
        Some(other) => Err(error(
            ZONE_STRING,
            format!("timezone must evaluate to a string, found {}", type_name_of(&other)),
        )),
    }
}

fn int(n: i64) -> Value {
    i32::try_from(n).map_or(Value::Int64(n), Value::Int32)
}

impl DateExpr {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            DateExpr::Field(f, date, timezone) => {
                let Some(ms) = date_ms(&date.eval(doc, scope)?, not_a_date)? else {
                    return Ok(Some(Value::Null));
                };
                let Some(tz) = zone(timezone.as_ref(), doc, scope)? else {
                    return Ok(Some(Value::Null));
                };
                Ok(Some(field(*f, &Parts::of(tz.local(ms)))))
            }
            DateExpr::ToString { date, format, timezone, on_null } => {
                let Some(tz) = zone(timezone.as_ref(), doc, scope)? else {
                    return Ok(Some(Value::Null));
                };
                let format = match format {
                    None => None,
                    Some(e) => match e.eval(doc, scope)? {
                        None | Some(Value::Null | Value::Undefined) => {
                            return Ok(Some(Value::Null));
                        }
                        Some(Value::String(s)) => {
                            check_format(&s)?;
                            Some(s)
                        }
                        Some(other) => {
                            return Err(error(
                                TO_STRING_FORMAT,
                                format!(
                                    "$dateToString requires that 'format' be a string, found: {} with value {}",
                                    type_name_of(&other),
                                    display(&other)
                                ),
                            ));
                        }
                    },
                };
                let Some(ms) = date_ms(&date.eval(doc, scope)?, not_a_date)? else {
                    return match on_null {
                        Some(e) => e.eval(doc, scope),
                        None => Ok(Some(Value::Null)),
                    };
                };
                let default = if timezone.is_some() {
                    "%Y-%m-%dT%H:%M:%S.%L"
                } else {
                    "%Y-%m-%dT%H:%M:%S.%LZ"
                };
                let f = format.as_deref().unwrap_or(default);
                format_date(f, ms, &tz).map(|s| Some(Value::String(s)))
            }
            DateExpr::ToParts { date, timezone, iso } => {
                let iso = match iso {
                    None => false,
                    Some(e) => match e.eval(doc, scope)? {
                        None | Some(Value::Null | Value::Undefined) => {
                            return Ok(Some(Value::Null));
                        }
                        Some(Value::Boolean(b)) => b,
                        Some(other) => {
                            return Err(error(
                                TO_PARTS_ISO,
                                format!(
                                    "iso8601 must evaluate to a bool, found {}",
                                    type_name_of(&other)
                                ),
                            ));
                        }
                    },
                };
                let Some(tz) = zone(timezone.as_ref(), doc, scope)? else {
                    return Ok(Some(Value::Null));
                };
                let Some(ms) = date_ms(&date.eval(doc, scope)?, not_a_date)? else {
                    return Ok(Some(Value::Null));
                };
                Ok(Some(to_parts(&Parts::of(tz.local(ms)), iso)))
            }
            DateExpr::FromParts { parts, iso, timezone } => {
                from_parts(parts, *iso, timezone.as_ref(), doc, scope)
            }
        }
    }
}

fn field(f: Field, p: &Parts) -> Value {
    let (iso_year, iso_week) = iso_week(p.days);
    let n = match f {
        Field::Year => p.year,
        Field::Month => p.month,
        Field::DayOfMonth => p.day,
        Field::Hour => p.hour,
        Field::Minute => p.minute,
        Field::Second => p.second,
        Field::Millisecond => p.millisecond,
        Field::DayOfYear => p.day_of_year(),
        Field::DayOfWeek => weekday(p.days) + 1,
        Field::IsoDayOfWeek => (weekday(p.days) + 6) % 7 + 1,
        Field::Week => p.week(),
        Field::IsoWeek => iso_week,
        Field::IsoWeekYear => return Value::Int64(iso_year),
    };
    int(n)
}

fn to_parts(p: &Parts, iso: bool) -> Value {
    let mut d = Document::new();
    if iso {
        let (y, w) = iso_week(p.days);
        d.push("isoWeekYear", int(y));
        d.push("isoWeek", int(w));
        d.push("isoDayOfWeek", int((weekday(p.days) + 6) % 7 + 1));
    } else {
        d.push("year", int(p.year));
        d.push("month", int(p.month));
        d.push("day", int(p.day));
    }
    d.push("hour", int(p.hour));
    d.push("minute", int(p.minute));
    d.push("second", int(p.second));
    d.push("millisecond", int(p.millisecond));
    Value::Document(d)
}

/// `ms` written with format `f` in zone `tz`.
pub(super) fn format_date(f: &str, ms: i64, tz: &Tz) -> Result<String, MqlError> {
    let p = Parts::of(tz.local(ms));
    if !(0..=9999).contains(&p.year) {
        return Err(error(
            DATE_RANGE,
            format!(
                "Could not convert date to string: date component was outside the supported range of 0-9999: {}",
                p.year
            ),
        ));
    }
    let offset = tz.offset_at(ms);
    let (iso_year, iso_week) = iso_week(p.days);
    let mut out = String::new();
    let mut chars = f.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let s = match chars.next() {
            Some('d') => format!("{:02}", p.day),
            Some('G') => format!("{iso_year:04}"),
            Some('H') => format!("{:02}", p.hour),
            Some('j') => format!("{:03}", p.day_of_year()),
            Some('L') => format!("{:03}", p.millisecond),
            Some('m') => format!("{:02}", p.month),
            Some('M') => format!("{:02}", p.minute),
            Some('S') => format!("{:02}", p.second),
            Some('u') => ((weekday(p.days) + 6) % 7 + 1).to_string(),
            Some('V') => format!("{iso_week:02}"),
            Some('w') => (weekday(p.days) + 1).to_string(),
            Some('Y') => format!("{:04}", p.year),
            Some('U') => format!("{:02}", p.week()),
            Some('z') => {
                let a = offset.unsigned_abs();
                format!("{}{:02}{:02}", if offset < 0 { '-' } else { '+' }, a / 3600, a / 60 % 60)
            }
            Some('Z') => (offset / 60).to_string(),
            Some('b') => MONTHS[(p.month - 1) as usize][..3].to_owned(),
            Some('B') => MONTHS[(p.month - 1) as usize].to_owned(),
            Some('%') => "%".to_owned(),
            Some(other) => {
                return Err(error(
                    FORMAT_CHAR,
                    format!("Invalid format character '%{other}' in format string"),
                ));
            }
            None => return Err(error(FORMAT_PERCENT, "Unmatched '%' at end of format string")),
        };
        out.push_str(&s);
    }
    Ok(out)
}

/// The range each part of `$dateFromParts` must be in.
const PART_MIN: i64 = -32768;
const PART_MAX: i64 = 32767;

fn from_parts(
    parts: &[(&'static str, Expr)],
    iso: bool,
    timezone: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let mut values = std::collections::BTreeMap::new();
    for (name, e) in parts {
        let Some(v) = e.eval(doc, scope)?.filter(|v| !matches!(v, Value::Null | Value::Undefined))
        else {
            return Ok(Some(Value::Null));
        };
        let n = int64_of(&v).filter(|_| super::values::is_number(&v)).ok_or_else(|| {
            error(
                FROM_PARTS_INTEGER,
                format!(
                    "'{name}' must evaluate to an integer, found {} with value {}",
                    type_name_of(&v),
                    display(&v)
                ),
            )
        })?;
        let leading = *name == "year" || *name == "isoWeekYear";
        if leading && !(1..=9999).contains(&n) {
            return Err(error(
                FROM_PARTS_YEAR_RANGE,
                format!("'{name}' must evaluate to an integer in the range 1 to 9999, found {n}"),
            ));
        }
        if !leading && !(PART_MIN..=PART_MAX).contains(&n) {
            return Err(error(
                FROM_PARTS_RANGE,
                format!(
                    "'{name}' must evaluate to a value in the range [-32768, 32767]; value {n} is not in range"
                ),
            ));
        }
        values.insert(*name, n);
    }
    let Some(tz) = zone(timezone, doc, scope)? else { return Ok(Some(Value::Null)) };
    let get = |n: &str, default: i64| values.get(n).copied().unwrap_or(default);
    let days = if iso {
        iso_week_one(get("isoWeekYear", 1)) + (get("isoWeek", 1) - 1) * 7 + get("isoDayOfWeek", 1)
            - 1
    } else {
        let months = get("year", 1970) * 12 + get("month", 1) - 1;
        days_from_civil(months.div_euclid(12), months.rem_euclid(12) + 1, 1) + get("day", 1) - 1
    };
    let local = days * super::calendar::MS_DAY
        + get("hour", 0) * super::calendar::MS_HOUR
        + get("minute", 0) * super::calendar::MS_MINUTE
        + get("second", 0) * MS_SECOND
        + get("millisecond", 0);
    Ok(Some(Value::DateTime(tz.utc(local))))
}
