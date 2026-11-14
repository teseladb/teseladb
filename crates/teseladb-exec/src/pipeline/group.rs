//! The stages that group documents (measured on a `refrouter` 9.0.4): `$group`, `$bucket`,
//! `$bucketAuto` and `$sortByCount`. Groups are keyed by values that compare equal (`1` and
//! `1.0` are one group, a missing `_id` is `null`) and come out in the order their first document
//! came; a `$group` that wrote to disk gives them in the order of their keys.

use std::collections::HashMap;

use teseladb_mql::MqlError;
use teseladb_bson::{Document, RawValue, Value};

use std::borrow::Cow;

use super::Env;
use super::errors::{
    GROUP_DOLLAR, GROUP_DOT, GROUP_ID, SORT_BY_COUNT_FIELD, SORT_BY_COUNT_OBJECT, error,
};
use super::spill::{Sorter, SpillStore, key_of, with_key, without_key};
use crate::accum::{Accumulator, Part, State};
use crate::expr::arith::type_name_of;
use crate::expr::{Ctx, Expr, Scope, order};

mod bucket;

pub use bucket::{Bucket, BucketAuto};
pub(super) use bucket::{BucketAutoRun, Bucketing};

/// The scope of an expression of a group over `doc`, `sbe` when RefDb evaluates it with its
/// slot-based engine.
fn group_scope(env: &dyn Env, doc: &Document, sbe: bool) -> Scope {
    let mut s = env.scope(doc);
    s.sbe = sbe;
    s
}

/// A sort by group key through `store` in runs of at most `limit` bytes, of documents that carry
/// their key ([`with_key`]).
fn key_sorter<'s>(
    collation: crate::collation::Collation,
    disk: (usize, &'s dyn SpillStore),
) -> Sorter<'s, (Vec<u8>, Value)> {
    let canon = collation.clone();
    Sorter::new(
        Box::new(move |w| {
            let k = key_of(w);
            Ok((canonical(&k, canon.get()), k))
        }),
        // Keys of one group compare equal even where the collation still tells them apart
        // (`a` and `A` at strength 2), so that the group keeps its documents' order.
        Box::new(move |a: &(Vec<u8>, Value), b| match a.0 == b.0 {
            true => Ok(std::cmp::Ordering::Equal),
            false => order(&Some(a.1.clone()), &Some(b.1.clone()), collation.get()),
        }),
        disk,
    )
}

/// The groups of the documents of `sorter` with accumulators `outputs`, built one at a time in
/// the order of their keys; each keeps the key of its first document (010 FR-010, T024).
fn groups_from_sorter(
    outputs: &Outputs,
    sorter: Sorter<'_, (Vec<u8>, Value)>,
    (env, sbe): (&dyn Env, bool),
) -> Result<Vec<(Value, Vec<State>)>, MqlError> {
    // Each group with its canonical key, in the order of their keys.
    let mut out: Vec<(Vec<u8>, Value, Vec<State>)> = Vec::new();
    sorter.finish(|(canon, key), w| {
        let doc = without_key(w);
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
    })?;
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
        Grouping::of(Cow::Borrowed(self), env, sbe)
    }
}

impl<'g> Grouping<'g> {
    /// A run of `group` that takes its input a batch at a time.
    pub(super) fn of(group: Cow<'g, Group>, env: &'g dyn Env, sbe: bool) -> Grouping<'g> {
        let plan = super::group_columns::columnar(&group.id, &group.outputs);
        Grouping {
            group,
            env,
            sbe,
            groups: Groups::default(),
            held: 0,
            plan,
            spill: false,
            kept: Vec::new(),
            kept_bytes: 0,
            sorter: None,
        }
    }
}

impl Group {
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

/// The groups of a `$group` over one part of its input, before their values are finished (010
/// FR-034): in the order they appeared, each with its canonical key, its key and its
/// accumulators' states. The groups of consecutive parts merge into those of the whole
/// ([`Group::merge_parts`]).
#[derive(Clone, Debug, Default)]
pub struct PartialGroups {
    list: Vec<(Vec<u8>, Value, Vec<Part>)>,
}

impl PartialGroups {
    /// How many groups.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

impl Group {
    /// Whether the states of every accumulator merge, so that the groups of parts of the input
    /// give those of the whole ([`Accumulator::merges`]).
    pub fn merges(&self) -> bool {
        self.outputs.iter().all(|(_, a)| a.merges())
    }

    /// The groups of `docs`, one part of the input, as [`Group::run`] would build them before
    /// finishing them; `None` when their keys pass `limit` bytes (the whole would then go through
    /// the store, in the order of the keys, or fail) or an accumulator does not merge.
    pub fn partial(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
        limit: usize,
    ) -> Result<Option<PartialGroups>, MqlError> {
        if !self.merges() {
            return Ok(None);
        }
        let mut grouping = self.start(env, sbe);
        grouping.absorb_groups(&docs, (limit, true))?;
        if grouping.spill {
            return Ok(None);
        }
        let collation = env.collation();
        let col = collation.get();
        let mut list = Vec::with_capacity(grouping.groups.list.len());
        for (key, states) in grouping.groups.list {
            let parts: Option<Vec<Part>> = states.into_iter().map(Part::of).collect();
            let Some(parts) = parts else { return Ok(None) };
            list.push((canonical(&key, col), key, parts));
        }
        Ok(Some(PartialGroups { list }))
    }

    /// The documents of the groups of the whole input from those of its consecutive parts
    /// `parts`, in their order: each group in the order it first appeared, with the key of its
    /// first document, as [`Group::run`] gives them over the parts one after the other, the
    /// memory limit included. `None` when a merge would not give the same states
    /// ([`Accumulator::merge`]) or the groups pass the limit with a store to write to (the run
    /// would then sort them by key through it), and the caller runs the stage over the whole.
    pub fn merge_parts(
        &self,
        parts: Vec<PartialGroups>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<Option<Vec<Document>>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut list: Vec<(Value, Vec<Part>)> = Vec::new();
        let mut held = 0usize;
        for part in parts {
            for (canon, key, states) in part.list {
                let Some(&i) = index.get(&canon) else {
                    held += super::errors::value_size(&key);
                    index.insert(canon, list.len());
                    list.push((key, states));
                    continue;
                };
                for ((_, acc), (into, later)) in
                    self.outputs.iter().zip(list[i].1.iter_mut().zip(&states))
                {
                    if !acc.merge(into, later, (col, sbe))? {
                        return Ok(None);
                    }
                }
            }
        }
        let (limit, store) = limits(env);
        if store.is_some() && held > limit {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(list.len());
        for (key, parts) in list {
            let states = parts.into_iter().map(Part::into_state).collect();
            out.push(self.finished(key, states, col, sbe)?);
        }
        check_memory(&self.outputs, &out, env.allow_disk_use(), sbe, limit)?;
        Ok(Some(out))
    }
}

/// A `$group` taking its input a batch at a time (010 FR-028): the groups so far and the bytes
/// they hold; with a store, the documents so far, which go through the store sorted by key once
/// the groups pass the stage's limit, as does every document after them. Until then the
/// documents wait in memory only within what the groups leave of the limit ([`spare`]); the
/// rest goes to the store as it comes, in runs the store counts only if the groups pass the
/// limit (010 T066).
pub(super) struct Grouping<'g> {
    group: Cow<'g, Group>,
    env: &'g dyn Env,
    sbe: bool,
    groups: Groups,
    held: usize,
    plan: Option<super::group_columns::Columnar>,
    /// The groups passed the limit: the rest only waits for the sort through the store.
    spill: bool,
    /// The input so far that waits in memory, kept only with a store to write to and before
    /// the sort by key starts.
    kept: Vec<Document>,
    /// The bytes of `kept`.
    kept_bytes: usize,
    /// The sort by key through the store: provisional while the groups fit in the limit.
    sorter: Option<Sorter<'g, (Vec<u8>, Value)>>,
}

/// The bytes the documents waiting for the store may hold beside groups that hold `held` of
/// the stage's `limit`: what the groups leave, and at least a quarter of the limit so that the
/// runs do not shrink to a document each as the groups near the limit.
pub(super) fn spare(limit: usize, held: usize) -> usize {
    limit.saturating_sub(held).max(limit / 4)
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
        let Some(store) = store else { return Ok(()) };
        if self.spill && !self.groups.list.is_empty() {
            // The groups are built again from the store, one at a time.
            self.groups = Groups::default();
            self.held = 0;
            if let Some(sorter) = &mut self.sorter {
                sorter.confirm();
            }
        }
        let budget = if self.spill { limit } else { spare(limit, self.held) };
        if self.sorter.is_none() {
            self.kept_bytes += batch.iter().map(super::errors::document_size).sum::<usize>();
            self.kept.extend(batch);
            if !self.spill && self.kept_bytes <= budget {
                return Ok(());
            }
            let sorter = key_sorter(self.env.collation(), (limit, store));
            self.sorter = Some(if self.spill { sorter } else { sorter.provisional() });
            self.kept_bytes = 0;
            let kept = std::mem::take(&mut self.kept);
            return self.feed_sorter(kept, budget);
        }
        self.feed_sorter(batch, budget)
    }

    /// Gives `docs` with their keys to the sort by key, the documents waiting in memory within
    /// `budget` bytes.
    fn feed_sorter(&mut self, docs: Vec<Document>, budget: usize) -> Result<(), MqlError> {
        let (env, sbe) = (self.env, self.sbe);
        let Some(sorter) = &mut self.sorter else { return Ok(()) };
        for doc in docs {
            let key =
                self.group.id.eval(&doc, &group_scope(env, &doc, sbe))?.unwrap_or(Value::Null);
            sorter.push_within(with_key(doc, key), budget)?;
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
        let (group, sbe) = (&*self.group, self.sbe);
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
        let (limit, _) = limits(self.env);
        let (env, sbe, outputs) = (self.env, self.sbe, &self.group.outputs);
        if let Some(sorter) = self.sorter.filter(|_| self.spill) {
            drop(self.groups);
            let col = env.collation();
            return groups_from_sorter(outputs, sorter, (env, sbe))?
                .into_iter()
                .map(|(k, states)| self.group.finished(k, states, col.get(), sbe))
                .collect();
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
    Ok(by_count(counting(e).run(docs, env, sbe)?))
}

/// The `$group` a `$sortByCount` of `e` stands for: by `e`, counting.
pub(super) fn counting(e: &Expr) -> Group {
    Group { id: e.clone(), outputs: vec![("count".to_owned(), Accumulator::Count)] }
}

/// The groups of a `$sortByCount` (from [`counting`]), the largest count first.
pub(super) fn by_count(mut out: Vec<Document>) -> Vec<Document> {
    let count = |d: &Document| match d.get("count") {
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        _ => 0,
    };
    out.sort_by_key(|d| std::cmp::Reverse(count(d)));
    out
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
