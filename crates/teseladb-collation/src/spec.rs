//! RefDb's collation document (FR-017): read as its IDL `Collation` struct reads it (strict,
//! fields in document order, `null` and `undefined` as absent but in `backwards`), then as
//! `CollatorFactoryICU::makeFromBSON`: `simple` is the binary order, the `locale` must be one
//! RefDb accepts, each absent option takes the locale's default, the version must be ICU's,
//! and two combinations are refused (`backwards` at strength 1; `caseFirst` without `caseLevel`
//! at strength 1 or 2). Errors carry RefDb's codes.

use teseladb_bson::{Document, NumberParts, RawDoc, RawValue, number_parts};

use crate::options::{CaseFirst, MaxVariable};
use crate::sortkey::{Alternate, Strength};
use crate::{Collation, LocaleError};

/// The ICU version RefDb 9.0 reports in `version` (D-043).
pub const ICU_VERSION: &str = "57.1";

/// A collation document RefDb refuses: its error code, code name and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError {
    /// RefDb's error code.
    pub code: i32,
    /// Its name.
    pub code_name: &'static str,
    /// The message.
    pub message: String,
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code_name, self.code, self.message)
    }
}

impl std::error::Error for SpecError {}

fn error(code: i32, code_name: &'static str, message: String) -> SpecError {
    SpecError { code, code_name, message }
}

fn bad_value(message: String) -> SpecError {
    error(2, "BadValue", message)
}

/// A complete collation, as RefDb reports it (`listIndexes`, `listCollections`, `explain`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollationSpec {
    /// `locale`
    pub locale: String,
    /// `caseLevel`
    pub case_level: bool,
    /// `caseFirst`
    pub case_first: CaseFirst,
    /// `strength`
    pub strength: Strength,
    /// `numericOrdering`
    pub numeric_ordering: bool,
    /// `alternate`
    pub alternate: Alternate,
    /// `maxVariable`
    pub max_variable: MaxVariable,
    /// `normalization`
    pub normalization: bool,
    /// `backwards`
    pub backwards: bool,
    /// `version`
    pub version: String,
}

/// What a collation document asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedCollation {
    /// `{locale: "simple"}`: the binary order of strings.
    Simple,
    /// A collation, and its complete document.
    Collation(Collation, CollationSpec),
}

impl CollationSpec {
    /// The document RefDb reports, fields in its order.
    pub fn to_document(&self) -> Document {
        let case_first = match self.case_first {
            CaseFirst::Off => "off",
            CaseFirst::Lower => "lower",
            CaseFirst::Upper => "upper",
        };
        let alternate = match self.alternate {
            Alternate::NonIgnorable => "non-ignorable",
            Alternate::Shifted => "shifted",
        };
        let max_variable = match self.max_variable {
            MaxVariable::Space => "space",
            _ => "punct",
        };
        Document::new()
            .with("locale", self.locale.as_str())
            .with("caseLevel", self.case_level)
            .with("caseFirst", case_first)
            .with("strength", self.strength as i32)
            .with("numericOrdering", self.numeric_ordering)
            .with("alternate", alternate)
            .with("maxVariable", max_variable)
            .with("normalization", self.normalization)
            .with("backwards", self.backwards)
            .with("version", self.version.as_str())
    }
}

/// The fields as the document gives them; `None` if absent.
#[derive(Default)]
struct Fields {
    locale: Option<String>,
    case_level: Option<bool>,
    case_first: Option<CaseFirst>,
    strength: Option<i32>,
    numeric_ordering: Option<bool>,
    alternate: Option<Alternate>,
    max_variable: Option<MaxVariable>,
    normalization: Option<bool>,
    backwards: Option<bool>,
    version: Option<String>,
}

/// RefDb's name of a BSON type, for messages.
fn type_name(v: &RawValue<'_>) -> &'static str {
    match v {
        RawValue::Double(_) => "double",
        RawValue::String(_) => "string",
        RawValue::Document(_) => "object",
        RawValue::Array(_) => "array",
        RawValue::Binary { .. } => "binData",
        RawValue::Undefined => "undefined",
        RawValue::ObjectId(_) => "objectId",
        RawValue::Boolean(_) => "bool",
        RawValue::DateTime(_) => "date",
        RawValue::Null => "null",
        RawValue::Regex { .. } => "regex",
        RawValue::DbPointer { .. } => "dbPointer",
        RawValue::JavaScript(_) => "javascript",
        RawValue::Symbol(_) => "symbol",
        RawValue::JavaScriptWithScope { .. } => "javascriptWithScope",
        RawValue::Int32(_) => "int",
        RawValue::Timestamp { .. } => "timestamp",
        RawValue::Int64(_) => "long",
        RawValue::Decimal128(_) => "decimal",
        RawValue::MinKey => "minKey",
        RawValue::MaxKey => "maxKey",
    }
}

fn wrong_type(name: &str, v: &RawValue<'_>, expected: &str) -> SpecError {
    let found = type_name(v);
    let message =
        format!("BSON field 'collation.{name}' is the wrong type '{found}', expected {expected}");
    error(14, "TypeMismatch", message)
}

fn string<'a>(name: &str, v: &RawValue<'a>) -> Result<&'a str, SpecError> {
    match v {
        RawValue::String(s) => Ok(s),
        other => Err(wrong_type(name, other, "type 'string'")),
    }
}

fn boolean(name: &str, v: &RawValue<'_>) -> Result<bool, SpecError> {
    match v {
        RawValue::Boolean(b) => Ok(*b),
        other => Err(wrong_type(name, other, "type 'bool'")),
    }
}

fn enumeration<T: Copy>(
    name: &str,
    v: &RawValue<'_>,
    values: &[(&str, T)],
) -> Result<T, SpecError> {
    let s = string(name, v)?;
    values.iter().find(|(n, _)| *n == s).map(|(_, t)| *t).ok_or_else(|| {
        let message =
            format!("Enumeration value '{s}' for field 'collation.{name}' is not a valid value.");
        bad_value(message)
    })
}

/// `strength` as IDL's `safeInt`: any number that is an integer within `i32`.
fn safe_int(v: &RawValue<'_>) -> Result<i32, SpecError> {
    let parts = number_parts(v)
        .ok_or_else(|| wrong_type("strength", v, "types '[long, int, decimal, double]'"))?;
    let integer = match parts {
        NumberParts::Zero => Some(0),
        NumberParts::Finite { negative, digits, exponent } => {
            // `±0.d₁…dₙ × 10^exponent` is an integer when the exponent reaches `n`.
            let shift = exponent - i32::try_from(digits.len()).unwrap_or(i32::MAX);
            (shift >= 0 && exponent <= 10)
                .then(|| {
                    let mut value = digits.iter().fold(0i64, |n, d| n * 10 + i64::from(*d));
                    for _ in 0..shift {
                        value *= 10;
                    }
                    if negative { -value } else { value }
                })
                .and_then(|n| i32::try_from(n).ok())
        }
        _ => None,
    };
    integer.ok_or_else(|| {
        bad_value("BSON field 'collation.strength' is not an integer within int range".into())
    })
}

fn fields(doc: RawDoc<'_>) -> Result<Fields, SpecError> {
    let mut f = Fields::default();
    let mut seen: Vec<&str> = Vec::new();
    for (name, v) in doc.iter() {
        if seen.contains(&name) {
            let message = format!("BSON field 'collation.{name}' is a duplicate field");
            return Err(error(40413, "IDLDuplicateField", message));
        }
        seen.push(name);
        if matches!(v, RawValue::Null | RawValue::Undefined) && name != "backwards" {
            continue;
        }
        match name {
            "locale" => f.locale = Some(string(name, &v)?.to_owned()),
            "caseLevel" => f.case_level = Some(boolean(name, &v)?),
            "caseFirst" => {
                let values = [
                    ("upper", CaseFirst::Upper),
                    ("lower", CaseFirst::Lower),
                    ("off", CaseFirst::Off),
                ];
                f.case_first = Some(enumeration(name, &v, &values)?);
            }
            "strength" => {
                let n = safe_int(&v)?;
                if !(0..=5).contains(&n) {
                    let bound = if n < 0 { ">= 0" } else { "<= 5" };
                    return Err(bad_value(format!(
                        "BSON field 'strength' value must be {bound}, actual value '{n}'"
                    )));
                }
                f.strength = Some(n);
            }
            "numericOrdering" => f.numeric_ordering = Some(boolean(name, &v)?),
            "alternate" => {
                let values =
                    [("non-ignorable", Alternate::NonIgnorable), ("shifted", Alternate::Shifted)];
                f.alternate = Some(enumeration(name, &v, &values)?);
            }
            "maxVariable" => {
                let values = [("punct", MaxVariable::Punct), ("space", MaxVariable::Space)];
                f.max_variable = Some(enumeration(name, &v, &values)?);
            }
            "normalization" => f.normalization = Some(boolean(name, &v)?),
            "backwards" => f.backwards = Some(boolean(name, &v)?),
            "version" => f.version = Some(string(name, &v)?.to_owned()),
            other => {
                let message = format!("BSON field 'collation.{other}' is an unknown field.");
                return Err(error(40415, "IDLUnknownField", message));
            }
        }
    }
    Ok(f)
}

fn strength(n: i32) -> Option<Strength> {
    Some(match n {
        1 => Strength::Primary,
        2 => Strength::Secondary,
        3 => Strength::Tertiary,
        4 => Strength::Quaternary,
        5 => Strength::Identical,
        _ => return None,
    })
}

/// Reads a collation document as RefDb does.
pub fn parse_collation(doc: RawDoc<'_>) -> Result<ParsedCollation, SpecError> {
    let f = fields(doc)?;
    // As RefDb writes the specification in its messages (measured on a `refrouter` 9.0.4).
    let spec_text = || teseladb_bson::shell::shell_doc(doc);
    let Some(locale) = f.locale else {
        let message = "BSON field 'collation.locale' is missing but a required field".into();
        return Err(error(40414, "IDLFailedToParse", message));
    };
    if locale.contains('\0') {
        let message =
            format!("Field 'locale' cannot contain null byte. Collation spec: {}", spec_text());
        return Err(bad_value(message));
    }
    if locale == "simple" {
        return Ok(ParsedCollation::Simple);
    }
    let base = Collation::for_locale(&locale).map_err(|LocaleError { suggestion, .. }| {
        let mean = suggestion.map(|s| format!(". Did you mean '{s}'?")).unwrap_or_default();
        bad_value(format!("Field 'locale' is invalid in: {}{mean}", spec_text()))
    })?;
    // RefDb lets strength 0 through its parser and then cannot use it (DV): refused here.
    let strength = match f.strength {
        None => base.strength,
        Some(n) => strength(n)
            .ok_or_else(|| bad_value(format!("Field 'strength' is invalid in: {}", spec_text())))?,
    };
    let collation = Collation {
        strength,
        case_level: f.case_level.unwrap_or(base.case_level),
        case_first: f.case_first.unwrap_or(base.case_first),
        numeric_ordering: f.numeric_ordering.unwrap_or(base.numeric_ordering),
        alternate: f.alternate.unwrap_or(base.alternate),
        max_variable: f.max_variable.unwrap_or(base.max_variable),
        normalization: f.normalization.unwrap_or(base.normalization),
        backwards: f.backwards.unwrap_or(base.backwards),
        tailoring: base.tailoring,
    };
    let version = match f.version {
        None => ICU_VERSION.to_owned(),
        Some(v) if v == ICU_VERSION => v,
        Some(v) => {
            let message = format!(
                "Requested collation version {v} but the only available collator version was \
                 {ICU_VERSION}. Requested collation spec: {}",
                spec_text()
            );
            return Err(error(161, "IncompatibleCollationVersion", message));
        }
    };
    if collation.backwards && collation.strength == Strength::Primary {
        let message = format!("'backwards' is invalid with 'strength' of 1 in: {}", spec_text());
        return Err(bad_value(message));
    }
    if collation.case_first != CaseFirst::Off
        && !collation.case_level
        && collation.strength <= Strength::Secondary
    {
        let message = format!(
            "'caseFirst' is invalid unless 'caseLevel' is on or 'strength' is greater than 2 \
             in: {}",
            spec_text()
        );
        return Err(bad_value(message));
    }
    let spec = CollationSpec {
        locale,
        case_level: collation.case_level,
        case_first: collation.case_first,
        strength: collation.strength,
        numeric_ordering: collation.numeric_ordering,
        alternate: collation.alternate,
        max_variable: collation.max_variable,
        normalization: collation.normalization,
        backwards: collation.backwards,
        version,
    };
    Ok(ParsedCollation::Collation(collation, spec))
}
