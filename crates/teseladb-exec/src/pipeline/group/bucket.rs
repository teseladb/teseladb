//! `$bucket` and `$bucketAuto`, each taking its input a batch at a time as the stages before it
//! give it (010 FR-028), with the memory and the store of the `$group` and the `$sort` they
//! stand for (010 FR-010).

use teseladb_bson::{Document, RawValue, Value};

use super::super::Env;
use super::super::errors::{
    self, BUCKET_AUTO_BUCKETS, BUCKET_AUTO_INT, BUCKET_AUTO_NUMERIC, BUCKET_AUTO_OUTPUT,
    BUCKET_AUTO_REQUIRED, BUCKET_BOUNDARIES_ARRAY, BUCKET_DEFAULT, BUCKET_FEW, BUCKET_GROUP_BY,
    BUCKET_NO_MATCH, BUCKET_NO_MATCH_CLASSIC, BUCKET_OUTPUT, BUCKET_REQUIRED, BUCKET_TYPES,
    BUCKET_UNSORTED, UNKNOWN_FIELD, error,
};
use super::super::granularity::Granularity;
use super::super::spill::{Sorter, key_of, with_key, without_key};
use super::{
    Outputs, canonical_type, check_memory, equal, group_scope, groups_from_sorter, held_by,
    key_sorter, less, limits, read_outputs,
};
use crate::accum::{Accumulator, State};
use crate::expr::arith::type_name_of;
use crate::expr::values::display;
use crate::expr::{Ctx, Expr, Scope, order};
use teseladb_mql::MqlError;

/// A `$bucket`.
#[derive(Clone, Debug)]
pub struct Bucket {
    group_by: Expr,
    boundaries: Vec<Value>,
    default: Option<Value>,
    outputs: Outputs,
}

/// Reads a `groupBy`: a `$` path or an expression object.
fn group_by(v: RawValue<'_>, stage: &str, ctx: &Ctx) -> Result<Expr, MqlError> {
    match v {
        RawValue::String(s) if s.starts_with('$') => Expr::parse_in(v, ctx),
        RawValue::Document(_) => Expr::parse_in(v, ctx),
        other => Err(error(
            BUCKET_GROUP_BY,
            format!(
                "The {stage} 'groupBy' field must be defined as a $-prefixed path or an expression, but found: {}.",
                display(&Value::from(other))
            ),
        )),
    }
}

/// The accumulators of `output`, or `count: {$sum: 1}` without one. `not_object` builds the stage's
/// own error for an `output` that is not a document, from the type name of what it found.
fn count_output(
    output: Option<RawValue<'_>>,
    ctx: &Ctx,
    not_object: impl FnOnce(&str) -> MqlError,
) -> Result<Outputs, MqlError> {
    match output {
        Some(RawValue::Document(d)) => read_outputs(d, "", ctx),
        Some(other) => Err(not_object(crate::expr::arith::raw_type_name(other))),
        None => {
            let spec = Document::new().with("$sum", 1);
            let count = crate::raw::with_raw_doc(&spec, |raw| {
                Accumulator::parse("count", RawValue::Document(raw), ctx)
            })?;
            Ok(vec![("count".to_owned(), count)])
        }
    }
}

impl Bucket {
    /// Reads the specification of a `$bucket`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Bucket, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                errors::BUCKET_NOT_OBJECT,
                format!(
                    "Argument to $bucket stage must be an object, but found type: {}.",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut gb, mut bounds, mut default, mut output) = (None, None, None, None);
        for (k, x) in d.iter() {
            match k {
                "groupBy" => gb = Some(group_by(x, "$bucket", ctx)?),
                "boundaries" => bounds = Some(x),
                "default" => default = Some(Value::from(x)),
                "output" => output = Some(x),
                other => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("Unrecognized option to $bucket: {other}."),
                    ));
                }
            }
        }
        let required = || {
            error(BUCKET_REQUIRED, "$bucket requires 'groupBy' and 'boundaries' to be specified.")
        };
        let group_by = gb.ok_or_else(required)?;
        let boundaries: Vec<Value> = match bounds {
            Some(RawValue::Array(a)) => a.iter().map(Value::from).collect(),
            Some(other) => {
                return Err(error(
                    BUCKET_BOUNDARIES_ARRAY,
                    format!(
                        "The $bucket 'boundaries' field must be an array, but found type: {}.",
                        crate::expr::arith::raw_type_name(other)
                    ),
                ));
            }
            None => return Err(required()),
        };
        if boundaries.len() < 2 {
            return Err(error(
                BUCKET_FEW,
                format!(
                    "The $bucket 'boundaries' field must have at least 2 values, but found {} value(s).",
                    boundaries.len()
                ),
            ));
        }
        for (i, w) in boundaries.windows(2).enumerate() {
            let (a, b) = (canonical_type(&w[0]), canonical_type(&w[1]));
            if a != b {
                return Err(error(
                    BUCKET_TYPES,
                    format!(
                        "All values in the the 'boundaries' option to $bucket must have the same type. Found conflicting types {} and {}.",
                        type_name_of(&w[0]),
                        type_name_of(&w[1])
                    ),
                ));
            }
            if order(&Some(w[0].clone()), &Some(w[1].clone()), None)? != std::cmp::Ordering::Less {
                return Err(error(
                    BUCKET_UNSORTED,
                    format!(
                        "The 'boundaries' option to $bucket must be sorted, but elements {i} and {} are not in ascending order ({} is not less than {}).",
                        i + 1,
                        display(&w[0]),
                        display(&w[1])
                    ),
                ));
            }
        }
        if let Some(def) = &default
            && canonical_type(def) == canonical_type(&boundaries[0])
        {
            let low = order(&Some(def.clone()), &Some(boundaries[0].clone()), None)?;
            let high =
                order(&Some(def.clone()), &Some(boundaries[boundaries.len() - 1].clone()), None)?;
            if low != std::cmp::Ordering::Less && high == std::cmp::Ordering::Less {
                return Err(error(
                    BUCKET_DEFAULT,
                    "The $bucket 'default' field must be less than the lowest boundary or greater than or equal to the highest boundary.",
                ));
            }
        }
        let outputs = count_output(output, ctx, |found| {
            error(
                BUCKET_OUTPUT,
                format!("The $bucket 'output' field must be an object, but found type: {found}."),
            )
        })?;
        Ok(Bucket { group_by, boundaries, default, outputs })
    }

    /// The buckets of `docs`, in boundary order, the default last. Past the stage's memory it
    /// fails with 292 without `allowDiskUse` and builds its buckets through the node's store
    /// with it, as the `$group` it stands for (010 FR-010).
    pub fn run(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        let mut run = self.start(env, sbe);
        run.absorb(docs)?;
        run.finish()
    }

    /// A run of the stage that takes its input a batch at a time ([`Bucketing::absorb`]).
    pub(in crate::pipeline) fn start<'g>(&'g self, env: &'g dyn Env, sbe: bool) -> Bucketing<'g> {
        Bucketing {
            bucket: self,
            env,
            sbe,
            states: vec![None; self.boundaries.len()],
            held: 0,
            kept: Vec::new(),
            sorter: None,
        }
    }

    /// The bucket of `doc`: its boundary's place, or one past the last for the default.
    fn slot(
        &self,
        doc: &Document,
        scope: &Scope,
        col: Option<&dyn teseladb_bson::Collator>,
        sbe: bool,
    ) -> Result<usize, MqlError> {
        let n = self.boundaries.len() - 1;
        let v = self.group_by.eval(doc, scope)?.unwrap_or(Value::Null);
        if canonical_type(&v) == canonical_type(&self.boundaries[0]) {
            for i in 0..n {
                let lo = order(&Some(self.boundaries[i].clone()), &Some(v.clone()), col)?;
                let hi = order(&Some(v.clone()), &Some(self.boundaries[i + 1].clone()), col)?;
                if lo != std::cmp::Ordering::Greater && hi == std::cmp::Ordering::Less {
                    return Ok(i);
                }
            }
        }
        match &self.default {
            Some(_) => Ok(n),
            // The slot-based engine and the classic one fail with codes of their own.
            None => Err(error(
                if sbe { BUCKET_NO_MATCH } else { BUCKET_NO_MATCH_CLASSIC },
                "$switch could not find a matching branch for an input, and no default was specified.",
            )),
        }
    }
}

/// A `$bucketAuto`.
#[derive(Clone, Debug)]
pub struct BucketAuto {
    group_by: Expr,
    buckets: usize,
    outputs: Outputs,
    granularity: Option<Granularity>,
}

impl BucketAuto {
    /// Reads the specification of a `$bucketAuto`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<BucketAuto, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                errors::BUCKET_AUTO_NOT_OBJECT,
                format!(
                    "The argument to $bucketAuto must be an object, but found type: {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut gb, mut buckets, mut output, mut granularity) = (None, None, None, None);
        for (k, x) in d.iter() {
            match k {
                "groupBy" => gb = Some(group_by(x, "$bucketAuto", ctx)?),
                "buckets" => buckets = Some(Value::from(x)),
                "output" => output = Some(x),
                "granularity" => {
                    let name = match x {
                        RawValue::String(s) => s.to_owned(),
                        other => display(&Value::from(other)),
                    };
                    granularity = Some(Granularity::named(&name)?);
                }
                other => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("Unrecognized option to $bucketAuto: {other}."),
                    ));
                }
            }
        }
        let (Some(group_by), Some(buckets)) = (gb, buckets) else {
            return Err(error(
                BUCKET_AUTO_REQUIRED,
                "$bucketAuto requires 'groupBy' and 'buckets' to be specified",
            ));
        };
        let n = bucket_count(&buckets)?;
        let outputs = count_output(output, ctx, |found| {
            error(
                BUCKET_AUTO_OUTPUT,
                format!(
                    "The $bucketAuto 'output' field must be an object, but found type: {found}"
                ),
            )
        })?;
        Ok(BucketAuto { group_by, buckets: n, outputs, granularity })
    }

    /// Buckets of about the same number of documents over the sorted values of `groupBy`; equal
    /// values stay in one bucket, and a bucket's `max` is the next bucket's `min` (the last
    /// value for the last bucket).
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let mut run = self.start(env);
        run.absorb(docs)?;
        run.finish()
    }

    /// A run of the stage that takes its input a batch at a time ([`BucketAutoRun::absorb`]).
    pub(in crate::pipeline) fn start<'g>(&'g self, env: &'g dyn Env) -> BucketAutoRun<'g> {
        BucketAutoRun { auto: self, env, keyed: Vec::new(), held: 0, sorter: None }
    }

    /// The buckets of `keyed`, the documents with their values in the order of the values.
    fn buckets_of(
        &self,
        keyed: Vec<(Value, Document)>,
        env: &dyn Env,
    ) -> Result<Vec<Document>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let total = keyed.len();
        let size = ((total as f64) / (self.buckets as f64)).round().max(1.0) as usize;
        // Each bucket: its range of `keyed` and its upper boundary.
        let mut ranges: Vec<(usize, usize, Value)> = Vec::new();
        let mut start = 0;
        while start < total {
            let mut end = (start + size).min(total);
            if ranges.len() + 1 == self.buckets {
                end = total;
            }
            let max = match self.granularity {
                Some(g) => {
                    let boundary = g.round(&keyed[end - 1].0, true)?;
                    while end < total && less(&keyed[end].0, &boundary, col)? {
                        end += 1;
                    }
                    boundary
                }
                None => {
                    while end < total && equal(&keyed[end].0, &keyed[end - 1].0, col)? {
                        end += 1;
                    }
                    match keyed.get(end) {
                        Some((v, _)) => v.clone(),
                        None => keyed[end - 1].0.clone(),
                    }
                }
            };
            ranges.push((start, end, max));
            start = end;
        }
        let mut out = Vec::with_capacity(ranges.len());
        let mut previous: Option<Value> = None;
        for (s, e, max) in ranges {
            let min = match (previous.take(), self.granularity) {
                (Some(p), Some(_)) => p,
                (None, Some(g)) => g.round(&keyed[s].0, false)?,
                (_, None) => keyed[s].0.clone(),
            };
            previous = Some(max.clone());
            let mut states: Vec<State> = self.outputs.iter().map(|(_, a)| a.start()).collect();
            for (_, doc) in &keyed[s..e] {
                let scope = group_scope(env, doc, false);
                for ((_, acc), st) in self.outputs.iter().zip(states.iter_mut()) {
                    acc.add(st, doc, &scope)?;
                }
            }
            let mut d =
                Document::new().with("_id", Document::new().with("min", min).with("max", max));
            for ((name, acc), st) in self.outputs.iter().zip(states) {
                d.push(name.clone(), acc.finish(st, col, false)?);
            }
            out.push(d);
        }
        Ok(out)
    }
}

/// The `buckets` of `$bucketAuto`: a positive 32-bit integer.
fn bucket_count(v: &Value) -> Result<usize, MqlError> {
    if crate::expr::values::f64_of(v).is_none() {
        return Err(error(
            BUCKET_AUTO_NUMERIC,
            format!(
                "The $bucketAuto 'buckets' field must be a numeric value, but found type: {}",
                type_name_of(v)
            ),
        ));
    }
    let n = crate::expr::values::int64_of(v)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| {
            error(
                BUCKET_AUTO_INT,
                format!(
                    "The $bucketAuto 'buckets' field must be representable as a 32-bit integer, but found {}",
                    display(v)
                ),
            )
        })?;
    if n <= 0 {
        return Err(error(
            BUCKET_AUTO_BUCKETS,
            format!("The $bucketAuto 'buckets' field must be greater than 0, but found: {n}"),
        ));
    }
    Ok(usize::try_from(n).unwrap_or(0))
}

/// A `$bucket` taking its input a batch at a time: the state of each bucket and the bytes their
/// values hold; with a store to write to, the documents so far with their bucket, which go
/// through the store sorted by bucket once the buckets pass the stage's limit.
pub(in crate::pipeline) struct Bucketing<'g> {
    bucket: &'g Bucket,
    env: &'g dyn Env,
    sbe: bool,
    /// The states of each bucket, the default last.
    states: Vec<Option<Vec<State>>>,
    held: usize,
    kept: Vec<(usize, Document)>,
    /// The sort by bucket, once the buckets passed the limit.
    sorter: Option<Sorter<'g, (Vec<u8>, Value)>>,
}

/// The key that carries a document's bucket through the store.
fn slot_key(i: usize) -> Value {
    Value::Int64(i64::try_from(i).unwrap_or(i64::MAX))
}

impl Bucketing<'_> {
    /// Takes the documents of `batch`, in their order, into their buckets.
    pub(in crate::pipeline) fn absorb(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        let (env, sbe, bucket) = (self.env, self.sbe, self.bucket);
        let (limit, store) = limits(env);
        let collation = env.collation();
        for doc in batch {
            let scope = group_scope(env, &doc, sbe);
            let i = bucket.slot(&doc, &scope, collation.get(), sbe)?;
            if let Some(sorter) = &mut self.sorter {
                sorter.push(with_key(doc, slot_key(i)))?;
                continue;
            }
            let st = self.states[i]
                .get_or_insert_with(|| bucket.outputs.iter().map(|(_, a)| a.start()).collect());
            for ((_, acc), s) in bucket.outputs.iter().zip(st.iter_mut()) {
                acc.add(s, &doc, &scope)?;
                self.held += held_by(acc, s);
            }
            let Some(store) = store else { continue };
            self.kept.push((i, doc));
            if self.held > limit {
                let mut sorter = key_sorter(env.collation(), (limit, store));
                for (i, d) in self.kept.drain(..) {
                    sorter.push(with_key(d, slot_key(i)))?;
                }
                self.states.iter_mut().for_each(|s| *s = None);
                self.sorter = Some(sorter);
            }
        }
        Ok(())
    }

    /// The documents of the buckets.
    pub(in crate::pipeline) fn finish(self) -> Result<Vec<Document>, MqlError> {
        let (env, sbe, bucket) = (self.env, self.sbe, self.bucket);
        let (limit, _) = limits(env);
        let n = bucket.boundaries.len() - 1;
        let filled: Vec<(usize, Vec<State>)> = match self.sorter {
            Some(sorter) => groups_from_sorter(&bucket.outputs, sorter, (env, sbe))?
                .into_iter()
                .map(|(k, st)| match k {
                    Value::Int64(i) => (usize::try_from(i).unwrap_or(n), st),
                    _ => (n, st),
                })
                .collect(),
            None => {
                self.states.into_iter().enumerate().filter_map(|(i, st)| Some((i, st?))).collect()
            }
        };
        let collation = env.collation();
        let col = collation.get();
        let mut out = Vec::new();
        for (i, st) in filled {
            let id = match i < n {
                true => bucket.boundaries[i].clone(),
                false => bucket.default.clone().unwrap_or(Value::Null),
            };
            let mut d = Document::new().with("_id", id);
            for ((name, acc), s) in bucket.outputs.iter().zip(st) {
                d.push(name.clone(), acc.finish(s, col, sbe)?);
            }
            out.push(d);
        }
        check_memory(&bucket.outputs, &out, env.allow_disk_use(), sbe, limit)?;
        Ok(out)
    }
}

/// A `$bucketAuto` taking its input a batch at a time: the documents so far with their values;
/// past the stage's limit they fail with 292 without `allowDiskUse` and go through the store
/// sorted by value with it, as the sort the stage runs.
pub(in crate::pipeline) struct BucketAutoRun<'g> {
    auto: &'g BucketAuto,
    env: &'g dyn Env,
    keyed: Vec<(Value, Document)>,
    held: usize,
    /// The sort by value, once the documents passed the limit.
    sorter: Option<Sorter<'g, Value>>,
}

impl BucketAutoRun<'_> {
    /// Takes the documents of `batch` with their values, in their order.
    pub(in crate::pipeline) fn absorb(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        let (env, auto) = (self.env, self.auto);
        let limit = env.stage_memory_limit();
        for doc in batch {
            let scope = group_scope(env, &doc, false);
            let v = auto.group_by.eval(&doc, &scope)?.unwrap_or(Value::Null);
            // The values are checked in the order the documents come.
            if auto.granularity.is_some() {
                Granularity::check(&v)?;
            }
            if let Some(sorter) = &mut self.sorter {
                sorter.push(with_key(doc, v))?;
                continue;
            }
            self.held += errors::document_size(&doc);
            self.keyed.push((v, doc));
            if self.held <= limit {
                continue;
            }
            if !env.allow_disk_use() {
                return Err(errors::sort_memory_error(limit));
            }
            if let Some(store) = env.spill() {
                let collation = env.collation();
                let mut sorter = Sorter::new(
                    Box::new(|w: &Document| Ok(key_of(w))),
                    Box::new(move |a: &Value, b: &Value| {
                        order(&Some(a.clone()), &Some(b.clone()), collation.get())
                    }),
                    (limit, store),
                );
                for (v, d) in self.keyed.drain(..) {
                    sorter.push(with_key(d, v))?;
                }
                self.sorter = Some(sorter);
            }
        }
        Ok(())
    }

    /// The buckets of the documents.
    pub(in crate::pipeline) fn finish(mut self) -> Result<Vec<Document>, MqlError> {
        let env = self.env;
        let keyed = match self.sorter.take() {
            Some(sorter) => {
                let mut out = Vec::new();
                sorter.finish(|v, w| {
                    out.push((v, without_key(w)));
                    Ok(())
                })?;
                out
            }
            None => {
                let collation = env.collation();
                let mut keyed = std::mem::take(&mut self.keyed);
                crate::expr::sort_by_values(&mut keyed, collation.get())?;
                keyed
            }
        };
        self.auto.buckets_of(keyed, env)
    }
}
