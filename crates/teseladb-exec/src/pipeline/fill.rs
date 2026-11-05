//! `$densify`, `$fill` and `$graphLookup` (measured on a `refrouter` 9.0.4).
//!
//! - `$densify` adds the documents a numeric or date field lacks between bounds (`full`: the
//!   least and greatest values of all documents, `partition`: of each partition, or two values),
//!   each step added to the value before it, the upper bound left out; the documents it adds hold
//!   the partition fields and the field alone, and documents without the field come first.
//! - `$fill` sets `null` or missing fields to a value, or fills them as `$locf` or `$linearFill`
//!   do over each partition sorted by `sortBy`.
//! - `$graphLookup` searches another collection breadth first from `startWith`, following
//!   `connectFromField` to `connectToField`, up to `maxDepth`.

use std::cmp::Ordering;
use std::collections::HashMap;

use teseladb_mql::{ErrorCode, Filter, FilterUse, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::errors::{FAILED_TO_PARSE, error, missing_field, unknown_field, wrong_type};
use super::sort::check_path;
use super::{Env, shape_set};
use crate::expr::dates_arith::{add_unit, check_unit};
use crate::expr::values::{display, display_bson, int64_of};
use crate::expr::{Ctx, Expr, field_path, order};
use crate::raw::with_raw_doc;
use crate::window::SetWindowFields;

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const STEP: ErrorCode = loc!(5733401);
const STEP_WHOLE: ErrorCode = loc!(6586400);
const BOUNDS_ARRAY: ErrorCode = loc!(5733402);
const BOUNDS_SIZE: ErrorCode = loc!(5733403);
const BOUNDS_KIND: ErrorCode = loc!(5733404);
const BOUNDS_DATES: ErrorCode = loc!(5733405);
const BOUNDS_NUMBERS: ErrorCode = loc!(5733406);
const NUMERIC_BOUNDS_UNIT: ErrorCode = loc!(5733409);
const DATE_BOUNDS_UNIT: ErrorCode = loc!(5733410);
const BOUNDS_VALUES: ErrorCode = loc!(5946800);
const BOUNDS_TYPES: ErrorCode = loc!(5876900);
const PARTITION_HOLDS_FIELD: ErrorCode = loc!(8993000);
const BOUNDS_PARTITION: ErrorCode = loc!(5733408);
const BOUNDS_STRING: ErrorCode = loc!(5946802);
const FIELD_TYPE: ErrorCode = loc!(5733201);
const NUMERIC_WITH_UNIT: ErrorCode = loc!(6053600);
const FILL_NOT_OBJECT: ErrorCode = loc!(6050200);
const FILL_BOTH: ErrorCode = loc!(6050201);
const FILL_METHOD: ErrorCode = loc!(6050202);
const FILL_EMPTY: ErrorCode = loc!(6050203);
const FILL_PARTITIONS: ErrorCode = loc!(6050204);
const GRAPH_MISSING: ErrorCode = loc!(12109300);
const GRAPH_DEPTH: ErrorCode = loc!(12109308);
const GRAPH_UNKNOWN: ErrorCode = loc!(12109312);

/// The bounds of `$densify`.
#[derive(Clone, Debug)]
enum Bounds {
    Full,
    Partition,
    Between(Value, Value),
}

/// A `$densify`.
#[derive(Clone, Debug)]
pub struct Densify {
    field: String,
    partitions: Vec<String>,
    step: Value,
    unit: Option<String>,
    bounds: Bounds,
}

/// The expected types of a stage argument, as `TypeMismatch` messages write them.
const STRING: &str = "type 'string'";
const OBJECT: &str = "type 'object'";
const NUMBERS: &str = "types '[decimal, long, int, double]'";

/// Whether `v` is a number.
fn numeric(v: &Value) -> bool {
    matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_))
}

fn less(a: &Value, b: &Value) -> Result<bool, MqlError> {
    // Densify values are numbers or dates: no collation applies.
    Ok(order(&Some(a.clone()), &Some(b.clone()), None)? == Ordering::Less)
}

/// `a + b` with RefDb's numeric types.
fn add(a: &Value, b: &Value) -> Value {
    use teseladb_mql::{DecOp, dec_binary};
    if matches!(a, Value::Decimal128(_)) || matches!(b, Value::Decimal128(_)) {
        return dec_binary(DecOp::Add, a, b).map_or(Value::Null, Value::Decimal128);
    }
    match (a, b) {
        (Value::Int32(x), Value::Int32(y)) => {
            let s = i64::from(*x) + i64::from(*y);
            i32::try_from(s).map_or(Value::Int64(s), Value::Int32)
        }
        (Value::Int32(_) | Value::Int64(_), Value::Int32(_) | Value::Int64(_)) => {
            match (int64_of(a), int64_of(b)) {
                (Some(x), Some(y)) => {
                    x.checked_add(y).map_or(Value::Double(x as f64 + y as f64), Value::Int64)
                }
                _ => Value::Null,
            }
        }
        _ => {
            let f = |v: &Value| crate::expr::values::f64_of(v).unwrap_or(0.0);
            Value::Double(f(a) + f(b))
        }
    }
}

impl Densify {
    /// Reads `{field, partitionByFields, range: {step, unit, bounds}}`, checked in RefDb's
    /// order: the arguments' types in document order, the missing ones, the paths, the step,
    /// the unit and then the bounds.
    pub fn parse(v: RawValue<'_>) -> Result<Densify, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "The $densify stage specification must be an object, found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut field, mut partitions, mut range) = (None, Vec::new(), None);
        for (k, x) in d.iter() {
            match (k, x) {
                ("field", RawValue::String(s)) => field = Some(s.to_owned()),
                ("field", x) => return Err(wrong_type("$densify.field", x, STRING)),
                ("partitionByFields", RawValue::Array(a)) => {
                    for (i, p) in a.iter().enumerate() {
                        match p {
                            RawValue::String(s) => partitions.push(s.to_owned()),
                            p => {
                                let at = format!("$densify.partitionByFields.{i}");
                                return Err(wrong_type(&at, p, STRING));
                            }
                        }
                    }
                }
                ("partitionByFields", x) => {
                    return Err(wrong_type("$densify.partitionByFields", x, "type 'array'"));
                }
                ("range", RawValue::Document(r)) => range = Some(r),
                ("range", x) => return Err(wrong_type("$densify.range", x, OBJECT)),
                (other, _) => return Err(unknown_field("$densify", other)),
            }
        }
        let field = field.ok_or_else(|| missing_field("$densify", "field"))?;
        let range = range.ok_or_else(|| missing_field("$densify", "range"))?;
        let (mut step, mut unit, mut bounds) = (None, None, None);
        for (k, x) in range.iter() {
            match (k, x) {
                ("step", x) if numeric(&Value::from(x)) => step = Some(Value::from(x)),
                ("step", x) => return Err(wrong_type("$densify.range.step", x, NUMBERS)),
                ("unit", RawValue::String(s)) => unit = Some(s.to_owned()),
                ("unit", x) => return Err(wrong_type("$densify.range.unit", x, STRING)),
                ("bounds", x) => bounds = Some(Value::from(x)),
                (other, _) => return Err(unknown_field("$densify.range", other)),
            }
        }
        let step = step.ok_or_else(|| missing_field("$densify.range", "step"))?;
        let bounds = bounds.ok_or_else(|| missing_field("$densify.range", "bounds"))?;
        check_path(&field)?;
        for p in &partitions {
            check_path(p)?;
            if *p == field {
                return Err(error(
                    PARTITION_HOLDS_FIELD,
                    format!(
                        "partitionByFields '{p}' cannot include field '{field}' that is being densified."
                    ),
                ));
            }
        }
        if crate::expr::values::f64_of(&step).is_none_or(|s| s.is_nan() || s <= 0.0) {
            return Err(error(
                STEP,
                "The step parameter in a range statement must be a strictly positive numeric value",
            ));
        }
        if let Some(u) = &unit {
            if crate::expr::values::f64_of(&step).is_none_or(|s| s.fract() != 0.0) {
                return Err(error(
                    STEP_WHOLE,
                    "The step parameter in a range statement must be a whole number when densifying a date range",
                ));
            }
            check_unit(u)?;
        }
        let bounds = Self::bounds(bounds, &step, unit.is_some())?;
        if matches!(bounds, Bounds::Partition) && partitions.is_empty() {
            return Err(error(
                BOUNDS_PARTITION,
                "One cannot specify the bounds as 'partition' without specifying a non-empty array of partitionByFields. You may have meant to specify 'full' bounds.",
            ));
        }
        Ok(Densify { field, partitions, step, unit, bounds })
    }

    /// Reads `bounds`: `full`, `partition`, or two ascending numbers of the step's type (no
    /// `unit`) or two dates (with a `unit`).
    fn bounds(bounds: Value, step: &Value, has_unit: bool) -> Result<Bounds, MqlError> {
        let items = match bounds {
            Value::String(s) if s == "full" => return Ok(Bounds::Full),
            Value::String(s) if s == "partition" => return Ok(Bounds::Partition),
            Value::String(_) => {
                return Err(error(
                    BOUNDS_STRING,
                    "Bounds string must either be 'full' or 'partition'",
                ));
            }
            Value::Array(items) => items,
            _ => {
                return Err(error(
                    BOUNDS_KIND,
                    "The bounds in a range statement must be the string 'full', 'partition', or an ascending array of two numbers or two dates",
                ));
            }
        };
        let [lo, hi] = <[Value; 2]>::try_from(items).map_err(|_| {
            error(
                BOUNDS_SIZE,
                "A bounding array in a range statement must have exactly two elements",
            )
        })?;
        if less(&hi, &lo)? {
            return Err(error(
                BOUNDS_ARRAY,
                "A bounding array must be an ascending array of either two dates or two numbers",
            ));
        }
        const MIXED: &str = "A bounding array must contain either both dates or both numeric types";
        if numeric(&lo) {
            if !numeric(&hi) {
                return Err(error(BOUNDS_NUMBERS, MIXED));
            }
            if has_unit {
                return Err(error(
                    NUMERIC_BOUNDS_UNIT,
                    "Numeric bounds may not have unit parameter",
                ));
            }
            let kind = std::mem::discriminant;
            if kind(&lo) != kind(&hi) || kind(&lo) != kind(step) {
                return Err(error(
                    BOUNDS_TYPES,
                    "Upper bound, lower bound, and step must all have the same type",
                ));
            }
        } else if matches!(lo, Value::DateTime(_)) {
            if !matches!(hi, Value::DateTime(_)) {
                return Err(error(BOUNDS_DATES, MIXED));
            }
            if !has_unit {
                return Err(error(
                    DATE_BOUNDS_UNIT,
                    "A bounding array of dates must specify a unit",
                ));
            }
        } else {
            return Err(error(BOUNDS_VALUES, "Explicit bounds must be numeric or dates"));
        }
        Ok(Bounds::Between(lo, hi))
    }

    /// The value after `v`.
    fn next(&self, v: &Value) -> Value {
        match (&self.unit, v) {
            (Some(u), Value::DateTime(ms)) => {
                let n = int64_of(&self.step).unwrap_or(1);
                Value::DateTime(add_unit(*ms, u, n).unwrap_or(*ms))
            }
            _ => add(v, &self.step),
        }
    }

    /// The documents with the values they lack; partitions whose values compare equal in
    /// collation `col` are one, shown with the last value seen.
    pub fn run(
        &self,
        docs: Vec<Document>,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Vec<Document>, MqlError> {
        let mut passed = Vec::new();
        let mut parts: Vec<(Vec<Value>, Vec<(Value, Document)>)> = Vec::new();
        let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
        for d in docs {
            let root = Value::Document(d.clone());
            let v = match field_path(&root, &self.field) {
                None | Some(Value::Null) => {
                    passed.push(d);
                    continue;
                }
                Some(v) => v,
            };
            match (&self.unit, &v) {
                (None, x) if numeric(x) => {}
                (Some(_), Value::DateTime(_)) => {}
                (Some(_), x) if numeric(x) => {
                    return Err(error(
                        NUMERIC_WITH_UNIT,
                        "Encountered numeric densify value in collection when step has a date unit.",
                    ));
                }
                _ => return Err(error(FIELD_TYPE, "Densify field type must be numeric or a date")),
            }
            let key: Vec<Value> = self
                .partitions
                .iter()
                .map(|p| field_path(&root, p).unwrap_or(Value::Null))
                .collect();
            let canon = super::group_key(&Value::Array(key.clone()), col);
            let i = *index.entry(canon).or_insert_with(|| {
                parts.push((Vec::new(), Vec::new()));
                parts.len() - 1
            });
            parts[i].0 = key;
            parts[i].1.push((v, d));
        }
        // Explicit bounds without partitions are filled even when no document holds the field.
        if parts.is_empty()
            && self.partitions.is_empty()
            && matches!(self.bounds, Bounds::Between(..))
        {
            parts.push((Vec::new(), Vec::new()));
        }
        for (_, docs) in &mut parts {
            crate::expr::sort_by_values(docs, None)?;
        }
        let (mut low, mut high): (Option<Value>, Option<Value>) = (None, None);
        for (_, docs) in &parts {
            if let (Some((first, _)), Some((last, _))) = (docs.first(), docs.last()) {
                if low.as_ref().is_none_or(|l| less(first, l).unwrap_or(false)) {
                    low = Some(first.clone());
                }
                if high.as_ref().is_none_or(|h| less(h, last).unwrap_or(false)) {
                    high = Some(last.clone());
                }
            }
        }
        let mut out = passed;
        for (key, docs) in parts {
            let (from, to, upper_included) = match &self.bounds {
                Bounds::Full => (low.clone(), high.clone(), true),
                Bounds::Partition => {
                    (docs.first().map(|d| d.0.clone()), docs.last().map(|d| d.0.clone()), true)
                }
                Bounds::Between(a, b) => (Some(a.clone()), Some(b.clone()), false),
            };
            let (Some(from), Some(to)) = (from, to) else { continue };
            let mut existing = docs.into_iter().peekable();
            let mut cur = from;
            loop {
                let in_range = if upper_included { !less(&to, &cur)? } else { less(&cur, &to)? };
                if !in_range {
                    break;
                }
                // The documents below the generated value come first.
                // Densify values are numbers or dates, which always compare.
                while let Some((_, d)) = existing.next_if(|(v, _)| less(v, &cur).unwrap_or(false)) {
                    out.push(d);
                }
                let present = existing.peek().is_some_and(|(v, _)| {
                    order(&Some(v.clone()), &Some(cur.clone()), None).ok() == Some(Ordering::Equal)
                });
                if !present {
                    let mut d = Document::new();
                    for (p, v) in self.partitions.iter().zip(&key) {
                        if !matches!(v, Value::Null) {
                            shape_set(&mut d, &p.split('.').collect::<Vec<_>>(), v.clone());
                        }
                    }
                    shape_set(&mut d, &self.field.split('.').collect::<Vec<_>>(), cur.clone());
                    out.push(d);
                }
                cur = self.next(&cur);
            }
            out.extend(existing.map(|(_, d)| d));
        }
        Ok(out)
    }
}

/// A `$fill`.
#[derive(Clone, Debug)]
pub struct Fill {
    /// The fields filled by method, as a `$setWindowFields`.
    window: Option<SetWindowFields>,
    /// The fields filled with a value.
    values: Vec<(String, Expr)>,
}

impl Fill {
    /// Whether it fills with values alone (no method needs a `$setWindowFields`).
    pub(super) fn is_values_only(&self) -> bool {
        self.window.is_none()
    }

    /// Reads `{sortBy, partitionBy | partitionByFields, output}`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Fill, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "The $fill stage specification must be an object, found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut sort, mut partition, mut fields, mut output) = (None, None, None, None);
        for (k, x) in d.iter() {
            match (k, x) {
                ("sortBy", RawValue::Document(s)) => sort = Some(s),
                ("sortBy", x) => return Err(wrong_type("$fill.sortBy", x, OBJECT)),
                ("partitionBy", RawValue::Document(_) | RawValue::String(_)) => {
                    partition = Some(Expr::parse_in(x, ctx)?);
                }
                ("partitionBy", x) => {
                    return Err(wrong_type("$fill.partitionBy", x, "types '[object, string]'"));
                }
                ("partitionByFields", RawValue::Array(a)) => {
                    let mut names = Vec::new();
                    for (i, f) in a.iter().enumerate() {
                        match f {
                            RawValue::String(s) => names.push(s.to_owned()),
                            f => {
                                let at = format!("$fill.partitionByFields.{i}");
                                return Err(wrong_type(&at, f, STRING));
                            }
                        }
                    }
                    fields = Some(names);
                }
                ("partitionByFields", x) => {
                    return Err(wrong_type("$fill.partitionByFields", x, "type 'array'"));
                }
                ("output", RawValue::Document(o)) => output = Some(o),
                ("output", x) => return Err(wrong_type("$fill.output", x, OBJECT)),
                (other, _) => return Err(unknown_field("$fill", other)),
            }
        }
        if partition.is_some() && fields.is_some() {
            return Err(error(
                FILL_PARTITIONS,
                "Maximum one of 'partitionBy' and 'partitionByFields can be specified in '$fill'",
            ));
        }
        if let Some(names) = fields {
            let mut obj = Document::new();
            for s in names {
                obj.push(&s, Value::String(format!("${s}")));
            }
            partition =
                Some(with_raw_doc(&obj, |raw| Expr::parse_in(RawValue::Document(raw), ctx))?);
        }
        let out = output.ok_or_else(|| missing_field("$fill", "output"))?;
        if out.iter().next().is_none() {
            return Err(error(
                FILL_EMPTY,
                "The output field in '$fill' must contain an object with at least one element",
            ));
        }
        let (mut methods, mut values) = (Vec::new(), Vec::new());
        for (field, spec) in out.iter() {
            let RawValue::Document(s) = spec else {
                return Err(error(
                    FILL_NOT_OBJECT,
                    "Each fill output specification must be an object with exactly one field",
                ));
            };
            let (mut method, mut value) = (None, None);
            for (k, x) in s.iter() {
                match (k, x) {
                    ("method", RawValue::String(m)) if m == "locf" || m == "linear" => {
                        method = Some(m.to_owned())
                    }
                    ("method", RawValue::String(_)) => {
                        return Err(error(FILL_METHOD, "Method must be either locf or linear"));
                    }
                    ("method", x) => return Err(wrong_type("$fill.method", x, STRING)),
                    ("value", x) => {
                        // `$fill` sets values with an `$addFields`, which words the errors.
                        value = Some(
                            Expr::parse_in(x, ctx).map_err(|e| super::invalid("$addFields", e))?,
                        );
                    }
                    (other, _) => return Err(unknown_field("$fill", other)),
                }
            }
            match (method, value) {
                (Some(m), None) => methods.push((field.to_owned(), m)),
                (None, Some(e)) => {
                    check_path(field).map_err(|e| super::invalid("$addFields", e))?;
                    values.push((field.to_owned(), e));
                }
                // An empty specification fills nothing.
                (None, None) => {}
                (Some(_), Some(_)) => {
                    return Err(error(
                        FILL_BOTH,
                        "Each fill output specification must have exactly one of 'method' or 'value' fields, not both",
                    ));
                }
            }
        }
        let window = if methods.is_empty() {
            None
        } else {
            Some(SetWindowFields::for_fill(partition, sort, &methods, ctx)?)
        };
        Ok(Fill { window, values })
    }

    /// The documents with their fields filled.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let mut docs = match &self.window {
            Some(w) => w.run(docs, env)?,
            None => docs,
        };
        if !self.values.is_empty() {
            for d in &mut docs {
                let scope = env.scope(d);
                let root = Value::Document(d.clone());
                for (field, e) in &self.values {
                    if matches!(field_path(&root, field), None | Some(Value::Null)) {
                        let v = e.eval(d, &scope)?.unwrap_or(Value::Null);
                        shape_set(d, &field.split('.').collect::<Vec<_>>(), v);
                    }
                }
            }
        }
        Ok(docs)
    }
}

/// A `$graphLookup`.
#[derive(Clone, Debug)]
pub struct GraphLookup {
    from: String,
    start: Expr,
    connect_from: String,
    connect_to: String,
    as_path: Vec<String>,
    max_depth: Option<i64>,
    depth_field: Option<String>,
    restrict: Option<Document>,
}

impl GraphLookup {
    /// Reads a `$graphLookup`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<GraphLookup, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "the $graphLookup stage specification must be an object, but found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut from, mut start, mut cf, mut ct, mut as_name) = (None, None, None, None, None);
        let (mut max_depth, mut depth_field, mut restrict) = (None, None, None);
        for (k, x) in d.iter() {
            match (k, x) {
                ("from", RawValue::String(s)) => from = Some(s.to_owned()),
                ("startWith", x) => start = Some(Expr::parse_in(x, ctx)?),
                ("connectFromField", RawValue::String(s)) => cf = Some(s.to_owned()),
                ("connectToField", RawValue::String(s)) => ct = Some(s.to_owned()),
                ("as", RawValue::String(s)) => as_name = Some(s.to_owned()),
                ("maxDepth", x) => {
                    let v = Value::from(x);
                    let n = int64_of(&v).unwrap_or(-1);
                    if n < 0 {
                        return Err(error(
                            GRAPH_DEPTH,
                            format!(
                                "$graphLookup 'maxDepth' must be nonnegative, but found: {}",
                                display(&v)
                            ),
                        ));
                    }
                    max_depth = Some(n);
                }
                ("depthField", RawValue::String(s)) => {
                    check_path(s)?;
                    depth_field = Some(s.to_owned());
                }
                ("restrictSearchWithMatch", RawValue::Document(r)) => {
                    restrict = Some(Document::from(r))
                }
                (other, _) => {
                    return Err(error(
                        GRAPH_UNKNOWN,
                        format!(
                            "unknown argument to $graphLookup: BSON field '$graphLookup.{other}' is an unknown field."
                        ),
                    ));
                }
            }
        }
        let Some(from) = from else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "missing 'from' option to $graphLookup stage specification: {}",
                    display_bson(&Value::Document(Document::from(d)))
                ),
            ));
        };
        let mut lacking = Vec::new();
        for (name, present) in [
            ("startWith", start.is_some()),
            ("connectFromField", cf.is_some()),
            ("connectToField", ct.is_some()),
            ("as", as_name.is_some()),
        ] {
            if !present {
                lacking.push(format!("'{name}'"));
            }
        }
        let (Some(start), Some(cf), Some(ct), Some(as_name)) = (start, cf, ct, as_name) else {
            return Err(error(
                GRAPH_MISSING,
                format!("$graphLookup is missing required field(s): {}", lacking.join(", ")),
            ));
        };
        for path in [&cf, &ct, &as_name].into_iter().chain(depth_field.as_ref()) {
            check_path(path)?;
        }
        Ok(GraphLookup {
            from,
            start,
            connect_from: cf,
            connect_to: ct,
            as_path: as_name.split('.').map(str::to_owned).collect(),
            max_depth,
            depth_field,
            restrict,
        })
    }

    /// Each document with the documents its search reaches.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let foreign = env.collection(&self.from)?;
        let restrict = match &self.restrict {
            Some(r) => Some(with_raw_doc(r, |raw| Filter::parse(raw, FilterUse::Query))?),
            None => None,
        };
        let segs: Vec<&str> = self.as_path.iter().map(String::as_str).collect();
        let mut out = Vec::with_capacity(docs.len());
        for d in docs {
            let start = self.start.eval(&d, &env.scope(&d))?;
            let found = self.search(start, &foreign, restrict.as_ref(), env)?;
            let mut e = d;
            shape_set(
                &mut e,
                &segs,
                Value::Array(found.into_iter().map(Value::Document).collect()),
            );
            out.push(e);
        }
        Ok(out)
    }

    /// The documents reached from `start`, breadth first, each once.
    /// A missing `startWith` reaches nothing, and `null` reaches only an explicit `null`.
    fn search(
        &self,
        start: Option<Value>,
        foreign: &[Document],
        restrict: Option<&Filter>,
        env: &dyn Env,
    ) -> Result<Vec<Document>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let mut frontier = match start {
            Some(Value::Array(items)) => items,
            Some(other) => vec![other],
            None => Vec::new(),
        };
        let mut seen: Vec<bool> = vec![false; foreign.len()];
        let mut found = Vec::new();
        let mut depth = 0i64;
        while !frontier.is_empty() && self.max_depth.is_none_or(|m| depth <= m) {
            let mut next = Vec::new();
            for (i, f) in foreign.iter().enumerate() {
                if seen[i] {
                    continue;
                }
                if !connects(f, &self.connect_to, &frontier, col)?
                    || !restrict.map_or(Ok(true), |r| super::scoped::matches(r, f, env))?
                {
                    continue;
                }
                seen[i] = true;
                match field_path(&Value::Document(f.clone()), &self.connect_from) {
                    Some(Value::Array(items)) => next.extend(items),
                    Some(v) => next.push(v),
                    None => {}
                }
                let mut doc = f.clone();
                if let Some(df) = &self.depth_field {
                    shape_set(&mut doc, &df.split('.').collect::<Vec<_>>(), Value::Int64(depth));
                }
                found.push(doc);
            }
            frontier = next;
            depth += 1;
        }
        Ok(found)
    }
}

/// Whether a value `path` reaches in `doc` (an array's elements too) equals one of `values`; a
/// missing field equals nothing.
fn connects(
    doc: &Document,
    path: &str,
    values: &[Value],
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<bool, MqlError> {
    let reached = match field_path(&Value::Document(doc.clone()), path) {
        None => return Ok(false),
        Some(Value::Array(items)) => {
            let mut all = items.clone();
            all.push(Value::Array(items));
            all
        }
        Some(v) => vec![v],
    };
    for r in &reached {
        for v in values {
            if order(&Some(r.clone()), &Some(v.clone()), col)? == Ordering::Equal {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
