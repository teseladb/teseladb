//! `collMod` of the special collections (015 FR-001, FR-006, FR-009): the definition of a view
//! (`viewOn`, `pipeline`), the buckets of a time series (`timeseries`, which only widen) and the
//! expiry of a time series or a clustered collection (`expireAfterSeconds`), with the checks, the
//! messages and the order of the options of a `refnode` 9.0.4 (measured on 02-11-2026).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::collection_types::{CollectionKind, Granularity};

use super::Backend;
use super::kinds::{
    EXPIRE, FIXED_BUCKETING, GRANULARITY, MAX_BUCKET_SPAN, MAX_SPAN, PIPELINE, ROUNDING,
    TIMESERIES, VIEW_ON, bad, bounded, checked_expire, integral, invalid, kind_of,
};
use crate::error::{BAD_VALUE, CommandError, IDL_UNKNOWN_FIELD};
use crate::types::{wrong_type, wrong_types};

/// The fields of `timeseries` that `collMod` takes.
const TIMESERIES_FIELDS: [&str; 3] = [GRANULARITY, MAX_SPAN, ROUNDING];
/// The numeric types of the spans of `collMod.timeseries`, as its errors name them.
const NUMBER: [&str; 4] = ["long", "int", "decimal", "double"];
/// The value of `expireAfterSeconds` that turns the expiry off.
const OFF: &str = "off";
/// The field of `collMod` that changes one index.
const INDEX: &str = "index";

/// Refuses a `collMod` that changes `timeseries` with anything else (`changes`: the fields of the
/// command after its name).
pub fn check_combined(changes: &Document) -> Result<(), CommandError> {
    if changes.get(TIMESERIES).is_some() && changes.iter().count() > 1 {
        return Err(invalid(
            "Cannot combine timeseries option with other modification options in a single collMod command. Send separate collMod commands for timeseries and other options.",
        ));
    }
    Ok(())
}

/// The options of `c` after a `collMod` of its view definition or of its buckets (`changes`, with
/// `index` given or not), checked; `None` when it changes neither, and a refusal for what the kind
/// of `c` does not take.
pub fn definition<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
    changes: &Document,
    index_given: bool,
) -> Result<Option<Document>, CommandError> {
    let kind = kind_of(c)?;
    let time_series = matches!(kind, CollectionKind::TimeSeries(_));
    // Its type before what the collection is (measured on a `refrouter` 9.0.4 on 05-11-2026).
    if let Some(v) = changes.get(TIMESERIES).filter(|v| !matches!(v, Value::Document(_))) {
        return Err(wrong_type("collMod.timeseries", v, "object"));
    }
    // With authorization, a view's definition changes whole (measured the same day).
    let one = changes.get(VIEW_ON).is_some() != changes.get(PIPELINE).is_some();
    if one && backend.privileges().is_some() {
        return Err(invalid(
            "Must specify both 'viewOn' and 'pipeline' when modifying a view and auth is enabled",
        ));
    }
    if changes.get(TIMESERIES).is_some() && !time_series {
        return Err(invalid(format!(
            "option only supported on a time-series collection: {TIMESERIES}"
        )));
    }
    let stored = Document::from(RawDoc::from_bytes(&c.options)?);
    if let CollectionKind::View { .. } = kind {
        let other = changes.iter().map(|(k, _)| k).find(|k| ![VIEW_ON, PIPELINE].contains(k));
        if let Some(k) = other.or(index_given.then_some(INDEX)) {
            return Err(invalid(format!("option not supported on a view: {k}")));
        }
        let view = view(&stored, changes)?;
        super::kinds::check_view_definition(backend, ns, &view)?;
        return Ok(Some(view));
    }
    for k in [PIPELINE, VIEW_ON] {
        if changes.get(k).is_some() {
            return Err(invalid(format!("option only supported on a view: {k}")));
        }
    }
    // Checked above: a `timeseries` comes only for a time series.
    let (Some(asked), CollectionKind::TimeSeries(ts)) = (changes.get(TIMESERIES), kind) else {
        return Ok(None);
    };
    let Value::Document(asked) = asked else {
        return Err(wrong_type("collMod.timeseries", asked, "object"));
    };
    let old = Buckets::of(ts.granularity, ts.bucket_max_span_seconds, ts.bucket_rounding_seconds);
    let new = buckets(&old, asked)?;
    if new == old {
        return Ok(Some(stored));
    }
    let mut out = Document::new().with(super::kinds::TIME_FIELD, ts.time_field);
    if let Some(m) = ts.meta_field {
        out.push(super::kinds::META_FIELD, m);
    }
    match new.granularity {
        Some(g) => out.push(GRANULARITY, g.name()),
        None => out.push(ROUNDING, span(new.rounding)),
    }
    out.push(MAX_SPAN, span(new.span));
    out.push(FIXED_BUCKETING, false);
    let mut options = stored;
    options.insert(TIMESERIES, out);
    Ok(Some(options))
}

/// A view's options after `collMod` changes its `viewOn`, its `pipeline` or both.
fn view(stored: &Document, changes: &Document) -> Result<Document, CommandError> {
    let mut out = stored.clone();
    if let Some(v) = changes.get(VIEW_ON) {
        let Value::String(on) = v else { return Err(wrong_type("collMod.viewOn", v, "string")) };
        if on.is_empty() {
            return Err(bad("'viewOn' cannot be empty"));
        }
        out.insert(VIEW_ON, on.as_str());
    }
    if let Some(v) = changes.get(PIPELINE) {
        let Value::Array(stages) = v else {
            return Err(wrong_type("collMod.pipeline", v, "array"));
        };
        if let Some((i, s)) =
            stages.iter().enumerate().find(|(_, s)| !matches!(s, Value::Document(_)))
        {
            return Err(wrong_type(&format!("collMod.pipeline.{i}"), s, "object"));
        }
        out.insert(PIPELINE, v.clone());
    }
    Ok(out)
}

/// How a time series lays out its buckets: a granularity, or a span and rounding of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Buckets {
    granularity: Option<Granularity>,
    span: i64,
    rounding: i64,
}

impl Buckets {
    fn of(granularity: Option<Granularity>, span: i64, rounding: Option<i64>) -> Buckets {
        let rounding = match (rounding, granularity) {
            (Some(r), _) => r,
            (None, Some(g)) => super::timeseries::rounding_of(g),
            (None, None) => span,
        };
        Buckets { granularity, span, rounding }
    }
}

/// The buckets `asked` of a time series with `old`: a granularity never goes down and the span and
/// rounding never shrink.
fn buckets(old: &Buckets, asked: &Document) -> Result<Buckets, CommandError> {
    if let Some((k, _)) = asked.iter().find(|(k, _)| !TIMESERIES_FIELDS.contains(k)) {
        return Err(CommandError::new(
            IDL_UNKNOWN_FIELD,
            format!("BSON field 'collMod.timeseries.{k}' is an unknown field."),
        ));
    }
    let granularity = match asked.get(GRANULARITY) {
        None | Some(Value::Null) => None,
        Some(Value::String(g)) => Some(Granularity::of(g).ok_or_else(|| {
            bad(format!(
                "Enumeration value '{g}' for field 'collMod.timeseries.granularity' is not a valid value."
            ))
        })?),
        Some(v) => return Err(wrong_type("collMod.timeseries.granularity", v, "string")),
    };
    let number = |f: &str| -> Result<Option<i64>, CommandError> {
        let Some(v) = asked.get(f) else { return Ok(None) };
        let n = integral(v)
            .ok_or_else(|| wrong_types(&format!("collMod.timeseries.{f}"), v, &NUMBER))?;
        bounded(f, n, 1, MAX_BUCKET_SPAN, true)?;
        // Within a year of seconds: the bound above.
        Ok(Some(n as i64))
    };
    let (span, rounding) = (number(MAX_SPAN)?, number(ROUNDING)?);
    let new = match granularity {
        Some(g) => {
            if rounding.is_some() {
                return Err(invalid(
                    "Timeseries 'bucketRoundingSeconds' is not configurable alongside the granularity parameter",
                ));
            }
            if span.is_some_and(|s| s != g.max_span_seconds()) {
                return Err(invalid(
                    "Timeseries 'bucketMaxSpanSeconds' is not configurable to a value other than the default for the provided granularity",
                ));
            }
            if old.granularity.is_some_and(|o| o.max_span_seconds() > g.max_span_seconds()) {
                return Err(invalid(
                    "Invalid transition for timeseries.granularity. Can only transition from 'seconds' to 'minutes' or 'minutes' to 'hours'.",
                ));
            }
            Buckets::of(Some(g), g.max_span_seconds(), None)
        }
        None => match (span, rounding) {
            (None, None) => return Ok(*old),
            (Some(s), Some(r)) if s == r => Buckets::of(None, s, Some(r)),
            (Some(_), Some(_)) => {
                return Err(invalid(
                    "Timeseries 'bucketRoundingSeconds' needs to be equal to 'bucketMaxSpanSeconds'",
                ));
            }
            _ => {
                return Err(invalid(
                    "Timeseries 'bucketMaxSpanSeconds' and 'bucketRoundingSeconds' need to be set alongside each other",
                ));
            }
        },
    };
    if new.span < old.span {
        return Err(invalid(
            "Timeseries 'bucketMaxSpanSeconds' needs to be equal or greater to transition",
        ));
    }
    if new.rounding < old.rounding {
        return Err(invalid(
            "Timeseries 'bucketRoundingSeconds' needs to be equal or greater to transition",
        ));
    }
    Ok(new)
}

/// A bucket span, at most a year in seconds: the bound checked before.
fn span(n: i64) -> i32 {
    n as i32
}

/// The options of `c` after `collMod` sets its `expireAfterSeconds` to `v` at `now_secs`: off with
/// `"off"`, before `timeseries` in a time series and in its place or last in a clustered
/// collection.
pub fn expiry(
    c: &CollectionEntry,
    mut options: Document,
    v: &Value,
    now_secs: i64,
) -> Result<Document, CommandError> {
    let time_series = match kind_of(c)? {
        CollectionKind::TimeSeries(_) => true,
        CollectionKind::Clustered { .. } => false,
        _ => {
            return Err(invalid(
                "'expireAfterSeconds' option is only supported on collections clustered by _id",
            ));
        }
    };
    let seconds = match v {
        Value::String(s) if s == OFF => None,
        Value::String(s) => {
            return Err(invalid(format!(
                "Invalid string value for the 'clusteredIndex::expireAfterSeconds' option. Got: '{s}'. Accepted value is '{OFF}'"
            )));
        }
        v => {
            let n = integral(v)
                .ok_or_else(|| wrong_types("collMod.expireAfterSeconds", v, &["string", "long"]))?;
            checked_expire(Some(n), "", now_secs)?
        }
    };
    let Some(seconds) = seconds else {
        options.remove(EXPIRE);
        return Ok(options);
    };
    if options.get(EXPIRE).is_some() || !time_series {
        options.insert(EXPIRE, seconds);
        return Ok(options);
    }
    let mut out = Document::new().with(EXPIRE, seconds);
    for (k, v) in options.iter() {
        out.push(k, v.clone());
    }
    Ok(out)
}

/// The field of `collMod` that resizes a capped collection.
pub const CAPPED_SIZE: &str = "cappedSize";
/// The field of `collMod` that changes a capped collection's most documents.
pub const CAPPED_MAX: &str = "cappedMax";
/// The types of `cappedSize` and `cappedMax`, in RefDb's order.
const CAPPED_LIMIT_TYPES: [&str; 4] = ["double", "decimal", "int", "long"];
/// The largest `cappedSize`: 1 PiB.
const MAX_CAPPED_SIZE: i128 = 1 << 50;
/// The bound `cappedMax` stays under.
const CAPPED_MAX_BOUND: i128 = 1 << 31;
/// What a `cappedMax` of zero or less is kept as: no most.
const NO_CAPPED_MAX: i64 = i32::MAX as i64;

/// The new limits a `collMod` gives a capped collection (015 FR-003).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CappedLimits {
    size: Option<i64>,
    max: Option<i64>,
}

/// Takes `cappedSize` and `cappedMax` out of `changes`, checked as a `refrouter` 9.0.4 checks them
/// before it looks at the collection (measured on 05-11-2026): numbers, a size from 1 to 1 PiB
/// (a `double` truncated, a `decimal` rounded half to even) and a most under 2^31, where zero or
/// less means none.
pub fn capped_limits(changes: &mut Document) -> Result<Option<CappedLimits>, CommandError> {
    let mut limits = CappedLimits::default();
    for field in [CAPPED_SIZE, CAPPED_MAX] {
        let Some(v) = changes.remove(field) else { continue };
        let numeric = matches!(
            v,
            Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_)
        );
        let n = super::kinds::integral(&v)
            .filter(|_| numeric)
            .ok_or_else(|| wrong_types(&format!("collMod.{field}"), &v, &CAPPED_LIMIT_TYPES))?;
        let refused = |rule: &str| {
            CommandError::new(
                BAD_VALUE,
                format!("BSON field '{field}' value must be {rule}, actual value '{n}'"),
            )
        };
        if field == CAPPED_SIZE {
            if n < 1 {
                return Err(refused(">= 1"));
            }
            if n > MAX_CAPPED_SIZE {
                return Err(refused(&format!("<= {MAX_CAPPED_SIZE}")));
            }
            limits.size = Some(n as i64);
        } else {
            if n >= CAPPED_MAX_BOUND {
                return Err(refused(&format!("< {CAPPED_MAX_BOUND}")));
            }
            limits.max = Some(if n <= 0 { NO_CAPPED_MAX } else { n as i64 });
        }
    }
    Ok((limits != CappedLimits::default()).then_some(limits))
}

/// Options `options` of collection `c` with the new limits of a `collMod`: the collection must be
/// capped (72 «Collection must be capped.», measured).
pub fn resize(
    c: &CollectionEntry,
    options: &mut Document,
    limits: CappedLimits,
) -> Result<(), CommandError> {
    if !matches!(kind_of(c)?, CollectionKind::Capped { .. }) {
        return Err(invalid("Collection must be capped."));
    }
    if let Some(size) = limits.size {
        options.insert(super::kinds::CAPPED_SIZE_OPTION, size);
    }
    if let Some(max) = limits.max {
        options.insert(super::kinds::CAPPED_MAX_OPTION, max);
    }
    Ok(())
}
