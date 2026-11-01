//! Reads of a time series (015 FR-007, T013): a filter's conditions on the time field and on the
//! meta value become bounds of the buckets ([`Bounds`]), checked against each bucket's
//! `control.min`, `control.max` and `meta` before it is unpacked; the measurements of the buckets
//! that may match are then filtered one by one by the caller, so the bounds only skip work.
//!
//! The bounds come from the filter's top level and its `$and`s: `{t: {$gt|$gte|$lt|$lte|$eq: d}}`
//! or `{t: d}` with a date `d`, and `{m: v}` with a value `v` that is no document or array (their
//! equality has rules of its own) and no regular expression.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, RawValue, Value, compare};
use teseladb_mql::timeseries_write::{BucketSpec, CONTROL, MAX, META, MIN, TsError};

/// What a filter says of the buckets it may match.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bounds {
    /// Least time a matching measurement may have, in milliseconds.
    pub after: Option<i64>,
    /// Greatest time a matching measurement may have, in milliseconds.
    pub before: Option<i64>,
    /// The meta value a matching measurement has.
    pub meta: Option<Value>,
}

impl Bounds {
    /// The bounds of `filter` over time series `spec`.
    pub fn of(spec: &BucketSpec, filter: &Document) -> Bounds {
        let mut b = Bounds::default();
        b.add(spec, filter);
        b
    }

    fn add(&mut self, spec: &BucketSpec, filter: &Document) {
        for (k, v) in filter.iter() {
            match (k, v) {
                ("$and", Value::Array(items)) => {
                    let docs = items.iter().filter_map(|i| match i {
                        Value::Document(d) => Some(d),
                        _ => None,
                    });
                    for d in docs {
                        self.add(spec, d);
                    }
                }
                (k, v) if k == spec.time_field => self.time(v),
                (k, v)
                    if spec.meta_field.as_deref() == Some(k)
                        && !matches!(
                            v,
                            Value::Document(_) | Value::Array(_) | Value::Regex { .. }
                        ) =>
                {
                    self.meta = Some(v.clone());
                }
                _ => {}
            }
        }
    }

    /// A condition on the time field.
    fn time(&mut self, v: &Value) {
        match v {
            Value::DateTime(t) => self.narrow(Some(*t), Some(*t)),
            Value::Document(ops) => {
                for (op, x) in ops.iter() {
                    let Value::DateTime(t) = x else { continue };
                    match op {
                        "$eq" => self.narrow(Some(*t), Some(*t)),
                        // Dates are whole milliseconds: a strict bound is the next one.
                        "$gt" => self.narrow(Some(t.saturating_add(1)), None),
                        "$gte" => self.narrow(Some(*t), None),
                        "$lt" => self.narrow(None, Some(t.saturating_sub(1))),
                        "$lte" => self.narrow(None, Some(*t)),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn narrow(&mut self, after: Option<i64>, before: Option<i64>) {
        if let Some(a) = after {
            self.after = Some(self.after.map_or(a, |x| x.max(a)));
        }
        if let Some(b) = before {
            self.before = Some(self.before.map_or(b, |x| x.min(b)));
        }
    }

    /// Whether bucket `bucket` may hold a measurement within the bounds.
    pub fn may_match(&self, spec: &BucketSpec, bucket: RawDoc<'_>) -> Result<bool, TsError> {
        let control = match bucket.get(CONTROL) {
            Some(RawValue::Document(c)) => c,
            _ => return Err(TsError::Malformed("a bucket without control".into())),
        };
        let time = |side: &str| match control.get(side) {
            Some(RawValue::Document(d)) => match d.get(&spec.time_field) {
                Some(RawValue::DateTime(t)) => Ok(t),
                _ => Err(TsError::Malformed(format!("control.{side} without the time"))),
            },
            _ => Err(TsError::Malformed(format!("a bucket without control.{side}"))),
        };
        let (lo, hi) = (time(MIN)?, time(MAX)?);
        if self.after.is_some_and(|a| a > hi) || self.before.is_some_and(|b| b < lo) {
            return Ok(false);
        }
        let Some(want) = &self.meta else { return Ok(true) };
        let holder = Document::new().with("v", want.clone()).to_bytes().map_err(malformed)?;
        let want = RawDoc::from_bytes(&holder).map_err(malformed)?;
        Ok(match (bucket.get(META), want.get("v")) {
            (Some(have), Some(want)) => compare(have, want, None) == Ordering::Equal,
            _ => false,
        })
    }
}

/// A meta bound that cannot be written as BSON (past its depth) cannot be compared.
fn malformed(e: teseladb_bson::BsonError) -> TsError {
    TsError::Malformed(e.to_string())
}
