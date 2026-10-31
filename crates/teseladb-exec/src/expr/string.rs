//! String expressions (measured on a `refrouter` 9.0.4): `$concat`, `$toLower`, `$toUpper`,
//! `$strLenBytes`, `$strLenCP`, `$substrBytes` (`$substr`), `$substrCP`, `$indexOfBytes`,
//! `$indexOfCP`, `$split` and `$strcasecmp` (`$toString` is a conversion, in `convert`). Most of them take `null` or a missing
//! value as `null` or the empty string, as noted on each; `$toLower`, `$toUpper`, `$substr*` and
//! `$strcasecmp` turn numbers and dates into text first.

use teseladb_bson::{Document, Value};
use teseladb_mql::MqlError;

use super::arith::type_name_of;
use super::{Eval, Expr, Scope, codes, count, error};

/// Which operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Str {
    Concat,
    ToLower,
    ToUpper,
    LenBytes,
    LenCp,
    SubstrBytes,
    SubstrCp,
    IndexBytes,
    IndexCp,
    Split,
    CaseCmp,
}

const NAMES: [(&str, Str); 12] = [
    ("$concat", Str::Concat),
    ("$toLower", Str::ToLower),
    ("$toUpper", Str::ToUpper),
    ("$strLenBytes", Str::LenBytes),
    ("$strLenCP", Str::LenCp),
    ("$substrBytes", Str::SubstrBytes),
    ("$substr", Str::SubstrBytes),
    ("$substrCP", Str::SubstrCp),
    ("$indexOfBytes", Str::IndexBytes),
    ("$indexOfCP", Str::IndexCp),
    ("$split", Str::Split),
    ("$strcasecmp", Str::CaseCmp),
];

/// A double as RefDb writes it as text: the shortest digits that read back the same, with an
/// exponent of at least two digits when the decimal exponent is below −4 or from 16 up (`1.5`,
/// `2`, `1e+16`, `1e-07`, `1.2345678901234568e+17`), as measured on a `refrouter` 9.0.4.
pub fn double_text(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    let text = format!("{x:e}");
    let (m, e) = text.split_once('e').unwrap_or((&text, "0"));
    let exp: i32 = e.parse().unwrap_or(0);
    if x != 0.0 && !(-4..16).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{m}e{sign}{:02}", exp.unsigned_abs());
    }
    format!("{x}")
}

/// A date as RefDb writes it: ISO 8601 in UTC with milliseconds.
pub fn date_text(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm, public domain).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

/// A value as text the way the string operators take it: `null` and a missing value are empty.
fn coerce(v: &Eval) -> Result<String, MqlError> {
    Ok(match v {
        None | Some(Value::Null | Value::Undefined) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Int32(n)) => n.to_string(),
        Some(Value::Int64(n)) => n.to_string(),
        Some(Value::Double(x)) => double_text(*x),
        Some(Value::Decimal128(d)) => d.to_string(),
        Some(Value::DateTime(t)) => date_text(*t),
        Some(other) => {
            return Err(error(
                codes::CANNOT_CONVERT_TO_STRING,
                format!("can't convert from BSON type {} to String", type_name_of(other)),
            ));
        }
    })
}

/// A whole-number argument of a string operator.
fn integer(v: &Eval) -> Option<i64> {
    match v {
        Some(Value::Int32(n)) => Some(i64::from(*n)),
        Some(Value::Int64(n)) => Some(*n),
        Some(Value::Double(x)) if x.fract() == 0.0 => Some(*x as i64),
        _ => None,
    }
}

impl Str {
    /// The operator named `name`.
    pub fn of(name: &str) -> Option<Str> {
        NAMES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
    }

    /// Checks the number of arguments.
    pub fn check(self, name: &str, args: &[Expr]) -> Result<(), MqlError> {
        match self {
            Str::Concat => Ok(()),
            Str::ToLower | Str::ToUpper | Str::LenBytes | Str::LenCp => count(name, args, 1..=1),
            Str::SubstrBytes | Str::SubstrCp => {
                count(if self == Str::SubstrCp { name } else { "$substrBytes" }, args, 3..=3)
            }
            Str::IndexBytes | Str::IndexCp => count(name, args, 2..=4),
            Str::Split | Str::CaseCmp => count(name, args, 2..=2),
        }
    }

    /// Evaluates the operator.
    pub fn eval(self, args: &[Expr], doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let vals: Vec<Eval> = args.iter().map(|a| a.eval(doc, scope)).collect::<Result<_, _>>()?;
        let text = |s: String| Ok(Some(Value::String(s)));
        match self {
            Str::Concat => {
                let mut out = String::new();
                for v in &vals {
                    match v {
                        None | Some(Value::Null | Value::Undefined) => {
                            return Ok(Some(Value::Null));
                        }
                        Some(Value::String(s)) => out.push_str(s),
                        Some(other) => {
                            return Err(error(
                                codes::CONCAT_TYPES,
                                format!(
                                    "$concat only supports strings, not {}",
                                    type_name_of(other)
                                ),
                            ));
                        }
                    }
                }
                text(out)
            }
            Str::ToLower => text(coerce(&vals[0])?.to_ascii_lowercase()),
            Str::ToUpper => text(coerce(&vals[0])?.to_ascii_uppercase()),
            Str::LenBytes | Str::LenCp => {
                let Some(Value::String(s)) = &vals[0] else {
                    let (code, name) = match self {
                        Str::LenBytes => (codes::STR_LEN_BYTES, "$strLenBytes"),
                        _ => (codes::STR_LEN_CP, "$strLenCP"),
                    };
                    let found = vals[0].as_ref().map_or("missing", type_name_of);
                    return Err(error(
                        code,
                        format!("{name} requires a string argument, found: {found}"),
                    ));
                };
                let n = if self == Str::LenBytes { s.len() } else { s.chars().count() };
                Ok(Some(Value::Int32(i32::try_from(n).unwrap_or(i32::MAX))))
            }
            Str::SubstrBytes => substr_bytes(&vals),
            Str::SubstrCp => substr_cp(&vals),
            Str::IndexBytes | Str::IndexCp => index_of(self, &vals),
            Str::Split => split(&vals),
            Str::CaseCmp => {
                let (a, b) = (
                    coerce(&vals[0])?.to_ascii_lowercase(),
                    coerce(&vals[1])?.to_ascii_lowercase(),
                );
                Ok(Some(Value::Int32(match a.cmp(&b) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                })))
            }
        }
    }
}

fn substr_bytes(vals: &[Eval]) -> Result<Eval, MqlError> {
    let s = coerce(&vals[0])?;
    let start = integer(&vals[1]).unwrap_or(0);
    if start < 0 {
        return Err(error(
            codes::SUBSTR_START,
            format!("$substrBytes:  starting index must be non-negative (got: {start})"),
        ));
    }
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(s.len());
    let len = integer(&vals[2]).unwrap_or(-1);
    let end = match usize::try_from(len) {
        Ok(l) => start.saturating_add(l).min(s.len()),
        Err(_) => s.len(),
    };
    if !s.is_char_boundary(start) {
        return Err(error(
            codes::SUBSTR_START_UTF8,
            "$substrBytes:  Invalid range, starting index is a UTF-8 continuation byte.",
        ));
    }
    if !s.is_char_boundary(end) {
        return Err(error(
            codes::SUBSTR_END_UTF8,
            "$substrBytes:  Invalid range, ending index is in the middle of a UTF-8 character.",
        ));
    }
    Ok(Some(Value::String(s[start..end].to_owned())))
}

fn substr_cp(vals: &[Eval]) -> Result<Eval, MqlError> {
    let s = coerce(&vals[0])?;
    let start = integer(&vals[1]).unwrap_or(0);
    let len = integer(&vals[2]).unwrap_or(0);
    if start < 0 {
        return Err(error(
            codes::SUBSTR_CP_START,
            "$substrCP: the starting index must be nonnegative integer.",
        ));
    }
    if len < 0 {
        return Err(error(
            codes::SUBSTR_CP_LENGTH,
            "$substrCP: length must be a nonnegative integer.",
        ));
    }
    let out: String = s
        .chars()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .take(usize::try_from(len).unwrap_or(usize::MAX))
        .collect();
    Ok(Some(Value::String(out)))
}

fn index_of(op: Str, vals: &[Eval]) -> Result<Eval, MqlError> {
    let name = if op == Str::IndexBytes { "$indexOfBytes" } else { "$indexOfCP" };
    let hay = match &vals[0] {
        None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(error(
                codes::INDEX_OF_FIRST,
                format!(
                    "{name} requires a string as the first argument, found: {}",
                    type_name_of(other)
                ),
            ));
        }
    };
    let Some(Value::String(needle)) = &vals[1] else {
        let found = vals[1].as_ref().map_or("missing", type_name_of);
        return Err(error(
            codes::INDEX_OF_SECOND,
            format!("{name} requires a string as the second argument, found: {found}"),
        ));
    };
    let units: Vec<usize> = if op == Str::IndexBytes {
        (0..=hay.len()).collect()
    } else {
        hay.char_indices().map(|(i, _)| i).chain([hay.len()]).collect()
    };
    let total = units.len() - 1;
    let bound = |i: usize, default: usize| -> Result<usize, MqlError> {
        match vals.get(i) {
            None => Ok(default),
            Some(v) => match integer(v) {
                Some(n) if n >= 0 => Ok(usize::try_from(n).unwrap_or(usize::MAX)),
                _ => Err(error(
                    codes::INDEX_OF_BOUND,
                    format!("{name} requires a nonnegative integral starting index, found: {v:?}"),
                )),
            },
        }
    };
    let (start, end) = (bound(2, 0)?, bound(3, total)?.min(total));
    if start > end || start > total {
        return Ok(Some(Value::Int32(-1)));
    }
    for (pos, &at) in units.iter().enumerate().take(end + 1).skip(start) {
        if hay.as_bytes()[at..].starts_with(needle.as_bytes()) && at + needle.len() <= units[end] {
            return Ok(Some(Value::Int32(i32::try_from(pos).unwrap_or(i32::MAX))));
        }
    }
    Ok(Some(Value::Int32(-1)))
}

fn split(vals: &[Eval]) -> Result<Eval, MqlError> {
    let s = match &vals[0] {
        None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(error(
                codes::SPLIT_FIRST,
                format!(
                    "$split requires an expression that evaluates to a string as a first argument, found: {}",
                    type_name_of(other)
                ),
            ));
        }
    };
    let sep = match &vals[1] {
        None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
        Some(Value::String(sep)) if sep.is_empty() => {
            return Err(error(codes::SPLIT_EMPTY, "$split requires a non-empty separator"));
        }
        Some(Value::String(sep)) => sep.clone(),
        Some(other) => {
            return Err(error(
                codes::SPLIT_SECOND,
                format!(
                    "$split requires an expression that evaluates to a string or regular expression as a second argument, found: {}",
                    type_name_of(other)
                ),
            ));
        }
    };
    Ok(Some(Value::Array(s.split(sep.as_str()).map(|p| Value::String(p.to_owned())).collect())))
}
