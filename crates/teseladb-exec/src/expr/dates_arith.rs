//! `$dateAdd`, `$dateSubtract`, `$dateDiff` and `$dateTrunc` (measured on a `refrouter` 9.0.4).
//!
//! Years, quarters, months, weeks and days count on the local calendar of the `timezone` (a
//! month added to January 31 is the last day of February, the same local time); hours and
//! smaller units are lengths of time. `$dateDiff` counts the boundaries of its unit between the
//! two dates; `$dateTrunc` puts a date in bins of `binSize` units counted from
//! 2000-01-01T00:00 local (weeks from the first `startOfWeek` on or after it), keeping the offset
//! in force for hours and smaller units.

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

use super::arith::type_name_of;
use super::calendar::{
    MS_DAY, MS_HOUR, MS_MINUTE, MS_SECOND, Parts, Tz, days_from_civil, month_len, weekday,
};
use super::dates::{date_ms, zone};
use super::values::{display, int64_of, is_number};
use super::{Ctx, Eval, Expr, Scope, codes, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const ADD_OBJECT: ErrorCode = code!(5166400);
const ADD_UNKNOWN: ErrorCode = code!(5166401);
const ADD_MISSING: ErrorCode = code!(5166402);
const ADD_DATE: ErrorCode = code!(5166403);
const ADD_AMOUNT: ErrorCode = code!(5166405);
const UNIT_STRING: ErrorCode = code!(5439013);
const DIFF_OBJECT: ErrorCode = code!(5166301);
const DIFF_UNKNOWN: ErrorCode = code!(5166302);
const DIFF_START: ErrorCode = code!(5166303);
const DIFF_END: ErrorCode = code!(5166304);
const DIFF_UNIT: ErrorCode = code!(5166305);
const NOT_A_DATE: ErrorCode = code!(5166307);
const START_OF_WEEK: ErrorCode = code!(5439016);
const START_OF_WEEK_STRING: ErrorCode = code!(5439015);
const TRUNC_OBJECT: ErrorCode = code!(5439007);
const TRUNC_UNKNOWN: ErrorCode = code!(5439008);
const TRUNC_DATE: ErrorCode = code!(5439009);
const TRUNC_UNIT: ErrorCode = code!(5439010);
const BIN_SIZE: ErrorCode = code!(5439017);
const BIN_POSITIVE: ErrorCode = code!(5439018);
const OVERFLOW: ErrorCode = ErrorCode { code: 159, name: "DurationOverflow" };

/// A unit of time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
}

fn unit_of(s: &str) -> Result<Unit, MqlError> {
    Ok(match s {
        "year" => Unit::Year,
        "quarter" => Unit::Quarter,
        "month" => Unit::Month,
        "week" => Unit::Week,
        "day" => Unit::Day,
        "hour" => Unit::Hour,
        "minute" => Unit::Minute,
        "second" => Unit::Second,
        "millisecond" => Unit::Millisecond,
        other => {
            return Err(error(codes::FAILED_TO_PARSE, format!("unknown time unit value: {other}")));
        }
    })
}

impl Unit {
    /// The length of a unit of time, for hours and smaller.
    fn length(self) -> Option<i64> {
        match self {
            Unit::Hour => Some(MS_HOUR),
            Unit::Minute => Some(MS_MINUTE),
            Unit::Second => Some(MS_SECOND),
            Unit::Millisecond => Some(1),
            _ => None,
        }
    }
}

/// Day of the week of `startOfWeek` (0 for Sunday): full names and three letters, in any case.
fn start_of_week(s: &str) -> Option<i64> {
    const DAYS: [&str; 7] =
        ["sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday"];
    let lower = s.to_ascii_lowercase();
    DAYS.iter().position(|d| *d == lower || d[..3] == lower).map(|i| i as i64)
}

/// A date arithmetic expression.
#[derive(Clone, Debug, PartialEq)]
pub enum DateArith {
    Add {
        subtract: bool,
        start: Expr,
        unit: Expr,
        amount: Expr,
        timezone: Option<Expr>,
    },
    Diff {
        start: Expr,
        end: Expr,
        unit: Expr,
        timezone: Option<Expr>,
        start_of_week: Option<Expr>,
    },
    Trunc {
        date: Expr,
        unit: Expr,
        bin: Option<Expr>,
        timezone: Option<Expr>,
        start_of_week: Option<Expr>,
    },
}

/// Parses one of these operators; `None` for any other.
pub(super) fn parse(
    name: &str,
    arg: RawValue<'_>,
    ctx: &Ctx,
) -> Result<Option<DateArith>, MqlError> {
    let fields = |bad: MqlError| -> Result<Vec<(String, Expr)>, MqlError> {
        let RawValue::Document(d) = arg else { return Err(bad) };
        d.iter().map(|(k, v)| Ok((k.to_owned(), Expr::parse_in(v, ctx)?))).collect()
    };
    let take = |list: &mut Vec<(String, Expr)>, key: &str| {
        list.iter().position(|(k, _)| k == key).map(|i| list.remove(i).1)
    };
    Ok(Some(match name {
        "$dateAdd" | "$dateSubtract" => {
            let mut f =
                fields(error(ADD_OBJECT, format!("{name} expects an object as its argument")))?;
            if let Some((k, _)) = f
                .iter()
                .find(|(k, _)| !["startDate", "unit", "amount", "timezone"].contains(&k.as_str()))
            {
                return Err(error(
                    ADD_UNKNOWN,
                    format!(
                        "Unrecognized argument to {name}: {k}. Expected arguments are startDate, unit, amount, and optionally timezone."
                    ),
                ));
            }
            let (start, unit, amount) =
                (take(&mut f, "startDate"), take(&mut f, "unit"), take(&mut f, "amount"));
            let (Some(start), Some(unit), Some(amount)) = (start, unit, amount) else {
                return Err(error(
                    ADD_MISSING,
                    format!("{name} requires startDate, unit, and amount to be present"),
                ));
            };
            DateArith::Add {
                subtract: name == "$dateSubtract",
                start,
                unit,
                amount,
                timezone: take(&mut f, "timezone"),
            }
        }
        "$dateDiff" => {
            let mut f =
                fields(error(DIFF_OBJECT, "$dateDiff only supports an object as its argument"))?;
            if let Some((k, _)) = f.iter().find(|(k, _)| {
                !["startDate", "endDate", "unit", "timezone", "startOfWeek"].contains(&k.as_str())
            }) {
                return Err(error(
                    DIFF_UNKNOWN,
                    format!("Unrecognized argument to $dateDiff: {k}"),
                ));
            }
            let start = take(&mut f, "startDate")
                .ok_or_else(|| error(DIFF_START, "Missing 'startDate' parameter to $dateDiff"))?;
            let end = take(&mut f, "endDate")
                .ok_or_else(|| error(DIFF_END, "Missing 'endDate' parameter to $dateDiff"))?;
            let unit = take(&mut f, "unit")
                .ok_or_else(|| error(DIFF_UNIT, "Missing 'unit' parameter to $dateDiff"))?;
            DateArith::Diff {
                start,
                end,
                unit,
                timezone: take(&mut f, "timezone"),
                start_of_week: take(&mut f, "startOfWeek"),
            }
        }
        "$dateTrunc" => {
            let mut f =
                fields(error(TRUNC_OBJECT, "$dateTrunc only supports an object as its argument"))?;
            if let Some((k, _)) = f.iter().find(|(k, _)| {
                !["date", "unit", "binSize", "timezone", "startOfWeek"].contains(&k.as_str())
            }) {
                return Err(error(
                    TRUNC_UNKNOWN,
                    format!(
                        "Unrecognized argument to $dateTrunc: {k}. Expected arguments are date, unit, and optionally, binSize, timezone, startOfWeek"
                    ),
                ));
            }
            let date = take(&mut f, "date")
                .ok_or_else(|| error(TRUNC_DATE, "Missing 'date' parameter to $dateTrunc"))?;
            let unit = take(&mut f, "unit")
                .ok_or_else(|| error(TRUNC_UNIT, "Missing 'unit' parameter to $dateTrunc"))?;
            DateArith::Trunc {
                date,
                unit,
                bin: take(&mut f, "binSize"),
                timezone: take(&mut f, "timezone"),
                start_of_week: take(&mut f, "startOfWeek"),
            }
        }
        _ => return Ok(None),
    }))
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

/// The unit argument of `name`, `None` for `null` or missing.
fn unit_arg(name: &str, e: &Expr, doc: &Document, scope: &Scope) -> Result<Option<Unit>, MqlError> {
    match e.eval(doc, scope)? {
        None | Some(Value::Null | Value::Undefined) => Ok(None),
        Some(Value::String(s)) => unit_of(&s).map(Some),
        Some(other) => Err(error(
            UNIT_STRING,
            format!("{name} requires 'unit' to be a string, but got {}", type_name_of(&other)),
        )),
    }
}

/// The `startOfWeek` argument (Sunday by default), `None` for `null` or missing.
fn week_start(
    name: &str,
    e: Option<&Expr>,
    unit: Unit,
    doc: &Document,
    scope: &Scope,
) -> Result<Option<i64>, MqlError> {
    let Some(e) = e else { return Ok(Some(0)) };
    match e.eval(doc, scope)? {
        None | Some(Value::Null | Value::Undefined) => Ok(None),
        Some(Value::String(s)) => match start_of_week(&s) {
            Some(d) => Ok(Some(d)),
            None if unit == Unit::Week => Err(error(
                START_OF_WEEK,
                format!(
                    "{name} parameter 'startOfWeek' value cannot be recognized as a day of a week: {s}"
                ),
            )),
            None => Ok(Some(0)),
        },
        Some(other) => Err(error(
            START_OF_WEEK_STRING,
            format!(
                "{name} requires 'startOfWeek' to be a string, but got {}",
                type_name_of(&other)
            ),
        )),
    }
}

/// Months added to local milliseconds `local`, the day kept or cut to the month's last.
fn add_months(local: i64, months: i64) -> i64 {
    let p = Parts::of(local);
    let total = p.year * 12 + p.month - 1 + months;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    let d = p.day.min(month_len(y, m));
    days_from_civil(y, m, d) * MS_DAY + local.rem_euclid(MS_DAY)
}

/// `ms` moved by `n` of time unit `unit` in UTC (`$setWindowFields` range bounds); `None` for
/// an unknown unit.
pub(crate) fn add_unit(ms: i64, unit: &str, n: i64) -> Option<i64> {
    let u = unit_of(unit).ok()?;
    Some(match u {
        Unit::Year => add_months(ms, n.saturating_mul(12)),
        Unit::Quarter => add_months(ms, n.saturating_mul(3)),
        Unit::Month => add_months(ms, n),
        Unit::Week => ms.saturating_add(n.saturating_mul(7 * MS_DAY)),
        Unit::Day => ms.saturating_add(n.saturating_mul(MS_DAY)),
        other => ms.saturating_add(n.saturating_mul(other.length().unwrap_or(1))),
    })
}

/// The milliseconds of a time unit of fixed length (`week` and shorter), for `$derivative` and
/// `$integral`.
pub(crate) fn fixed_unit_ms(unit: &str) -> Option<i64> {
    match unit_of(unit).ok()? {
        Unit::Week => Some(7 * MS_DAY),
        Unit::Day => Some(MS_DAY),
        other => other.length(),
    }
}

impl DateArith {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            DateArith::Add { subtract, start, unit, amount, timezone } => {
                eval_add(*subtract, start, unit, amount, timezone.as_ref(), doc, scope)
            }
            DateArith::Diff { start, end, unit, timezone, start_of_week } => {
                eval_diff(start, end, unit, timezone.as_ref(), start_of_week.as_ref(), doc, scope)
            }
            DateArith::Trunc { date, unit, bin, timezone, start_of_week } => eval_trunc(
                date,
                unit,
                bin.as_ref(),
                timezone.as_ref(),
                start_of_week.as_ref(),
                doc,
                scope,
            ),
        }
    }
}

fn eval_add(
    subtract: bool,
    start: &Expr,
    unit: &Expr,
    amount: &Expr,
    timezone: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let name = if subtract { "$dateSubtract" } else { "$dateAdd" };
    let start = start.eval(doc, scope)?;
    let unit = unit_arg(name, unit, doc, scope)?;
    let amount = amount.eval(doc, scope)?;
    let Some(tz) = zone(timezone, doc, scope)? else { return Ok(Some(Value::Null)) };
    let Some(ms) = date_ms(&start, |_| {
        error(ADD_DATE, format!("{name} requires startDate to be convertible to a date"))
    })?
    else {
        return Ok(Some(Value::Null));
    };
    let (Some(unit), Some(amount)) = (unit, amount.filter(|v| !nullish(&Some(v.clone())))) else {
        return Ok(Some(Value::Null));
    };
    let n = int64_of(&amount)
        .filter(|_| is_number(&amount))
        .ok_or_else(|| error(ADD_AMOUNT, format!("{name} expects integer amount of time units")))?;
    let n = if subtract { n.checked_neg().unwrap_or(i64::MAX) } else { n };
    let r = match unit {
        Unit::Hour | Unit::Minute | Unit::Second | Unit::Millisecond => {
            let len = unit.length().unwrap_or(1);
            n.checked_mul(len).and_then(|d| ms.checked_add(d)).ok_or_else(|| {
                error(
                    OVERFLOW,
                    format!(
                        "Overflow while adding {n}ms to {}ms",
                        ms.wrapping_add(i64::MIN).wrapping_sub(1)
                    ),
                )
            })?
        }
        calendar => {
            let local = tz.local(ms);
            let moved = match calendar {
                Unit::Year => add_months(local, n.saturating_mul(12)),
                Unit::Quarter => add_months(local, n.saturating_mul(3)),
                Unit::Month => add_months(local, n),
                Unit::Week => local.saturating_add(n.saturating_mul(7 * MS_DAY)),
                _ => local.saturating_add(n.saturating_mul(MS_DAY)),
            };
            tz.utc(moved)
        }
    };
    Ok(Some(Value::DateTime(r)))
}

/// The date argument `which` of `name`, `None` for `null` or missing.
fn date_arg(name: &str, which: &str, v: &Eval) -> Result<Option<i64>, MqlError> {
    date_ms(v, |other| {
        error(
            NOT_A_DATE,
            format!("{name} requires '{which}' to be a date, but got {}", type_name_of(other)),
        )
    })
}

fn eval_diff(
    start: &Expr,
    end: &Expr,
    unit: &Expr,
    timezone: Option<&Expr>,
    start_of_week: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let name = "$dateDiff";
    let a = date_arg(name, "startDate", &start.eval(doc, scope)?)?;
    let b = date_arg(name, "endDate", &end.eval(doc, scope)?)?;
    let unit = unit_arg(name, unit, doc, scope)?;
    let Some(tz) = zone(timezone, doc, scope)? else { return Ok(Some(Value::Null)) };
    let (Some(a), Some(b), Some(unit)) = (a, b, unit) else { return Ok(Some(Value::Null)) };
    let Some(sow) = week_start(name, start_of_week, unit, doc, scope)? else {
        return Ok(Some(Value::Null));
    };
    let n = match unit.length() {
        Some(len) => (trunc_short(b, len, &tz) - trunc_short(a, len, &tz)) / len,
        None => {
            let (pa, pb) = (Parts::of(tz.local(a)), Parts::of(tz.local(b)));
            match unit {
                Unit::Year => pb.year - pa.year,
                Unit::Quarter => {
                    (pb.year * 4 + (pb.month - 1) / 3) - (pa.year * 4 + (pa.month - 1) / 3)
                }
                Unit::Month => (pb.year * 12 + pb.month) - (pa.year * 12 + pa.month),
                Unit::Week => {
                    let week =
                        |days: i64| (days - (weekday(days) - sow).rem_euclid(7)).div_euclid(7);
                    week(pb.days) - week(pa.days)
                }
                _ => pb.days - pa.days,
            }
        }
    };
    Ok(Some(Value::Int64(n)))
}

/// `ms` cut to a multiple of `len` in local time, the offset in force kept.
fn trunc_short(ms: i64, len: i64, tz: &Tz) -> i64 {
    let off = i64::from(tz.offset_at(ms)) * MS_SECOND;
    let local = ms + off;
    local - local.rem_euclid(len) - off
}

/// Milliseconds of 2000-01-01T00:00, where bins start.
const BIN_ORIGIN: i64 = 946_684_800_000;
/// Day number of 2000-01-01.
const BIN_ORIGIN_DAYS: i64 = 10_957;

fn eval_trunc(
    date: &Expr,
    unit: &Expr,
    bin: Option<&Expr>,
    timezone: Option<&Expr>,
    start_of_week: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let name = "$dateTrunc";
    let d = date_arg(name, "date", &date.eval(doc, scope)?)?;
    let unit = unit_arg(name, unit, doc, scope)?;
    let size = match bin {
        None => Some(1),
        Some(e) => match e.eval(doc, scope)? {
            None | Some(Value::Null | Value::Undefined) => None,
            Some(v) => {
                let n = int64_of(&v).filter(|_| is_number(&v)).ok_or_else(|| {
                    error(
                        BIN_SIZE,
                        format!(
                            "{name} requires 'binSize' to be a 64-bit integer, but got value '{}' of type {}",
                            display(&v),
                            type_name_of(&v)
                        ),
                    )
                })?;
                if n <= 0 {
                    return Err(error(
                        BIN_POSITIVE,
                        format!(
                            "{name} requires 'binSize' to be greater than 0, but got value {n}"
                        ),
                    ));
                }
                Some(n)
            }
        },
    };
    let Some(tz) = zone(timezone, doc, scope)? else { return Ok(Some(Value::Null)) };
    let (Some(ms), Some(unit), Some(size)) = (d, unit, size) else { return Ok(Some(Value::Null)) };
    let Some(sow) = week_start(name, start_of_week, unit, doc, scope)? else {
        return Ok(Some(Value::Null));
    };
    if let Some(len) = unit.length() {
        let bin = len.saturating_mul(size);
        let off = i64::from(tz.offset_at(ms)) * MS_SECOND;
        let local = ms + off;
        let cut = BIN_ORIGIN + (local - BIN_ORIGIN).div_euclid(bin) * bin;
        return Ok(Some(Value::DateTime(cut - off)));
    }
    let p = Parts::of(tz.local(ms));
    let days = match unit {
        Unit::Year | Unit::Quarter | Unit::Month => {
            let per = match unit {
                Unit::Year => 12,
                Unit::Quarter => 3,
                _ => 1,
            } * size;
            let months = (p.year - 2000) * 12 + p.month - 1;
            let start = months.div_euclid(per) * per;
            days_from_civil(2000 + start.div_euclid(12), start.rem_euclid(12) + 1, 1)
        }
        Unit::Week => {
            let origin = BIN_ORIGIN_DAYS + (sow - weekday(BIN_ORIGIN_DAYS)).rem_euclid(7);
            origin + (p.days - origin).div_euclid(7 * size) * 7 * size
        }
        _ => BIN_ORIGIN_DAYS + (p.days - BIN_ORIGIN_DAYS).div_euclid(size) * size,
    };
    Ok(Some(Value::DateTime(tz.utc(days * MS_DAY))))
}
