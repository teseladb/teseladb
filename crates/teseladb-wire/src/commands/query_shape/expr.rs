//! Aggregation expressions in a query shape (010 FR-023, measured on the reference router 9.0.4,
//! 01-11-2026). An expression keeps its operators and field paths; a constant becomes its
//! literal, wrapped in `{$const: …}` in the representative shape except as an operand of an
//! operator that takes a list (`{$add: [1, "$n"]}`), where it stays bare. The operators that
//! take a list write a single operand as a one-element list; those that take any number of
//! operands show a list of constants as one literal in the debug shape (`{$add: "?array<>"}`).
//! Named arguments follow each operator's own order, with the defaults the reference server fills
//! in.

use teseladb_bson::{Document, Value};

use super::literal::{Policy, constant, literal};

/// The operators that take any number of operands: a list of constants is one literal in the
/// debug shape.
const VARIADIC: &[&str] = &[
    "$add",
    "$multiply",
    "$sum",
    "$avg",
    "$max",
    "$min",
    "$stdDevPop",
    "$stdDevSamp",
    "$and",
    "$or",
    "$concat",
    "$concatArrays",
    "$mergeObjects",
    "$setIntersection",
    "$setUnion",
    "$bitAnd",
    "$bitOr",
    "$bitXor",
    "$ifNull",
];

/// The operators whose argument is one expression, written as it is rather than as a list.
const DIRECT: &[&str] = &["$toHashedIndexKey", "$rand"];

/// The named arguments of each operator in the order the shape writes them; an argument given
/// under another name is `(given, written)`.
const NAMED: &[(&str, &[&str])] = &[
    ("$bottom", &["output", "input", "sortBy"]),
    ("$top", &["output", "input", "sortBy"]),
    ("$bottomN", &["n", "output", "input", "sortBy"]),
    ("$topN", &["n", "output", "input", "sortBy"]),
    ("$firstN", &["n", "input"]),
    ("$lastN", &["n", "input"]),
    ("$maxN", &["n", "input"]),
    ("$minN", &["n", "input"]),
    ("$convert", &["input", "to", "format", "base", "byteOrder", "onError", "onNull"]),
    ("$dateAdd", &["startDate", "unit", "amount", "timezone"]),
    ("$dateSubtract", &["startDate", "unit", "amount", "timezone"]),
    ("$dateDiff", &["startDate", "endDate", "unit", "timezone", "startOfWeek"]),
    (
        "$dateFromParts",
        &[
            "year",
            "month",
            "day",
            "isoWeekYear",
            "isoWeek",
            "isoDayOfWeek",
            "hour",
            "minute",
            "second",
            "millisecond",
            "timezone",
        ],
    ),
    ("$dateFromString", &["dateString", "timezone", "format", "onNull", "onError"]),
    ("$dateToParts", &["date", "timezone", "iso8601"]),
    ("$dateToString", &["date", "format", "timezone", "onNull"]),
    ("$dateTrunc", &["date", "unit", "binSize", "timezone", "startOfWeek"]),
    ("$deserializeEJSON", &["input", "onError"]),
    ("$serializeEJSON", &["input", "relaxed", "onError"]),
    ("$filter", &["input", "as", "arrayIndexAs", "cond", "limit"]),
    ("$map", &["input", "as", "arrayIndexAs", "in"]),
    ("$reduce", &["input", "initialValue", "arrayIndexAs", "in"]),
    ("$getField", &["field", "input"]),
    ("$setField", &["field", "input", "value"]),
    ("$hash", &["input", "algorithm"]),
    ("$let", &["vars", "in"]),
    ("$trim", &["input", "chars"]),
    ("$ltrim", &["input", "chars"]),
    ("$rtrim", &["input", "chars"]),
    ("$median", &["input", "method"]),
    ("$percentile", &["input", "p", "method"]),
    ("$regexFind", &["input", "regex", "options"]),
    ("$regexFindAll", &["input", "regex", "options"]),
    ("$regexMatch", &["input", "regex", "options"]),
    ("$replaceAll", &["input", "find", "replacement"]),
    ("$replaceOne", &["input", "find", "replacement"]),
    ("$similarityCosine", &["vectors", "score"]),
    ("$similarityDotProduct", &["vectors", "score"]),
    ("$similarityEuclidean", &["vectors", "score"]),
    ("$sortArray", &["input", "sortBy"]),
    ("$switch", &["branches", "default"]),
    ("$zip", &["inputs", "defaults", "useLongestLength"]),
    ("$cond", &["if", "then", "else"]),
    ("$shift", &["by", "output", "default"]),
    ("$expMovingAvg", &["alpha", "N", "input"]),
    ("$minMaxScaler", &["input", "min", "max"]),
];

/// The date operators that take `{date, timezone}` (or the date alone).
const DATE_PARTS: &[&str] = &[
    "$year",
    "$month",
    "$dayOfMonth",
    "$dayOfWeek",
    "$dayOfYear",
    "$hour",
    "$minute",
    "$second",
    "$millisecond",
    "$week",
    "$isoWeek",
    "$isoWeekYear",
    "$isoDayOfWeek",
];

/// The arguments written as they are in the debug shape and as `{$const: …}` in the
/// representative one.
const VERBATIM: &[(&str, &str)] =
    &[("$convert", "to"), ("$getField", "field"), ("$setField", "field")];

/// The arguments that name a variable or give a sort, written as they are in both shapes.
const NAMES: &[(&str, &str)] = &[
    ("$filter", "as"),
    ("$filter", "arrayIndexAs"),
    ("$map", "as"),
    ("$map", "arrayIndexAs"),
    ("$reduce", "arrayIndexAs"),
    ("$sortArray", "sortBy"),
    ("$top", "sortBy"),
    ("$bottom", "sortBy"),
    ("$topN", "sortBy"),
    ("$bottomN", "sortBy"),
    ("$zip", "defaults"),
    ("$zip", "useLongestLength"),
    ("$percentile", "p"),
    ("$percentile", "method"),
    ("$median", "method"),
    ("$similarityCosine", "score"),
    ("$similarityDotProduct", "score"),
    ("$similarityEuclidean", "score"),
    ("$minMaxScaler", "min"),
    ("$minMaxScaler", "max"),
];

/// The conversions that are `$convert` to a type, with the type and whether they take a
/// `format`.
const CONVERSIONS: &[(&str, &str, bool)] = &[
    ("$toString", "string", true),
    ("$toDecimal", "decimal", false),
    ("$toDouble", "double", false),
    ("$toBool", "bool", false),
    ("$toInt", "int", false),
    ("$toLong", "long", false),
    ("$toDate", "date", false),
    ("$toObjectId", "objectId", false),
    ("$toArray", "array", false),
    ("$toObject", "object", false),
];
/// The format the reference server gives the conversions that take one.
const AUTO_FORMAT: &str = "auto";
/// The variable that removes a field.
const REMOVE: &str = "$$REMOVE";
/// The binary subtype of a UUID.
const UUID_SUBTYPE: i32 = 4;

/// The one operand of `arg`: `[x]` or `x`.
fn sole(arg: &Value) -> &Value {
    match arg {
        Value::Array(items) if items.len() == 1 => &items[0],
        other => other,
    }
}

/// The expression the reference server parses operator `op` into, when it is another in disguise.
fn desugared(op: &str, arg: &Value) -> Option<Value> {
    let doc = |pairs: Vec<(&str, Value)>| {
        let mut d = Document::new();
        for (k, v) in pairs {
            d.push(k, v);
        }
        Value::Document(d)
    };
    let convert = |input: Value, to: Value, format: bool| {
        let mut c = vec![("input", input), ("to", to)];
        if format {
            c.push(("format", Value::String(AUTO_FORMAT.to_owned())));
        }
        doc(vec![("$convert", doc(c))])
    };
    if let Some((_, to, format)) = CONVERSIONS.iter().find(|(o, _, _)| *o == op) {
        return Some(convert(sole(arg).clone(), Value::String((*to).to_owned()), *format));
    }
    match op {
        "$toUUID" => {
            let to = doc(vec![
                ("type", Value::String("binData".to_owned())),
                ("subtype", Value::Int32(UUID_SUBTYPE)),
            ]);
            Some(convert(sole(arg).clone(), to, true))
        }
        "$unsetField" => match arg {
            Value::Document(d) => {
                let mut named = d.clone();
                named.push("value", Value::String(REMOVE.to_owned()));
                Some(doc(vec![("$setField", Value::Document(named))]))
            }
            _ => None,
        },
        "$hexHash" => {
            let hash = doc(vec![("$hash", arg.clone())]);
            Some(convert(hash, Value::String("string".to_owned()), true))
        }
        "$sigmoid" => {
            let neg =
                doc(vec![("$multiply", Value::Array(vec![Value::Int32(-1), sole(arg).clone()]))]);
            let exp = doc(vec![("$exp", Value::Array(vec![neg]))]);
            let add = doc(vec![("$add", Value::Array(vec![Value::Int32(1), exp]))]);
            Some(doc(vec![("$divide", Value::Array(vec![Value::Int32(1), add]))]))
        }
        _ if DATE_PARTS.contains(&op)
            && !matches!(sole(arg), Value::Document(d) if operator(d).is_none()) =>
        {
            Some(doc(vec![(op, doc(vec![("date", sole(arg).clone())]))]))
        }
        "$similarityCosine" | "$similarityDotProduct" | "$similarityEuclidean" => match arg {
            Value::Array(_) => Some(doc(vec![(
                op,
                doc(vec![("vectors", arg.clone()), ("score", Value::Boolean(false))]),
            )])),
            _ => None,
        },
        _ => None,
    }
}

/// The value of `v` if it is a constant expression: a value that is no field path, `$literal`
/// or `$const`, or a document or array of constants.
pub fn constant_value(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) if s.starts_with('$') => None,
        Value::Document(d) => match operator(d) {
            Some(("$literal" | "$const", x)) => Some(x.clone()),
            Some(_) => None,
            None => {
                let mut out = Document::new();
                for (k, x) in d.iter() {
                    out.push(k, constant_value(x)?);
                }
                Some(Value::Document(out))
            }
        },
        Value::Array(items) => {
            items.iter().map(constant_value).collect::<Option<Vec<_>>>().map(Value::Array)
        }
        other => Some(other.clone()),
    }
}

/// The operator and argument of `d` if it is `{$op: arg}`.
pub fn operator(d: &Document) -> Option<(&str, &Value)> {
    let mut it = d.iter();
    match (it.next(), it.next()) {
        (Some((k, v)), None) if k.starts_with('$') => Some((k, v)),
        _ => None,
    }
}

/// A constant where an expression stands: its literal, as `{$const: …}` in the representative
/// shape.
fn wrapped(c: &Value, p: Policy) -> Value {
    match p {
        Policy::Debug => literal(c, p),
        Policy::Representative => constant(literal(c, p)),
    }
}

/// A constant written as it is in the debug shape and as `{$const: …}` in the representative
/// one.
fn verbatim(c: Value, p: Policy) -> Value {
    match p {
        Policy::Debug => c,
        Policy::Representative => constant(c),
    }
}

/// Expression `v` in a shape.
pub fn expression(v: &Value, p: Policy) -> Value {
    if let Some(c) = constant_value(v) {
        return match v {
            // `{$literal: "$s"}` keeps its value when that value would read as a path.
            Value::Document(d) if matches!(operator(d), Some((_, Value::String(s))) if s.starts_with('$')) => {
                constant(c)
            }
            _ => wrapped(&c, p),
        };
    }
    match v {
        Value::Document(d) => match operator(d) {
            Some((op, arg)) => match desugared(op, arg) {
                Some(e) => expression(&e, p),
                None => operation(op, arg, p),
            },
            None => {
                let mut out = Document::new();
                for (k, x) in d.iter() {
                    out.push(k, expression(x, p));
                }
                Value::Document(out)
            }
        },
        Value::Array(items) => Value::Array(items.iter().map(|x| expression(x, p)).collect()),
        other => other.clone(),
    }
}

/// An operand of an operator that takes a list: a constant number, string or other scalar
/// stays bare, a constant array or document is `{$const: …}`.
fn operand(v: &Value, p: Policy) -> Value {
    match constant_value(v) {
        Some(c @ (Value::Array(_) | Value::Document(_))) => wrapped(&c, p),
        Some(c) => literal(&c, p),
        None => expression(v, p),
    }
}

/// The argument of an accumulator or window function.
pub fn accumulator_argument(v: &Value, p: Policy) -> Value {
    expression(v, p)
}

/// Operator `op` with argument `arg`.
fn operation(op: &str, arg: &Value, p: Policy) -> Value {
    let out = match (op, arg) {
        ("$meta", _) => arg.clone(),
        (_, _) if DIRECT.contains(&op) => expression(arg, p),
        (_, Value::Document(named)) if operator(named).is_none() && named_op(op) => {
            Value::Document(named_arguments(op, named, p))
        }
        (_, Value::Array(items)) => list(op, items, p),
        (_, single) => list(op, std::slice::from_ref(single), p),
    };
    Value::Document(Document::new().with(op, out))
}

/// Whether `op` takes named arguments.
fn named_op(op: &str) -> bool {
    NAMED.iter().any(|(o, _)| *o == op) || DATE_PARTS.contains(&op) || !VARIADIC.contains(&op)
}

/// The operands of `op` as a list.
fn list(op: &str, items: &[Value], p: Policy) -> Value {
    if p == Policy::Debug
        && VARIADIC.contains(&op)
        && let Some(c) = constant_value(&Value::Array(items.to_vec()))
    {
        return literal(&c, p);
    }
    Value::Array(items.iter().map(|x| operand(x, p)).collect())
}

/// The named arguments of `op`, in its order.
fn named_arguments(op: &str, named: &Document, p: Policy) -> Document {
    let order: Vec<(&str, &str)> = match NAMED.iter().find(|(o, _)| *o == op) {
        Some((_, keys)) => keys.iter().map(|k| k.split_once('>').unwrap_or((k, k))).collect(),
        None if DATE_PARTS.contains(&op) => vec![("date", "date"), ("timezone", "timezone")],
        None => named.iter().map(|(k, _)| (k, k)).collect(),
    };
    let mut out = Document::new();
    for (given, written) in order {
        let Some(v) = named.get(given).cloned().or_else(|| default_argument(op, given)) else {
            continue;
        };
        out.push(written, argument(op, given, &v, p));
    }
    out
}

/// The value the reference server gives a named argument left out.
fn default_argument(op: &str, name: &str) -> Option<Value> {
    match (op, name) {
        ("$map" | "$filter", "as") => Some(Value::String("this".to_owned())),
        ("$zip", "defaults") => Some(Value::Array(Vec::new())),
        ("$zip", "useLongestLength") => Some(Value::Boolean(false)),
        ("$minMaxScaler", "min") => Some(Value::Int32(0)),
        ("$minMaxScaler", "max") => Some(Value::Int32(1)),
        _ => None,
    }
}

/// Named argument `name` of `op`.
fn argument(op: &str, name: &str, v: &Value, p: Policy) -> Value {
    if NAMES.contains(&(op, name)) {
        return v.clone();
    }
    if VERBATIM.contains(&(op, name))
        && let Some(c) = constant_value(v)
    {
        return verbatim(c, p);
    }
    match (op, name, v) {
        (
            "$similarityCosine" | "$similarityDotProduct" | "$similarityEuclidean",
            "vectors",
            Value::Array(items),
        ) => Value::Array(items.iter().map(|x| operand(x, p)).collect()),
        ("$let", "vars", Value::Document(vars)) => {
            let mut out = Document::new();
            for (k, x) in vars.iter() {
                out.push(k, expression(x, p));
            }
            Value::Document(out)
        }
        ("$switch", "branches", Value::Array(branches)) => Value::Array(
            branches
                .iter()
                .map(|b| match b {
                    Value::Document(d) => {
                        let mut out = Document::new();
                        for k in ["case", "then"] {
                            if let Some(x) = d.get(k) {
                                out.push(k, expression(x, p));
                            }
                        }
                        Value::Document(out)
                    }
                    other => expression(other, p),
                })
                .collect(),
        ),
        _ => expression(v, p),
    }
}
