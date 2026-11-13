//! The stages that group documents (measured on a `refrouter` 9.0.4): `$group`, `$bucket`,
//! `$bucketAuto` and `$sortByCount`. Groups are keyed by values that compare equal (`1` and
//! `1.0` are one group, a missing `_id` is `null`) and come out in the order their first document
//! came; a `$group` that wrote to disk gives them in the order of their keys.

use std::collections::HashMap;

use teseladb_mql::MqlError;
use teseladb_bson::{Document, RawValue, Value};

use super::Env;
use super::errors::{
    BUCKET_AUTO_BUCKETS, BUCKET_AUTO_INT, BUCKET_AUTO_NUMERIC, BUCKET_AUTO_OUTPUT,
    BUCKET_AUTO_REQUIRED, BUCKET_BOUNDARIES_ARRAY, BUCKET_DEFAULT, BUCKET_FEW, BUCKET_GROUP_BY,
    BUCKET_NO_MATCH, BUCKET_NO_MATCH_CLASSIC, BUCKET_OUTPUT, BUCKET_REQUIRED, BUCKET_TYPES,
    BUCKET_UNSORTED, GROUP_DOLLAR, GROUP_DOT, GROUP_ID, SORT_BY_COUNT_FIELD, SORT_BY_COUNT_OBJECT,
    UNKNOWN_FIELD, error,
};
use super::granularity::Granularity;
use crate::accum::{Accumulator, State};
use crate::expr::arith::type_name_of;
use crate::expr::values::display;
use crate::expr::{Ctx, Expr, Scope, order};

/// The scope of an expression of a group over `doc`, `sbe` when RefDb evaluates it with its
/// slot-based engine.
fn group_scope(env: &dyn Env, doc: &Document, sbe: bool) -> Scope {
    let mut s = env.scope(doc);
    s.sbe = sbe;
    s
}

/// The groups of `keyed` (each document with its key) with accumulators `outputs`, sorted by
/// key through `store` in runs of at most `limit` bytes and built one at a time, in the order of
/// their keys; each keeps the key of its first document (010 FR-010, T024).
fn groups_on_disk(
    outputs: &Outputs,
    keyed: Vec<(Value, Document)>,
    (env, sbe): (&dyn Env, bool),
    (limit, store): (usize, &dyn super::spill::SpillStore),
) -> Result<Vec<(Value, Vec<State>)>, MqlError> {
    let collation = env.collation();
    let col = collation.get();
    let wrapped = keyed.into_iter().map(|(k, d)| super::spill::with_key(d, k));
    // Each group with its canonical key, in the order of their keys.
    let mut out: Vec<(Vec<u8>, Value, Vec<State>)> = Vec::new();
    super::spill::external_sort(
        wrapped.collect(),
        |w| {
            let k = super::spill::key_of(w);
            Ok((canonical(&k, col), k))
        },
        // Keys of one group compare equal even where the collation still tells them apart
        // (`a` and `A` at strength 2), so that the group keeps its documents' order.
        |a: &(Vec<u8>, Value), b| match a.0 == b.0 {
            true => Ok(std::cmp::Ordering::Equal),
            false => order(&Some(a.1.clone()), &Some(b.1.clone()), col),
        },
        (limit, store),
        |(canon, key), w| {
            let doc = super::spill::without_key(w);
            let scope = group_scope(env, &doc, sbe);
            let states = match out.last_mut() {
                Some((c, _, states)) if *c == canon => states,
                _ => {
                    out.push((canon, key, outputs.iter().map(|(_, a)| a.start()).collect()));
                    let last = out.len() - 1;
                    &mut out[last].2
                }
            };
            for ((_, acc), st) in outputs.iter().zip(states.iter_mut()) {
                acc.add(st, &doc, &scope)?;
            }
            Ok(())
        },
    )?;
    Ok(out.into_iter().map(|(_, k, states)| (k, states)).collect())
}

/// The bytes a group holds for the value accumulator `acc` just took into `st`, when it collects
/// values (`$push`, `$addToSet`).
fn held_by(acc: &Accumulator, st: &State) -> usize {
    match st {
        State::Many(items) if acc.collects() => items.last().map_or(0, super::errors::value_size),
        _ => 0,
    }
}

/// The accumulators of a group's fields, in the specification's order.
type Outputs = Vec<(String, Accumulator)>;

/// A `$group`.
#[derive(Clone, Debug)]
pub struct Group {
    id: Expr,
    outputs: Outputs,
}

/// Reads output fields (`$group`'s, `$bucket`'s `output`) from `d`, skipping `skip`.
fn read_outputs(d: teseladb_bson::RawDoc<'_>, skip: &str, ctx: &Ctx) -> Result<Outputs, MqlError> {
    let mut out = Vec::new();
    for (k, v) in d.iter() {
        if k == skip {
            continue;
        }
        if k.starts_with('$') {
            return Err(error(
                GROUP_DOLLAR,
                format!("The field name '{k}' cannot be an operator name"),
            ));
        }
        if k.contains('.') {
            return Err(error(GROUP_DOT, format!("The field name '{k}' cannot contain '.'")));
        }
        out.push((k.to_owned(), Accumulator::parse(k, v, ctx)?));
    }
    Ok(out)
}

impl Group {
    /// Reads the specification of a `$group`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<Group, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                super::errors::GROUP_SPEC_NOT_OBJECT,
                "a group's fields must be specified in an object",
            ));
        };
        let id = d
            .get("_id")
            .ok_or_else(|| error(GROUP_ID, "a group specification must include an _id"))?;
        Ok(Group { id: Expr::parse_in(id, ctx)?, outputs: read_outputs(d, "_id", ctx)? })
    }

    /// The groups of `docs`. With `allowDiskUse` and a store, a group whose keys and collected
    /// values pass the stage's limit sorts its documents by key through the store and builds
    /// one group at a time, in the order of their keys (010 FR-010, T024).
    pub fn run(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        let mut grouping = self.start(env, sbe);
        grouping.absorb(docs)?;
        grouping.finish()
    }

    /// A run of the stage that takes its input a batch at a time ([`Grouping::absorb`]), as the
    /// stages before it give it (010 FR-028).
    pub(super) fn start<'g>(&'g self, env: &'g dyn Env, sbe: bool) -> Grouping<'g> {
        Grouping {
            group: self,
            env,
            sbe,
            groups: Groups::default(),
            held: 0,
            plan: super::group_columns::columnar(&self.id, &self.outputs),
            spill: false,
            kept: Vec::new(),
        }
    }

    /// The groups of `docs` sorted by key through `store` in runs of at most `limit` bytes,
    /// built one at a time.
    fn run_sorted(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
        disk: (usize, &dyn super::spill::SpillStore),
    ) -> Result<Vec<Document>, MqlError> {
        let mut keyed = Vec::with_capacity(docs.len());
        for doc in docs {
            let key = self.id.eval(&doc, &group_scope(env, &doc, sbe))?.unwrap_or(Value::Null);
            keyed.push((key, doc));
        }
        let col = env.collation();
        groups_on_disk(&self.outputs, keyed, (env, sbe), disk)?
            .into_iter()
            .map(|(k, states)| self.finished(k, states, col.get(), sbe))
            .collect()
    }

    /// The document of the group of key `key` with states `states`.
    fn finished(
        &self,
        key: Value,
        states: Vec<State>,
        col: Option<&dyn teseladb_bson::Collator>,
        sbe: bool,
    ) -> Result<Document, MqlError> {
        let mut d = Document::new().with("_id", key);
        for ((name, acc), st) in self.outputs.iter().zip(states) {
            d.push(name.clone(), acc.finish(st, col, sbe)?);
        }
        Ok(d)
    }
}

/// A `$group` taking its input a batch at a time (010 FR-028): the groups so far and the bytes
/// they hold; with a store, the documents so far, which go through the store sorted by key if the
/// groups pass the stage's limit.
pub(super) struct Grouping<'g> {
    group: &'g Group,
    env: &'g dyn Env,
    sbe: bool,
    groups: Groups,
    held: usize,
    plan: Option<super::group_columns::Columnar>,
    /// The groups passed the limit: the rest only waits for the sort through the store.
    spill: bool,
    /// The input so far, kept only with a store to write to.
    kept: Vec<Document>,
}

/// The limit of a group's memory in `env`, and the store it writes to with `allowDiskUse`.
fn limits(env: &dyn Env) -> (usize, Option<&dyn super::spill::SpillStore>) {
    let limit = env.stage_memory_limit().min(env.operation_memory_limit());
    (limit, env.spill().filter(|_| env.allow_disk_use()))
}

impl Grouping<'_> {
    /// Takes the documents of `batch`, in their order, into the groups.
    pub(super) fn absorb(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        let (limit, store) = limits(self.env);
        if !self.spill {
            self.absorb_groups(&batch, (limit, store.is_some()))?;
        }
        if store.is_some() {
            self.kept.extend(batch);
        }
        Ok(())
    }

    /// The groups of the documents of `batch` with those so far, until they pass `limit` when
    /// there is a store to write to.
    fn absorb_groups(
        &mut self,
        batch: &[Document],
        (limit, store): (usize, bool),
    ) -> Result<(), MqlError> {
        let (group, sbe) = (self.group, self.sbe);
        let collation = self.env.collation();
        let col = collation.get();
        'batches: for chunk in batch.chunks(teseladb_mql::vm::BATCH) {
            // The batch's columns, when the group runs on them and the documents are stored.
            let raws = self.plan.as_ref().and_then(|_| super::group_columns::stored(chunk));
            let columns = self.plan.as_ref().zip(raws.as_deref()).map(|(p, r)| (p, p.load(r)));
            if let Some((p, c)) = columns.as_ref().filter(|(p, c)| p.whole(c)) {
                // Every key and input from the columns: the lanes of each group of the batch
                // go into its accumulators together.
                let mut lanes_of: Vec<(usize, u64)> = Vec::new();
                for lane in 0..chunk.len() {
                    let key = p.key(c, lane).expect("a whole batch's keys come from its columns");
                    let before = self.groups.list.len();
                    let size = super::errors::value_size(&key);
                    let g = self.groups.index_of(key, &group.outputs, col);
                    if self.groups.list.len() > before {
                        self.held += size;
                    }
                    match lanes_of.iter_mut().find(|(x, _)| *x == g) {
                        Some((_, mask)) => *mask |= 1 << lane,
                        None => lanes_of.push((g, 1 << lane)),
                    }
                    if store && self.held > limit {
                        self.spill = true;
                        break 'batches;
                    }
                }
                // The accumulators on columns collect no values, so they hold no more bytes.
                for (g, mask) in lanes_of {
                    let states = &mut self.groups.list[g].1;
                    for (k, ((_, acc), st)) in
                        group.outputs.iter().zip(states.iter_mut()).enumerate()
                    {
                        p.accumulate(c, k, acc, st, mask, (col, sbe))?;
                    }
                }
                continue;
            }
            for (lane, doc) in chunk.iter().enumerate() {
                let mut scope: Option<Scope> = None;
                let key = match columns.as_ref().and_then(|(p, c)| p.key(c, lane)) {
                    Some(k) => k,
                    None => {
                        let s = scope.get_or_insert_with(|| group_scope(self.env, doc, sbe));
                        group.id.eval(doc, s)?.unwrap_or(Value::Null)
                    }
                };
                let before = self.groups.list.len();
                let size = super::errors::value_size(&key);
                let states = self.groups.entry(key, &group.outputs, col)?;
                for (k, ((_, acc), st)) in group.outputs.iter().zip(states.iter_mut()).enumerate() {
                    match columns.as_ref().and_then(|(p, c)| p.input(c, k, lane)) {
                        Some(v) => acc.add_evaluated(st, v, (col, sbe))?,
                        None => {
                            let s = scope.get_or_insert_with(|| group_scope(self.env, doc, sbe));
                            acc.add(st, doc, s)?;
                        }
                    }
                    self.held += held_by(acc, st);
                }
                if self.groups.list.len() > before {
                    self.held += size;
                }
                if store && self.held > limit {
                    self.spill = true;
                    break 'batches;
                }
            }
        }
        Ok(())
    }

    /// The documents of the groups, or of the groups sorted through the store when they passed
    /// the limit.
    pub(super) fn finish(self) -> Result<Vec<Document>, MqlError> {
        let (limit, store) = limits(self.env);
        let (env, sbe, outputs) = (self.env, self.sbe, &self.group.outputs);
        if let (true, Some(store)) = (self.spill, store) {
            drop(self.groups);
            return self.group.run_sorted(self.kept, env, sbe, (limit, store));
        }
        let collation = env.collation();
        let out = self.groups.finish("_id", outputs, collation.get(), sbe)?;
        check_memory(outputs, &out, env.allow_disk_use(), sbe, limit)?;
        Ok(out)
    }
}

/// The 292 of a group over `limit` bytes (the stage's limit or the operation's, the lower)
/// when the command does not allow writing to disk: the classic engine counts the groups'
/// keys and values, the slot-based one the arrays of `$push` and `$addToSet` (measured on a
/// `refrouter` 9.0.4).
fn check_memory(
    outputs: &Outputs,
    out: &[Document],
    allow_disk_use: bool,
    sbe: bool,
    limit: usize,
) -> Result<(), MqlError> {
    if allow_disk_use {
        return Ok(());
    }
    let held = match sbe {
        false => super::errors::bytes_of(out),
        true => out
            .iter()
            .map(|d| {
                outputs
                    .iter()
                    .filter(|(_, a)| a.collects())
                    .filter_map(|(f, _)| d.get(f))
                    .map(super::errors::value_size)
                    .sum::<usize>()
            })
            .sum(),
    };
    if held <= limit {
        return Ok(());
    }
    let name = if sbe { "group" } else { "$group" };
    Err(error(
        super::errors::NO_DISK_USE,
        format!(
            "Exceeded memory limit for {name}, but didn't allow external spilling; pass allowDiskUse:true to opt in"
        ),
    ))
}

/// Groups in the order they appear, with their accumulator states.
#[derive(Default)]
struct Groups {
    index: HashMap<Vec<u8>, usize>,
    list: Vec<(Value, Vec<State>)>,
}

impl Groups {
    fn entry(
        &mut self,
        key: Value,
        outputs: &Outputs,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<&mut Vec<State>, MqlError> {
        let i = self.index_of(key, outputs, col);
        Ok(&mut self.list[i].1)
    }

    /// The position of the group of `key` in the list, which it joins when new.
    fn index_of(
        &mut self,
        key: Value,
        outputs: &Outputs,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> usize {
        let canon = canonical(&key, col);
        match self.index.get(&canon) {
            Some(i) => *i,
            None => {
                self.list.push((key, outputs.iter().map(|(_, a)| a.start()).collect()));
                self.index.insert(canon, self.list.len() - 1);
                self.list.len() - 1
            }
        }
    }

    fn finish(
        self,
        id: &str,
        outputs: &Outputs,
        col: Option<&dyn teseladb_bson::Collator>,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        let mut out = Vec::with_capacity(self.list.len());
        for (key, states) in self.list {
            let mut d = Document::new().with(id, key);
            for ((name, acc), st) in outputs.iter().zip(states) {
                d.push(name.clone(), acc.finish(st, col, sbe)?);
            }
            out.push(d);
        }
        Ok(out)
    }
}

fn less(a: &Value, b: &Value, col: Option<&dyn teseladb_bson::Collator>) -> Result<bool, MqlError> {
    Ok(order(&Some(a.clone()), &Some(b.clone()), col)? == std::cmp::Ordering::Less)
}

fn equal(a: &Value, b: &Value, col: Option<&dyn teseladb_bson::Collator>) -> Result<bool, MqlError> {
    Ok(order(&Some(a.clone()), &Some(b.clone()), col)? == std::cmp::Ordering::Equal)
}

/// A key that is the same for values that compare equal: numbers by their value, the other
/// types by their bytes, documents and arrays element by element.
pub(super) fn canonical(v: &Value, col: Option<&dyn teseladb_bson::Collator>) -> Vec<u8> {
    let mut out = Vec::new();
    encode(v, col, &mut out);
    out
}

fn encode(v: &Value, col: Option<&dyn teseladb_bson::Collator>, out: &mut Vec<u8>) {
    use teseladb_mql::decimal_math::from_f64_exact;
    let number = |d: teseladb_bson::Decimal128, out: &mut Vec<u8>| {
        out.push(b'n');
        // The shortest form of a decimal: equal numbers write the same text.
        let text = normal_number(&d.to_string());
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    };
    match v {
        Value::Null | Value::Undefined => out.push(b'0'),
        Value::Int32(n) => integer(i64::from(*n), out),
        Value::Int64(n) => integer(*n, out),
        Value::Double(x) if x.is_nan() => out.extend_from_slice(b"nNaN\0"),
        // An integer a double holds exactly: the text of that integer.
        Value::Double(x) if x.fract() == 0.0 && x.abs() < EXACT_INTEGERS => integer(*x as i64, out),
        Value::Double(x) => number(from_f64_exact(*x), out),
        Value::Decimal128(d) => number(*d, out),
        // In a collation, strings that compare equal share their sort key.
        Value::String(s) => match col {
            Some(c) => {
                let key = c.sort_key(s);
                out.push(b'k');
                out.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_le_bytes());
                out.extend_from_slice(&key);
            }
            None => {
                out.push(b's');
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
        },
        Value::Document(d) => {
            out.push(b'd');
            for (k, x) in d.iter() {
                out.extend_from_slice(k.as_bytes());
                out.push(0);
                encode(x, col, out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'a');
            for x in items {
                encode(x, col, out);
            }
            out.push(b']');
        }
        other => {
            out.push(b'o');
            let d = Document::new().with("v", other.clone());
            out.extend_from_slice(&d.to_bytes().unwrap_or_default());
        }
    }
}

/// A decimal's text with the exponent and trailing zeros normalized (`1.50E+1` and `15` agree).
/// The doubles below it in magnitude hold each integer exactly (2^53).
const EXACT_INTEGERS: f64 = 9_007_199_254_740_992.0;

/// The key of integer `n`: the text [`normal_number`] gives its decimal, written without one.
fn integer(n: i64, out: &mut Vec<u8>) {
    use std::io::Write;
    out.push(b'n');
    if n == 0 {
        out.extend_from_slice(b"0\0");
        return;
    }
    let mut core = n.unsigned_abs();
    let mut zeros = 0u32;
    while core.is_multiple_of(10) {
        core /= 10;
        zeros += 1;
    }
    let sign = if n < 0 { "-" } else { "" };
    // Writing to a vector does not fail.
    let _ = write!(out, "{sign}{core}e{zeros}");
    out.push(0);
}

fn normal_number(text: &str) -> String {
    match text {
        "NaN" | "Infinity" | "-Infinity" => return text.to_owned(),
        _ => {}
    }
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text),
    };
    let (mant, exp) = match body.split_once(['E', 'e']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = format!("{int}{frac}");
    let exp = exp - frac.len() as i64;
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return "0".to_owned();
    }
    let zeros = trimmed.len() - trimmed.trim_end_matches('0').len();
    let core = trimmed.trim_end_matches('0');
    format!("{}{core}e{}", if neg { "-" } else { "" }, exp + zeros as i64)
}

/// The type a boundary of `$bucket` belongs to (numbers are one type).
fn canonical_type(v: &Value) -> &'static str {
    match v {
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => "number",
        other => type_name_of(other),
    }
}

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
                super::errors::BUCKET_NOT_OBJECT,
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
        let collation = env.collation();
        let col = collation.get();
        let limit = env.stage_memory_limit().min(env.operation_memory_limit());
        let n = self.boundaries.len() - 1;
        let mut states: Vec<Option<Vec<State>>> = vec![None; n + 1];
        let mut slots = Vec::with_capacity(docs.len());
        let mut held = 0usize;
        for doc in &docs {
            let scope = group_scope(env, doc, sbe);
            let i = self.slot(doc, &scope, col, sbe)?;
            slots.push(i);
            let st = states[i]
                .get_or_insert_with(|| self.outputs.iter().map(|(_, a)| a.start()).collect());
            for ((_, acc), s) in self.outputs.iter().zip(st.iter_mut()) {
                acc.add(s, doc, &scope)?;
                held += held_by(acc, s);
            }
        }
        let store = env.spill().filter(|_| env.allow_disk_use() && held > limit);
        let filled: Vec<(usize, Vec<State>)> = match store {
            Some(store) => {
                drop(states);
                let keyed = slots
                    .into_iter()
                    .zip(docs)
                    .map(|(i, d)| (Value::Int64(i64::try_from(i).unwrap_or(i64::MAX)), d))
                    .collect();
                groups_on_disk(&self.outputs, keyed, (env, sbe), (limit, store))?
                    .into_iter()
                    .map(|(k, st)| match k {
                        Value::Int64(i) => (usize::try_from(i).unwrap_or(n), st),
                        _ => (n, st),
                    })
                    .collect()
            }
            None => states.into_iter().enumerate().filter_map(|(i, st)| Some((i, st?))).collect(),
        };
        let mut out = Vec::new();
        for (i, st) in filled {
            let id = if i < n {
                self.boundaries[i].clone()
            } else {
                self.default.clone().unwrap_or(Value::Null)
            };
            let mut d = Document::new().with("_id", id);
            for ((name, acc), s) in self.outputs.iter().zip(st) {
                d.push(name.clone(), acc.finish(s, col, sbe)?);
            }
            out.push(d);
        }
        check_memory(&self.outputs, &out, env.allow_disk_use(), sbe, limit)?;
        Ok(out)
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
                super::errors::BUCKET_AUTO_NOT_OBJECT,
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
        let collation = env.collation();
        let col = collation.get();
        let mut keyed = Vec::with_capacity(docs.len());
        for doc in docs {
            let scope = group_scope(env, &doc, false);
            let v = self.group_by.eval(&doc, &scope)?.unwrap_or(Value::Null);
            // The values are checked in the order the documents come.
            if self.granularity.is_some() {
                Granularity::check(&v)?;
            }
            keyed.push((v, doc));
        }
        let keyed = sorted_by_values(keyed, env)?;
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

/// `keyed` sorted by its values, through the node's store when the command allows it and the
/// documents pass the stage's limit (010 FR-010, T024).
pub(super) fn sorted_by_values(
    mut keyed: Vec<(Value, Document)>,
    env: &dyn Env,
) -> Result<Vec<(Value, Document)>, MqlError> {
    let collation = env.collation();
    let col = collation.get();
    let limit = env.stage_memory_limit();
    let held: usize = keyed.iter().map(|(_, d)| super::errors::document_size(d)).sum();
    let Some(store) = env.spill().filter(|_| env.allow_disk_use() && held > limit) else {
        crate::expr::sort_by_values(&mut keyed, col)?;
        return Ok(keyed);
    };
    let wrapped = keyed.into_iter().map(|(v, d)| super::spill::with_key(d, v));
    let mut out = Vec::new();
    super::spill::external_sort(
        wrapped.collect(),
        |w| Ok(super::spill::key_of(w)),
        |a: &Value, b| order(&Some(a.clone()), &Some(b.clone()), col),
        (limit, store),
        |v, w| {
            out.push((v, super::spill::without_key(w)));
            Ok(())
        },
    )?;
    Ok(out)
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

/// `$sortByCount`: groups by the expression and counts, the largest count first.
pub fn sort_by_count(v: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
    match v {
        RawValue::String(s) if s.starts_with('$') => Expr::parse_in(v, ctx),
        RawValue::Document(d) => match d.iter().next() {
            Some((k, _)) if k.starts_with('$') => Expr::parse_in(v, ctx),
            _ => Err(error(
                SORT_BY_COUNT_OBJECT,
                "the sortByCount field must be defined as a $-prefixed path or an expression inside an object",
            )),
        },
        RawValue::String(_) => Err(error(
            SORT_BY_COUNT_FIELD,
            "the sortByCount field must be defined as a $-prefixed path or an expression inside an object",
        )),
        _ => Err(error(
            super::errors::SORT_BY_COUNT_TYPE,
            "the sortByCount field must be specified as a string or as an object",
        )),
    }
}

/// The counts of `$sortByCount` over `docs`.
pub fn run_sort_by_count(
    e: &Expr,
    docs: Vec<Document>,
    env: &dyn Env,
    sbe: bool,
) -> Result<Vec<Document>, MqlError> {
    // `$sortByCount` is a `$group` that counts, then a `$sort` by the count (with its memory).
    let group = Group { id: e.clone(), outputs: vec![("count".to_owned(), Accumulator::Count)] };
    let mut out = group.run(docs, env, sbe)?;
    let count = |d: &Document| match d.get("count") {
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        _ => 0,
    };
    out.sort_by_key(|d| std::cmp::Reverse(count(d)));
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// The key the decimal of a number gives, as every number had it before the integers' own.
    fn through_decimal(d: teseladb_bson::Decimal128) -> Vec<u8> {
        let mut out = vec![b'n'];
        out.extend_from_slice(normal_number(&d.to_string()).as_bytes());
        out.push(0);
        out
    }

    fn key(v: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        encode(v, None, &mut out);
        out
    }

    #[test]
    fn integers_and_whole_doubles_keep_the_key_of_their_decimal() {
        use teseladb_mql::decimal_math::from_f64_exact;
        let ints = [
            0i64,
            1,
            -1,
            7,
            10,
            -10,
            100,
            120,
            1_000_000,
            -4_500,
            i64::from(i32::MAX),
            i64::from(i32::MIN),
            i64::MAX,
            i64::MIN,
            1 << 53,
        ];
        for n in ints {
            let d = teseladb_bson::Decimal128::from_parts(n < 0, u128::from(n.unsigned_abs()), 0)
                .expect("an integer");
            assert_eq!(key(&Value::Int64(n)), through_decimal(d), "{n}");
            if let Ok(small) = i32::try_from(n) {
                assert_eq!(key(&Value::Int32(small)), through_decimal(d), "{n}");
            }
        }
        for x in [0.0, -0.0, 1.0, -3.0, 250.0, 1e15, -9_007_199_254_740_991.0, 0.5, 1e300] {
            assert_eq!(key(&Value::Double(x)), through_decimal(from_f64_exact(x)), "{x}");
        }
        assert_eq!(key(&Value::Int32(5)), key(&Value::Double(5.0)));
    }
}
