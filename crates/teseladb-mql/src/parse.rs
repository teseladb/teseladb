//! Query filters read into an [`Expr`] tree (FR-009, FR-010): every query operator, with the
//! errors RefDb gives for malformed ones. `$text`, the geospatial operators, `$where` and
//! `$expr` are read here and evaluated through [`crate::Extensions`].

use teseladb_bson::{RawDoc, RawValue};

use crate::error::{MqlError, Result, bad_value, codes, type_mismatch};
use crate::fmt::shell;
use crate::geo_filter;
use crate::jsonschema::Schema;
use crate::ops;
use crate::regex::Regex;
use crate::types::{self, TypeSpec};
use crate::value::{OwnedValue, display, exact_i64, is_number, to_f64, truncate_i64, truthy};

/// Where a filter is used, which decides the operators it may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterUse {
    /// `find`, `update`, `delete` and the other commands with a query.
    Query,
    /// A collection validator: no `$text`, `$where`, `$near` or `$nearSphere`.
    Validator,
    /// An `arrayFilters` document or the condition of `$pull`.
    Nested,
}

/// A parsed filter.
#[derive(Debug)]
pub enum Expr {
    /// All of them.
    And(Vec<Expr>),
    /// At least one.
    Or(Vec<Expr>),
    /// None of them.
    Nor(Vec<Expr>),
    /// A predicate on the values of a path.
    Field {
        /// Dotted path.
        path: String,
        /// The predicate.
        pred: Pred,
    },
    /// `$expr`.
    Expr(OwnedValue),
    /// `$where`.
    Where(OwnedValue),
    /// `$text`.
    Text(TextQuery),
    /// `$jsonSchema`.
    JsonSchema(Box<Schema>),
    /// `$sampleRate`.
    SampleRate(f64),
    /// Always matches (`{}`, `$alwaysTrue`, `$comment`).
    True,
    /// Never matches (`$alwaysFalse`, `$all: []`).
    False,
}

/// A predicate on the values of a path.
#[derive(Debug)]
pub enum Pred {
    /// Equal to the value.
    Eq(OwnedValue),
    /// Ordered against the value.
    Cmp(CmpOp, OwnedValue),
    /// Ordered against the value as an expression compares (`$_internalExprGt` and the others
    /// that the partial filters of a time series' bucket indexes use, 015): in RefDb's order
    /// across types, on the whole value; a missing value never matches.
    ExprCmp(CmpOp, OwnedValue),
    /// Equal to one of the values or matching one of the expressions.
    In(Vec<InItem>),
    /// A string matching the expression.
    Regex(Box<Regex>),
    /// The path exists.
    Exists,
    /// A value of one of the types.
    Type(Vec<TypeSpec>),
    /// `value mod divisor == remainder`.
    Mod {
        /// Divisor.
        divisor: i64,
        /// Remainder.
        remainder: i64,
    },
    /// An array of that length.
    Size(i64),
    /// An array with an element that matches the filter (`$elemMatch` with fields).
    ElemMatchDoc(Box<Expr>),
    /// An array with an element that matches the predicate (`$elemMatch` with operators).
    ElemMatchValue(Box<Pred>),
    /// A number or binary data with the bits.
    Bits(BitsOp, Vec<u32>),
    /// A geospatial operator.
    Geo(Box<GeoQuery>),
    /// All of them, each on the whole path.
    And(Vec<Pred>),
    /// Not the predicate, on the whole path.
    Not(Box<Pred>),
}

/// Orderings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `$gt`.
    Gt,
    /// `$gte`.
    Gte,
    /// `$lt`.
    Lt,
    /// `$lte`.
    Lte,
}

/// An item of `$in`.
#[derive(Debug)]
pub enum InItem {
    /// A value.
    Value(OwnedValue),
    /// A regular expression.
    Regex(Box<Regex>),
}

/// The bit operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitsOp {
    /// `$bitsAllClear`.
    AllClear,
    /// `$bitsAllSet`.
    AllSet,
    /// `$bitsAnyClear`.
    AnyClear,
    /// `$bitsAnySet`.
    AnySet,
}

/// `$text`.
#[derive(Clone, Debug, PartialEq)]
pub struct TextQuery {
    /// `$search`.
    pub search: String,
    /// `$language`.
    pub language: Option<String>,
    /// `$caseSensitive`.
    pub case_sensitive: bool,
    /// `$diacriticSensitive`.
    pub diacritic_sensitive: bool,
}

/// A geospatial operator and its argument, which 009 reads.
#[derive(Debug, PartialEq)]
pub struct GeoQuery {
    /// `$geoWithin`, `$geoIntersects`, `$near` or `$nearSphere`.
    pub operator: String,
    /// The argument.
    pub spec: OwnedValue,
    /// `$near` or `$nearSphere`, with `$maxDistance` and `$minDistance` beside it.
    pub near: bool,
    /// `$maxDistance`.
    pub max_distance: Option<f64>,
    /// `$minDistance`.
    pub min_distance: Option<f64>,
}

/// Reads a filter for `usage`.
pub fn parse(doc: RawDoc<'_>, usage: FilterUse) -> Result<Expr> {
    let expr = Parser { usage, top: true }.document(doc)?;
    if near_count(&expr, true)? > 1 {
        return Err(bad_value("Too many geoNear expressions"));
    }
    Ok(expr)
}

/// The `$near` and `$nearSphere` of a filter, which are only taken at the top level or inside
/// `$and` (and a `$or` of one clause, which RefDb removes).
fn near_count(e: &Expr, top: bool) -> Result<usize> {
    let sum = |items: &[Expr], top: bool| -> Result<usize> {
        items.iter().map(|e| near_count(e, top)).sum::<Result<usize>>()
    };
    match e {
        Expr::And(items) => sum(items, top),
        Expr::Or(items) => sum(items, top && items.len() == 1),
        Expr::Nor(items) => sum(items, false),
        Expr::Field { pred, .. } => near_in(pred, top),
        _ => Ok(0),
    }
}

fn near_in(p: &Pred, top: bool) -> Result<usize> {
    match p {
        Pred::Geo(g) if g.near && !top => Err(bad_value("geo $near must be top-level expr")),
        Pred::Geo(g) => Ok(usize::from(g.near)),
        Pred::And(preds) => preds.iter().map(|p| near_in(p, top)).sum(),
        Pred::Not(p) | Pred::ElemMatchValue(p) => near_in(p, false),
        Pred::ElemMatchDoc(e) => near_count(e, false),
        _ => Ok(0),
    }
}

struct Parser {
    usage: FilterUse,
    top: bool,
}

impl Parser {
    fn document(&self, doc: RawDoc<'_>) -> Result<Expr> {
        let mut items = Vec::new();
        for (key, value) in doc.iter() {
            items.push(if key.starts_with('$') {
                self.top_level(key, value)?
            } else {
                Expr::Field { path: key.to_owned(), pred: self.field(key, value)? }
            });
        }
        Ok(match items.len() {
            0 => Expr::True,
            1 => items.pop().unwrap_or(Expr::True),
            _ => Expr::And(items),
        })
    }

    fn nested(&self) -> Parser {
        Parser { usage: self.usage, top: false }
    }

    fn top_level(&self, key: &str, value: RawValue<'_>) -> Result<Expr> {
        Ok(match key {
            ops::AND | ops::OR | ops::NOR => {
                let RawValue::Array(arr) = value else {
                    return Err(bad_value(format!("{key} argument must be an array")));
                };
                let mut parts = Vec::new();
                for item in arr.iter() {
                    let RawValue::Document(d) = item else {
                        return Err(bad_value(format!("{key} argument's entries must be objects")));
                    };
                    parts.push(self.nested_top().document(d)?);
                }
                if parts.is_empty() {
                    return Err(bad_value(format!("{key} argument must be a non-empty array")));
                }
                match key {
                    ops::AND => Expr::And(parts),
                    ops::OR => Expr::Or(parts),
                    _ => Expr::Nor(parts),
                }
            }
            ops::EXPR => {
                self.top_only(key)?;
                Expr::Expr(OwnedValue::from_raw(value)?)
            }
            ops::WHERE => {
                self.top_only(key)?;
                if self.usage != FilterUse::Query {
                    return Err(bad_value("$where is not allowed in this context"));
                }
                if !matches!(
                    value,
                    RawValue::String(_)
                        | RawValue::JavaScript(_)
                        | RawValue::JavaScriptWithScope { .. }
                ) {
                    return Err(bad_value("$where got bad type"));
                }
                Expr::Where(OwnedValue::from_raw(value)?)
            }
            ops::TEXT => {
                self.top_only(key)?;
                if self.usage != FilterUse::Query {
                    return Err(bad_value("$text is not allowed in this context"));
                }
                Expr::Text(text(value)?)
            }
            ops::JSON_SCHEMA => {
                let RawValue::Document(d) = value else {
                    return Err(type_mismatch("$jsonSchema must be an object"));
                };
                Expr::JsonSchema(Box::new(Schema::parse(d)?))
            }
            ops::SAMPLE_RATE => {
                let rate = to_f64(&value)
                    .filter(|_| is_number(&value))
                    .ok_or_else(|| bad_value("argument to $sampleRate must be a numeric type"))?;
                if !(0.0..=1.0).contains(&rate) {
                    return Err(bad_value("numeric argument to $sampleRate must be in [0, 1]"));
                }
                Expr::SampleRate(rate)
            }
            ops::COMMENT => Expr::True,
            ops::NOT => {
                return Err(bad_value(
                    "unknown top level operator: $not. If you are trying to negate an entire \
                     expression, use $nor.",
                ));
            }
            ops::ALWAYS_TRUE | ops::ALWAYS_FALSE => {
                if value != RawValue::Int32(1) && to_f64(&value) != Some(1.0) {
                    return Err(failed(format!("{key} must be an integer value of 1")));
                }
                if key == ops::ALWAYS_TRUE { Expr::True } else { Expr::False }
            }
            _ => {
                return Err(bad_value(format!(
                    "unknown top level operator: {key}. If you have a field name that starts \
                     with a '$' symbol, consider using $getField or $setField."
                )));
            }
        })
    }

    /// Inside `$and`, `$or` and `$nor` the operators of the top level stay allowed.
    fn nested_top(&self) -> Parser {
        Parser { usage: self.usage, top: self.top }
    }

    fn top_only(&self, key: &str) -> Result<()> {
        if self.top {
            Ok(())
        } else {
            Err(bad_value(format!("{key} can only be applied to the top-level document")))
        }
    }

    /// The predicate of `path: value`.
    fn field(&self, path: &str, value: RawValue<'_>) -> Result<Pred> {
        match value {
            RawValue::Document(d) if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
                self.operators(path, d)
            }
            RawValue::Regex { pattern, options } => {
                Ok(Pred::Regex(Box::new(Regex::new(pattern, options)?)))
            }
            v => Ok(Pred::Eq(comparable(v)?)),
        }
    }

    /// `{$op: arg, ...}` on a path.
    fn operators(&self, path: &str, doc: RawDoc<'_>) -> Result<Pred> {
        if let Some((key, value)) = doc.iter().find(|(k, _)| geo_filter::is_geo(k)) {
            let near = geo_filter::is_near(key);
            if near && self.usage != FilterUse::Query {
                return Err(bad_value(format!("{key} is not allowed in this context")));
            }
            let g = if near {
                geo_filter::near(doc, key, value)?
            } else {
                geo_filter::within(doc, key, value)?
            };
            return Ok(Pred::Geo(Box::new(g)));
        }
        let mut preds = Vec::new();
        let mut regex: Option<(Option<RawValue<'_>>, Option<&str>)> = None;
        let mut options_first = false;
        for (key, value) in doc.iter() {
            match key {
                ops::REGEX => {
                    let r = regex.get_or_insert((None, None));
                    options_first = r.1.is_some();
                    r.0 = Some(value);
                }
                ops::OPTIONS => {
                    let RawValue::String(o) = value else {
                        return Err(bad_value("$options has to be a string"));
                    };
                    regex.get_or_insert((None, None)).1 = Some(o);
                }
                _ => preds.push(self.operator(path, key, value)?),
            }
        }
        if let Some((pattern, options)) = regex {
            let Some(pattern) = pattern else { return Err(bad_value("$options needs a $regex")) };
            preds.push(Pred::Regex(Box::new(regex_operand(pattern, options, options_first)?)));
        }
        Ok(if preds.len() == 1 {
            preds.pop().unwrap_or(Pred::And(Vec::new()))
        } else {
            Pred::And(preds)
        })
    }

    fn operator(&self, path: &str, key: &str, value: RawValue<'_>) -> Result<Pred> {
        Ok(match key {
            ops::EQ => Pred::Eq(comparable(value)?),
            ops::NE => Pred::Not(Box::new(match value {
                RawValue::Regex { .. } => return Err(bad_value("Can't have regex as arg to $ne.")),
                v => Pred::Eq(comparable(v)?),
            })),
            ops::GT | ops::GTE | ops::LT | ops::LTE => {
                let op = match key {
                    ops::GT => CmpOp::Gt,
                    ops::GTE => CmpOp::Gte,
                    ops::LT => CmpOp::Lt,
                    _ => CmpOp::Lte,
                };
                if let RawValue::Regex { .. } = value {
                    return Err(bad_value(format!(
                        "Can't have RegEx as arg to non-equality predicate over field '{path}'."
                    )));
                }
                Pred::Cmp(op, comparable(value)?)
            }
            ops::EXPR_GT | ops::EXPR_GTE | ops::EXPR_LT | ops::EXPR_LTE => {
                let op = match key {
                    ops::EXPR_GT => CmpOp::Gt,
                    ops::EXPR_GTE => CmpOp::Gte,
                    ops::EXPR_LT => CmpOp::Lt,
                    _ => CmpOp::Lte,
                };
                Pred::ExprCmp(op, comparable(value)?)
            }
            ops::IN => Pred::In(in_list(key, value)?),
            ops::NIN => Pred::Not(Box::new(Pred::In(in_list(key, value)?))),
            ops::EXISTS => {
                if truthy(&value) {
                    Pred::Exists
                } else {
                    Pred::Not(Box::new(Pred::Exists))
                }
            }
            ops::TYPE => Pred::Type(type_list(path, value)?),
            ops::MOD => modulo(value)?,
            ops::SIZE => Pred::Size(size(value)?),
            ops::ALL => self.all(path, value)?,
            ops::ELEM_MATCH => self.elem_match(path, value)?,
            ops::NOT => self.not(path, value)?,
            ops::BITS_ALL_CLEAR | ops::BITS_ALL_SET | ops::BITS_ANY_CLEAR | ops::BITS_ANY_SET => {
                let op = match key {
                    ops::BITS_ALL_CLEAR => BitsOp::AllClear,
                    ops::BITS_ALL_SET => BitsOp::AllSet,
                    ops::BITS_ANY_CLEAR => BitsOp::AnyClear,
                    _ => BitsOp::AnySet,
                };
                Pred::Bits(op, bit_positions(path, key, value)?)
            }
            ops::WHERE => return Err(bad_value("$where cannot be applied to a field")),
            _ => return Err(bad_value(format!("unknown operator: {key}"))),
        })
    }

    fn not(&self, path: &str, value: RawValue<'_>) -> Result<Pred> {
        Ok(Pred::Not(Box::new(match value {
            RawValue::Regex { pattern, options } => {
                Pred::Regex(Box::new(Regex::new(pattern, options)?))
            }
            RawValue::Document(d) => {
                if d.is_empty() {
                    return Err(bad_value("$not argument must be a non-empty object"));
                }
                if d.iter().any(|(k, _)| !k.starts_with('$')) {
                    return Err(bad_value(format!(
                        "unknown operator: {}",
                        d.iter().next().map_or("", |(k, _)| k)
                    )));
                }
                self.operators(path, d)?
            }
            _ => return Err(bad_value("$not argument must be a regex or an object")),
        })))
    }

    fn all(&self, path: &str, value: RawValue<'_>) -> Result<Pred> {
        let RawValue::Array(arr) = value else { return Err(bad_value("$all needs an array")) };
        let mut preds = Vec::new();
        let mut elem_matches = 0usize;
        for item in arr.iter() {
            preds.push(match item {
                RawValue::Document(d) if let Some((ops::ELEM_MATCH, inner)) = d.iter().next() => {
                    elem_matches += 1;
                    self.elem_match(path, inner)?
                }
                RawValue::Document(d)
                    if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) =>
                {
                    return Err(bad_value("no $ expressions in $all"));
                }
                RawValue::Regex { pattern, options } => {
                    Pred::Regex(Box::new(Regex::new(pattern, options)?))
                }
                v => Pred::Eq(comparable(v)?),
            });
        }
        if elem_matches > 0 && elem_matches != preds.len() {
            return Err(bad_value("$all/$elemMatch has to be consistent"));
        }
        if preds.is_empty() {
            // `$all: []` matches nothing.
            return Ok(Pred::In(Vec::new()));
        }
        Ok(Pred::And(preds))
    }

    fn elem_match(&self, path: &str, value: RawValue<'_>) -> Result<Pred> {
        let RawValue::Document(d) = value else {
            return Err(bad_value("$elemMatch needs an Object"));
        };
        let value_form = d.iter().next().is_some_and(|(k, _)| {
            k.starts_with('$')
                && !matches!(
                    k,
                    ops::AND
                        | ops::OR
                        | ops::NOR
                        | ops::WHERE
                        | ops::EXPR
                        | ops::TEXT
                        | ops::JSON_SCHEMA
                        | ops::COMMENT
                        | ops::SAMPLE_RATE
                        | ops::ALWAYS_TRUE
                        | ops::ALWAYS_FALSE
                )
        });
        if !value_form {
            for (k, _) in d.iter() {
                if matches!(k, ops::WHERE) {
                    return Err(bad_value("$where can only be applied to the top-level document"));
                }
                if matches!(k, ops::TEXT) {
                    return Err(bad_value("$text can only be applied to the top-level document"));
                }
                if matches!(k, ops::EXPR) {
                    return Err(bad_value("$expr can only be applied to the top-level document"));
                }
            }
            return Ok(Pred::ElemMatchDoc(Box::new(self.nested().document(d)?)));
        }
        if let Some((k, _)) = d.iter().find(|(k, _)| !k.starts_with('$')) {
            return Err(bad_value(format!("unknown operator: {k}")));
        }
        Ok(Pred::ElemMatchValue(Box::new(self.nested().operators(path, d)?)))
    }
}

fn failed(message: String) -> MqlError {
    MqlError::new(codes::FAILED_TO_PARSE, message)
}

/// A value for equality or ordering: `undefined` cannot be compared.
fn comparable(v: RawValue<'_>) -> Result<OwnedValue> {
    if v == RawValue::Undefined {
        return Err(bad_value("cannot compare to undefined"));
    }
    OwnedValue::from_raw(v)
}

fn in_list(key: &str, value: RawValue<'_>) -> Result<Vec<InItem>> {
    let RawValue::Array(arr) = value else {
        return Err(bad_value(format!("{key} needs an array")));
    };
    let mut items = Vec::new();
    for v in arr.iter() {
        items.push(match v {
            RawValue::Regex { pattern, options } => {
                InItem::Regex(Box::new(Regex::new(pattern, options)?))
            }
            RawValue::Document(d) if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) => {
                return Err(bad_value(format!("cannot nest $ under {key}")));
            }
            RawValue::Undefined => {
                return Err(bad_value("Cannot insert undefined into InListData"));
            }
            v => InItem::Value(comparable(v)?),
        });
    }
    Ok(items)
}

/// `$regex` with its `$options`; a regular expression with flags takes no `$options` after it,
/// nor a non-empty one before it (measured).
fn regex_operand(
    pattern: RawValue<'_>,
    options: Option<&str>,
    options_first: bool,
) -> Result<Regex> {
    // An unknown flag of `$options` has a space before its message (measured).
    let from_options = |e: MqlError| {
        if e.code == codes::REGEX_INVALID_OPTION {
            MqlError::new(e.code, format!(" {}", e.message))
        } else {
            e
        }
    };
    match pattern {
        RawValue::String(p) => Regex::new(p, options.unwrap_or("")).map_err(from_options),
        RawValue::Regex { pattern, options: own } => {
            let clash = match options {
                Some(_) if own.is_empty() => None,
                Some(o) if options_first => (!o.is_empty()).then_some(codes::REGEX_OPTIONS_BEFORE),
                Some(_) => Some(codes::REGEX_OPTIONS_AFTER),
                None => None,
            };
            if let Some(code) = clash {
                return Err(MqlError::new(code, "options set in both $regex and $options"));
            }
            match options.filter(|_| own.is_empty()) {
                Some(o) => Regex::new(pattern, o).map_err(from_options),
                None => Regex::new(pattern, own),
            }
        }
        _ => Err(bad_value("$regex has to be a string")),
    }
}

/// The alias `$type` refuses with a hint.
const MISSING: &str = "missing";

fn type_list(path: &str, value: RawValue<'_>) -> Result<Vec<TypeSpec>> {
    let one = |v: RawValue<'_>| -> Result<TypeSpec> {
        match v {
            RawValue::String(MISSING) => Err(bad_value(
                "'missing' is not a legal type name. To query for non-existence of a field, use \
                 {$exists:false}.",
            )),
            RawValue::String(alias) => types::by_alias(alias)
                .ok_or_else(|| bad_value(format!("Unknown type name alias: {alias}"))),
            v if is_number(&v) => {
                let code = exact_i64(&v).and_then(|n| i32::try_from(n).ok());
                code.and_then(types::by_code).ok_or_else(|| {
                    bad_value(format!("Invalid numerical type code: {}", display(v)))
                })
            }
            _ => Err(type_mismatch("type must be represented as a number or a string")),
        }
    };
    match value {
        RawValue::Array(arr) => {
            let list = arr.iter().map(one).collect::<Result<Vec<_>>>()?;
            if list.is_empty() {
                return Err(failed(format!("{path} must match at least one type")));
            }
            Ok(list)
        }
        v => Ok(vec![one(v)?]),
    }
}

fn modulo(value: RawValue<'_>) -> Result<Pred> {
    let RawValue::Array(arr) = value else {
        return Err(bad_value("malformed mod, needs to be an array"));
    };
    let items: Vec<RawValue<'_>> = arr.iter().collect();
    if items.len() < 2 {
        return Err(bad_value("malformed mod, not enough elements"));
    }
    if items.len() > 2 {
        return Err(bad_value("malformed mod, too many elements"));
    }
    let number = |v: RawValue<'_>, what: &str| -> Result<i64> {
        if !is_number(&v) {
            return Err(bad_value(format!("malformed mod, {what} not a number")));
        }
        let why = if to_f64(&v).is_some_and(|x| !x.is_finite()) {
            "Unable to coerce NaN/Inf to integral type"
        } else {
            "Out of bounds coercing to integral value"
        };
        truncate_i64(&v).ok_or_else(|| {
            bad_value(format!("malformed mod, {what} value is invalid :: caused by :: {why}"))
        })
    };
    let divisor = number(items[0], "divisor")?;
    let remainder = number(items[1], "remainder")?;
    if divisor == 0 {
        return Err(bad_value("divisor cannot be 0"));
    }
    Ok(Pred::Mod { divisor, remainder })
}

fn size(value: RawValue<'_>) -> Result<i64> {
    let element = format!("{}: {}", ops::SIZE, shell(value));
    if !is_number(&value) {
        return Err(bad_value(format!("Failed to parse $size. Expected a number in: {element}")));
    }
    let n = exact_i64(&value).ok_or_else(|| {
        bad_value(format!("Failed to parse $size. Expected an integer: {element}"))
    })?;
    if n < 0 {
        let msg = format!("Failed to parse $size. Expected a non-negative number in: {element}");
        return Err(bad_value(msg));
    }
    Ok(n)
}

/// Largest bit position of a bit operator.
const MAX_BIT: i64 = i32::MAX as i64;

fn bit_positions(path: &str, key: &str, value: RawValue<'_>) -> Result<Vec<u32>> {
    match value {
        RawValue::Array(arr) => {
            let mut out = Vec::new();
            for (i, v) in arr.iter().enumerate() {
                let element = format!("{i}: {}", shell(v));
                let why = if !is_number(&v) {
                    Some("Expected a number in")
                } else {
                    match exact_i64(&v) {
                        None => Some("Expected an integer"),
                        Some(n) if !(0..=MAX_BIT).contains(&n) => {
                            Some("Expected a non-negative number in")
                        }
                        Some(_) => None,
                    }
                };
                if let Some(why) = why {
                    let msg = format!("Failed to parse bit position. {why}: {element}");
                    return Err(bad_value(msg));
                }
                let n = exact_i64(&v).unwrap_or(0);
                out.push(u32::try_from(n).unwrap_or(u32::MAX));
            }
            Ok(out)
        }
        RawValue::Binary { bytes, .. } => Ok((0..bytes.len() * 8)
            .filter(|i| bytes[i / 8] >> (i % 8) & 1 == 1)
            .filter_map(|i| u32::try_from(i).ok())
            .collect()),
        v if is_number(&v) => {
            let element = format!("{key}: {}", shell(v));
            let n =
                exact_i64(&v).ok_or_else(|| failed(format!("Expected an integer: {element}")))?;
            if n < 0 {
                return Err(failed(format!("Expected a non-negative number in: {element}")));
            }
            Ok((0..63).filter(|i| n >> i & 1 == 1).collect())
        }
        _ => Err(bad_value(format!(
            "{path} takes an Array, a number, or a BinData but received: {key}: {}",
            shell(value)
        ))),
    }
}

fn text(value: RawValue<'_>) -> Result<TextQuery> {
    let RawValue::Document(d) = value else { return Err(bad_value("$text expects an object")) };
    let mut q = TextQuery {
        search: String::new(),
        language: None,
        case_sensitive: false,
        diacritic_sensitive: false,
    };
    let mut has_search = false;
    for (k, v) in d.iter() {
        match (k, v) {
            (ops::SEARCH, RawValue::String(s)) => {
                q.search = s.to_owned();
                has_search = true;
            }
            (ops::LANGUAGE, RawValue::String(s)) => q.language = Some(s.to_owned()),
            (ops::CASE_SENSITIVE, RawValue::Boolean(b)) => q.case_sensitive = b,
            (ops::DIACRITIC_SENSITIVE, RawValue::Boolean(b)) => q.diacritic_sensitive = b,
            (ops::SEARCH | ops::LANGUAGE, v) => {
                let found = types::name_of(v.element_type());
                let msg = format!("\"{k}\" had the wrong type. Expected string, found {found}");
                return Err(type_mismatch(msg));
            }
            (ops::CASE_SENSITIVE | ops::DIACRITIC_SENSITIVE, v) => {
                let found = types::name_of(v.element_type());
                let msg = format!("\"{k}\" had the wrong type. Expected bool, found {found}");
                return Err(type_mismatch(msg));
            }
            _ => return Err(bad_value("extra fields in $text")),
        }
    }
    if !has_search {
        return Err(MqlError::new(codes::NO_SUCH_KEY, "Missing expected field \"$search\""));
    }
    Ok(q)
}
