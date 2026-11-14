//! Dates written as text (measured on a reference router 9.0.4): `$dateFromString`, and `$toDate` and
//! `$convert` of a string, which read text without a format as the reference server's timelib does (see
//! `date_free`), and the formats of `%` specifiers that `$dateFromString` takes; then the zone of
//! the text or of the `timezone` argument, the reference server's truncation of microseconds and its error
//! 159 for a date past them.

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

use super::arith::type_name_of;
use super::calendar::Tz;
use super::date_format::parse_format;
use super::date_free::{DateFail, Parsed, TextZone, parse_free};
use super::dates::check_format;
use super::values::display;
use super::{Ctx, Eval, Expr, Scope, error};

/// A conversion that failed.
const CONVERSION_FAILURE: ErrorCode = ErrorCode { code: 241, name: "ConversionFailure" };
const FROM_OBJECT: ErrorCode = ErrorCode { code: 40540, name: "Location40540" };
const FROM_UNKNOWN: ErrorCode = ErrorCode { code: 40541, name: "Location40541" };
const FROM_MISSING: ErrorCode = ErrorCode { code: 40542, name: "Location40542" };
const FROM_FORMAT: ErrorCode = ErrorCode { code: 40684, name: "Location40684" };

/// The reference server's error for a date past its microseconds.
const DURATION_OVERFLOW: ErrorCode = ErrorCode { code: 159, name: "DurationOverflow" };

const MICROS_SECOND: i128 = 1_000_000;
const MICROS_MS: i128 = 1000;

/// Whether `tz` is the reference server's plain UTC (`UTC` or a zero offset), with which a text may still
/// carry its own zone; `GMT`, `Etc/UTC` and every other name refuse one.
fn plain_utc(tz: &Tz) -> bool {
    match tz {
        Tz::Utc => true,
        Tz::Fixed(o) => *o == 0,
        Tz::Named(z) => z.name() == "UTC",
    }
}

/// The UTC microseconds of `p` read in `tz` (when the text has no zone), or the refusal of a
/// text with a zone together with a `timezone` argument other than plain UTC.
fn resolve_micros(p: &Parsed, tz: Option<&Tz>) -> Result<i128, DateFail> {
    let refuse = |what: String| {
        Err(DateFail::Text(format!(
            "you cannot pass in a date/time string with {what} together with a timezone argument"
        )))
    };
    let tz = tz.filter(|tz| p.zone == TextZone::None || !plain_utc(tz));
    match (p.zone, tz) {
        (TextZone::None, Some(tz)) => {
            let secs = p.local_us.div_euclid(MICROS_SECOND);
            let frac = p.local_us.rem_euclid(MICROS_SECOND);
            let ms = i64::try_from(secs * MICROS_MS).map_err(|_| DateFail::Overflow)?;
            Ok(i128::from(tz.utc(ms)) * MICROS_MS + frac)
        }
        (TextZone::Z, Some(_)) => refuse("time zone information ('Z')".to_owned()),
        (TextZone::Abbreviation(_), Some(_)) => {
            refuse(format!("time zone information ('{}')", p.abbreviation))
        }
        (TextZone::Offset(_), Some(_)) => refuse("GMT offset".to_owned()),
        (TextZone::None | TextZone::Z, None) => Ok(p.local_us),
        (TextZone::Offset(o) | TextZone::Abbreviation(o), None) => {
            Ok(p.local_us - i128::from(o) * MICROS_SECOND)
        }
    }
}

/// The milliseconds of `p`, as the reference server casts timelib's seconds to microseconds (overflowing
/// past ±9 223 372 036 854 seconds) and truncates them toward zero.
fn resolve(p: &Parsed, tz: Option<&Tz>) -> Result<i64, DateFail> {
    let us = resolve_micros(p, tz)?;
    let secs = us.div_euclid(MICROS_SECOND);
    i64::try_from(secs * MICROS_SECOND).map_err(|_| DateFail::Overflow)?;
    i64::try_from(us / MICROS_MS).map_err(|_| DateFail::Overflow)
}

/// The message of a text the reference server cannot read.
fn failure_text(text: &str, why: &str) -> String {
    if why.starts_with("an incomplete") || why.starts_with("you cannot") {
        why.to_owned()
    } else {
        format!("Error parsing date string '{text}'; {why}")
    }
}

/// The reference server's error for a date past its microseconds.
fn overflow() -> MqlError {
    error(
        DURATION_OVERFLOW,
        "Overflow casting from a lower-precision duration to a higher-precision duration",
    )
}

/// The milliseconds of ISO 8601 text with a zone, as `$deserializeEJSON` reads `$date`.
pub(super) fn parse_iso(text: &str) -> Option<i64> {
    let p = parse_free(text).ok()?;
    resolve(&p, None).ok()
}

/// The date of `text` for `$toDate` and `$convert`, or its conversion failure.
pub(super) fn parse_for_convert(text: &str) -> Result<Value, MqlError> {
    match parse_free(text).and_then(|p| resolve(&p, None)) {
        Ok(ms) => Ok(Value::DateTime(ms)),
        Err(DateFail::Overflow) => Err(overflow()),
        Err(DateFail::Text(why)) => Err(error(CONVERSION_FAILURE, failure_text(text, &why))),
    }
}

/// `$dateFromString`.
#[derive(Clone, Debug, PartialEq)]
pub struct FromString {
    text: Expr,
    format: Option<Expr>,
    timezone: Option<Expr>,
    on_error: Option<Expr>,
    on_null: Option<Expr>,
}

/// Parses `$dateFromString`; `None` for any other operator.
pub(super) fn parse(
    name: &str,
    arg: RawValue<'_>,
    ctx: &Ctx,
) -> Result<Option<FromString>, MqlError> {
    if name != "$dateFromString" {
        return Ok(None);
    }
    let RawValue::Document(d) = arg else {
        return Err(error(
            FROM_OBJECT,
            format!(
                "$dateFromString only supports an object as an argument, found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let mut f = FromString {
        text: Expr::Literal(Value::Null),
        format: None,
        timezone: None,
        on_error: None,
        on_null: None,
    };
    let mut text = None;
    for (k, v) in d.iter() {
        let e = Some(Expr::parse_in(v, ctx)?);
        match k {
            "dateString" => text = e,
            "format" => {
                if let RawValue::String(s) = v {
                    check_format(s)?;
                }
                f.format = e;
            }
            "timezone" => f.timezone = e,
            "onError" => f.on_error = e,
            "onNull" => f.on_null = e,
            other => {
                return Err(error(
                    FROM_UNKNOWN,
                    format!("Unrecognized argument to $dateFromString: {other}"),
                ));
            }
        }
    }
    f.text = text
        .ok_or_else(|| error(FROM_MISSING, "Missing 'dateString' parameter to $dateFromString"))?;
    Ok(Some(f))
}

impl FromString {
    /// Evaluates `$dateFromString`.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let text = self.text.eval(doc, scope)?;
        let format = match &self.format {
            None => None,
            Some(e) => match e.eval(doc, scope)? {
                None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
                Some(Value::String(s)) => {
                    check_format(&s)?;
                    Some(s)
                }
                Some(other) => {
                    return Err(error(
                        FROM_FORMAT,
                        format!(
                            "$dateFromString requires that 'format' be a string, found: {} with value {}",
                            type_name_of(&other),
                            display(&other)
                        ),
                    ));
                }
            },
        };
        let tz = match &self.timezone {
            None => None,
            Some(e) => match e.eval(doc, scope)? {
                None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
                Some(Value::String(s)) => Some(Tz::parse(&s)?),
                Some(other) => {
                    return Err(error(
                        ErrorCode { code: 40517, name: "Location40517" },
                        format!(
                            "timezone must evaluate to a string, found {}",
                            type_name_of(&other)
                        ),
                    ));
                }
            },
        };
        let fail = |msg: String| -> Result<Eval, MqlError> {
            match &self.on_error {
                Some(e) => e.eval(doc, scope),
                None => Err(error(CONVERSION_FAILURE, msg)),
            }
        };
        let s = match text {
            None | Some(Value::Null | Value::Undefined) => {
                return match &self.on_null {
                    Some(e) => e.eval(doc, scope),
                    None => Ok(Some(Value::Null)),
                };
            }
            Some(Value::String(s)) => s,
            Some(other) => {
                return fail(format!(
                    "$dateFromString requires that 'dateString' be a string, found: {} with value {}",
                    type_name_of(&other),
                    display(&other)
                ));
            }
        };
        let parsed = match &format {
            Some(f) => parse_format(&s, f).map_err(DateFail::Text),
            None => parse_free(&s),
        };
        match parsed.and_then(|p| resolve(&p, tz.as_ref())) {
            Ok(ms) => Ok(Some(Value::DateTime(ms))),
            Err(DateFail::Overflow) => Err(overflow()),
            Err(DateFail::Text(why)) => fail(failure_text(&s, &why)),
        }
    }
}
