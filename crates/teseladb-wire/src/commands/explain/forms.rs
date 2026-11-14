//! The stages after a query plan as `explain` lists them (measured on the reference router 9.0.4,
//! 01-11-2026): each in the form the reference server keeps after parsing it, its expressions
//! optimised (constants as `{$const: v}`), the stages that are others in disguise as those others
//! (`$unset` as `$project`, `$replaceWith` as `$replaceRoot`, `$count` and `$sortByCount` as a
//! `$group` and what follows it, `$bucket` as a `$group` by a `$switch`, `$fill` of values as
//! `$addFields`), and the defaults they take written out.

use teseladb_bson::{Document, Value};

use super::aggregate::int64;
use super::parsed;
use super::steps::{Accumulator, Bucket, Fill, FillWith, GroupEntry, Kind, Step, Window};

/// Expression `v` as the reference server optimises and writes it.
pub(super) fn optimized(v: &Value) -> Value {
    let ctx = teseladb_exec::expr::Ctx::default();
    let scope = teseladb_exec::expr::Scope::default();
    teseladb_exec::expr::optimize::optimize(v, &ctx, &scope)
        .map_or_else(|_| v.clone(), |o| o.written())
}

/// Whether a `$project` value drops its field.
pub(super) fn off(v: &Value) -> bool {
    matches!(v, Value::Int32(0) | Value::Int64(0) | Value::Boolean(false))
        || matches!(v, Value::Double(x) if *x == 0.0)
}

/// Whether a `$project` value keeps its field (a flag that is not off, or an expression).
fn keeps(v: &Value) -> bool {
    !flag(v) || !off(v)
}

/// Whether a `$project` value is a flag (rather than an expression).
fn flag(v: &Value) -> bool {
    matches!(
        v,
        Value::Boolean(_)
            | Value::Int32(_)
            | Value::Int64(_)
            | Value::Double(_)
            | Value::Decimal128(_)
    )
}

/// `d` with `value` at dotted `path`, nesting documents.
fn put(d: &mut Document, path: &str, value: Value) {
    match path.split_once('.') {
        None => d.push(path, value),
        Some((head, rest)) => {
            let mut inner = match d.get(head) {
                Some(Value::Document(x)) => x.clone(),
                _ => Document::new(),
            };
            put(&mut inner, rest, value);
            d.insert(head, inner);
        }
    }
}

/// A `$project` as the reference server keeps it: an inclusion with `_id` first, the kept fields,
/// then the computed ones; an exclusion with the dropped fields, then `_id`; dotted paths nested.
pub(super) fn project(p: &Document) -> Document {
    let inclusion = p.iter().filter(|(k, _)| *k != "_id").any(|(_, v)| keeps(v));
    let id_off = p.get("_id").is_some_and(off);
    let mut out = Document::new();
    if inclusion {
        // A computed `_id` goes with the computed fields.
        let id_flag = p.get("_id").is_none_or(flag);
        if !id_off && id_flag {
            out.push("_id", true);
        }
        // Kept fields first, then those under a dotted path (measured).
        let kept: Vec<(&str, &Value)> = p.iter().filter(|(k, v)| *k != "_id" && flag(v)).collect();
        for (k, v) in kept.iter().filter(|(k, _)| !k.contains('.')) {
            out.push(*k, Value::Boolean(!off(v)));
        }
        for (k, v) in kept.iter().filter(|(k, _)| k.contains('.')) {
            put(&mut out, k, Value::Boolean(!off(v)));
        }
        for (k, v) in p.iter().filter(|(k, v)| (*k != "_id" || !id_flag) && !flag(v)) {
            let value = match v {
                Value::Document(d) if d.iter().next().is_some_and(|(x, _)| !x.starts_with('$')) => {
                    Value::Document(nested(d))
                }
                other => optimized(other),
            };
            put(&mut out, k, value);
        }
        if id_off {
            out.push("_id", false);
        }
        return out;
    }
    // An exclusion writes a given `_id` first, the one it keeps by default last (measured).
    if p.get("_id").is_some() {
        out.push("_id", !id_off);
    }
    for (k, _) in p.iter().filter(|(k, _)| *k != "_id") {
        put(&mut out, k, Value::Boolean(false));
    }
    if p.get("_id").is_none() {
        out.push("_id", true);
    }
    out
}

/// A nested document of a `$project`: flags as booleans, expressions optimised.
fn nested(d: &Document) -> Document {
    let mut out = Document::new();
    for (k, v) in d.iter() {
        let value = if flag(v) { Value::Boolean(!off(v)) } else { optimized(v) };
        out.push(k, value);
    }
    out
}

/// The fields of `$addFields`, optimised, dotted paths nested.
fn add_fields(p: &Document) -> Document {
    let mut out = Document::new();
    for (k, v) in p.iter() {
        let value = match v {
            Value::Document(d) if d.iter().next().is_some_and(|(x, _)| !x.starts_with('$')) => {
                Value::Document(add_fields(d))
            }
            other => optimized(other),
        };
        put(&mut out, k, value);
    }
    out
}

/// An accumulator of `$group`: `$count` as a sum of ones, an argument optimised (the named
/// arguments of `$top` and the like as given).
fn accumulator(a: &Accumulator) -> Value {
    let arg = match (a.op.as_str(), &a.arg) {
        ("$count", _) => {
            return Value::Document(
                Document::new().with("$sum", Document::new().with("$const", 1)),
            );
        }
        (_, Value::Document(named))
            if named.iter().next().is_some_and(|(k, _)| !k.starts_with('$')) =>
        {
            Value::Document(named_arguments(named))
        }
        (_, other) => optimized(other),
    };
    Value::Document(Document::new().with(a.op.as_str(), arg))
}

/// The named arguments of an accumulator or window function as the reference server writes them
/// back (measured): `n` optimised, then `$shift`'s `by` before its `output`, and `output` before
/// `sortBy`; any other in its place.
fn named_arguments(named: &Document) -> Document {
    const ORDER: [&str; 5] = ["n", "by", "output", "sortBy", "default"];
    let mut out = Document::new();
    for k in ORDER {
        if let Some(v) = named.get(k) {
            let v = if k == "n" { optimized(v) } else { v.clone() };
            out.push(k, v);
        }
    }
    for (k, v) in named.iter().filter(|(k, _)| !ORDER.contains(k)) {
        out.push(k, v.clone());
    }
    out
}

/// A `$group` with its key and accumulators as the reference server keeps them.
fn group(entries: &[GroupEntry]) -> Document {
    let mut out = Document::new();
    for e in entries {
        match e {
            GroupEntry::Key(k) => out.push("_id", optimized(k)),
            GroupEntry::Field(name, a) => out.push(name, accumulator(a)),
        }
    }
    out.with("$willBeMerged", false)
}

/// `{$sort: {sortKey: key}}`.
fn sort_by(key: Document) -> Document {
    Document::new().with("$sort", Document::new().with("sortKey", key))
}

/// The `$switch` that gives the bucket of `groupBy` among `boundaries`, or `default`.
fn bucket_key(b: &Bucket) -> Value {
    let by = &b.group_by;
    let mut branches = Vec::new();
    for w in b.boundaries.windows(2) {
        let cond = Document::new().with(
            "$and",
            Value::Array(vec![
                Value::Document(
                    Document::new().with("$gte", Value::Array(vec![by.clone(), w[0].clone()])),
                ),
                Value::Document(
                    Document::new().with("$lt", Value::Array(vec![by.clone(), w[1].clone()])),
                ),
            ]),
        );
        branches.push(Value::Document(
            Document::new()
                .with("case", cond)
                .with("then", Document::new().with("$literal", w[0].clone())),
        ));
    }
    let mut sw = Document::new().with("branches", Value::Array(branches));
    if let Some(d) = &b.default {
        sw.push("default", Document::new().with("$literal", d.clone()));
    }
    optimized(&Value::Document(Document::new().with("$switch", sw)))
}

/// The output of `$bucket` and `$bucketAuto`, a count by default, as the reference server keeps it.
fn bucket_output(output: Option<&Vec<(String, Accumulator)>>) -> Vec<(String, Value)> {
    let count = [("count".to_owned(), Accumulator { op: "$sum".to_owned(), arg: Value::Int32(1) })];
    output.map_or(&count[..], |o| &o[..]).iter().map(|(k, a)| (k.clone(), accumulator(a))).collect()
}

/// `value` at `key` of `d` when there is one.
fn put_some(d: &mut Document, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        d.push(key, v.clone());
    }
}

/// The projection an `$unset` of `fields` is: the fields dropped, nested when dotted, `_id`
/// kept; the engine writes the dropped fields alone (measured).
pub(super) fn unset_projection(fields: &[String], engine: bool) -> Document {
    let mut spec = Document::new();
    for f in fields {
        spec.push(f, false);
    }
    let mut d = project(&spec);
    if engine {
        d.remove("_id");
    }
    d
}

/// The values a `$fill` sets, as the `$addFields` it is: each field `$ifNull` of its value.
pub(super) fn fill_values(f: &Fill) -> Document {
    let mut d = Document::new();
    let values = f.output.iter().filter_map(|(k, w)| match w {
        FillWith::Value(v) => Some((k, v)),
        FillWith::Linear | FillWith::Locf => None,
    });
    for (k, value) in values {
        let e = Document::new()
            .with("$ifNull", Value::Array(vec![Value::String(format!("${k}")), value.clone()]));
        d.push(k, optimized(&Value::Document(e)));
    }
    d
}

/// A `$fill` by a method as the window over its sort it is: `$locf` or `$linearFill`.
fn fill_window(f: &Fill) -> Vec<Document> {
    let mut w = Document::new();
    put_some(&mut w, "partitionBy", f.partition_by.as_ref());
    put_some(&mut w, "sortBy", f.sort_by.clone().map(Value::Document).as_ref());
    let mut out = Document::new();
    for (k, how) in &f.output {
        let method = match how {
            FillWith::Linear => "$linearFill",
            FillWith::Value(_) | FillWith::Locf => "$locf",
        };
        out.push(k, Document::new().with(method, format!("${k}")));
    }
    w.push("output", out);
    let w = super::steps::window(&w);
    // A partition by an expression goes through its own field, as `$setWindowFields`'s.
    match partition_expression(w.partition_by.as_ref()) {
        Some(e) => keyed_window(&w, e),
        None => vec![Document::new().with("$_internalSetWindowFields", window_fields(&w))],
    }
}

/// The entries `explain` lists for stage `st` (most one, some two).
pub(super) fn forms(st: &Step) -> Vec<Document> {
    forms_with(st, None)
}

/// The clauses of a `$match` as a stage lists them (measured): each in its place, as parsed,
/// except each `$expr` that implies clauses on its paths, which goes last with them (before
/// it, or after it when it implies several).
pub(super) fn match_form(m: &Document, vars: Option<&Document>) -> Document {
    let mut out: Vec<Value> = Vec::new();
    let mut implied: Vec<Value> = Vec::new();
    for c in super::moves::clauses(m) {
        let rewritten =
            teseladb_exec::expr::optimize::optimize_filter(&c, vars).unwrap_or(c.clone());
        let group = match rewritten.get("$and") {
            Some(Value::Array(parts)) if c.get("$expr").is_some() => parts.clone(),
            _ => Vec::new(),
        };
        if let [expr, more] = &group[..] {
            match more {
                Value::Document(d) => match d.get("$and") {
                    Some(Value::Array(each)) if each.len() > 1 => {
                        implied.push(expr.clone());
                        implied.extend(each.iter().cloned());
                    }
                    Some(Value::Array(each)) => {
                        implied.extend(each.iter().cloned());
                        implied.push(expr.clone());
                    }
                    _ => implied.extend([more.clone(), expr.clone()]),
                },
                other => implied.extend([other.clone(), expr.clone()]),
            }
            continue;
        }
        if let Some(Value::Array(items)) = rewritten.get("$or") {
            out.push(Value::Document(Document::new().with("$or", or_form(items, vars))));
            continue;
        }
        let shown = parsed(&rewritten, vars).unwrap_or(rewritten);
        match shown.get("$and") {
            Some(Value::Array(parts)) if shown.len() == 1 => out.extend(parts.iter().cloned()),
            _ => out.push(Value::Document(shown)),
        }
    }
    out.extend(implied);
    match <[Value; 1]>::try_from(out) {
        Ok([Value::Document(only)]) => only,
        Ok([other]) => Document::new().with("$and", vec![other]),
        Err(all) => Document::new().with("$and", all),
    }
}

/// The clauses of an `$or` of a `$match` as a stage lists them (measured): each as parsed, in
/// their order, two of them the other way round.
fn or_form(items: &[Value], vars: Option<&Document>) -> Vec<Value> {
    let mut out: Vec<Value> = super::steps::documents(items)
        .map(|d| Value::Document(parsed(d, vars).unwrap_or(d.clone())))
        .collect();
    if out.len() == 2 {
        out.reverse();
    }
    out
}

/// The entries `explain` lists for stage `st`, with the values of variables `vars` in its
/// filters.
pub(super) fn forms_with(st: &Step, vars: Option<&Document>) -> Vec<Document> {
    let name = st.name();
    let one = |d: Document| vec![Document::new().with(name, d)];
    match &st.kind {
        Kind::AddFields(p) => one(add_fields(p)),
        Kind::Project(p) => one(project(p)),
        Kind::Unset(fields) => {
            vec![Document::new().with("$project", unset_projection(fields, false))]
        }
        Kind::Group(entries) => one(group(entries)),
        Kind::Count(field) => {
            let g = Document::new()
                .with("_id", Document::new().with("$const", Value::Null))
                .with(
                    field.as_str(),
                    Document::new().with("$sum", Document::new().with("$const", 1)),
                )
                .with("$willBeMerged", false);
            let p = Document::new().with(field.as_str(), true).with("_id", false);
            vec![Document::new().with("$group", g), Document::new().with("$project", p)]
        }
        Kind::SortByCount(v) => {
            let g = Document::new()
                .with("_id", optimized(v))
                .with("count", Document::new().with("$sum", Document::new().with("$const", 1)))
                .with("$willBeMerged", false);
            vec![Document::new().with("$group", g), sort_by(Document::new().with("count", -1))]
        }
        Kind::Bucket(b) => {
            let mut g = Document::new().with("_id", bucket_key(b));
            for (k, v) in bucket_output(b.output.as_ref()) {
                g.push(&k, v);
            }
            g.push("$willBeMerged", false);
            vec![Document::new().with("$group", g), sort_by(Document::new().with("_id", 1))]
        }
        Kind::BucketAuto(b) => {
            let mut d = Document::new()
                .with("groupBy", b.group_by.clone())
                .with("buckets", b.buckets.clone());
            put_some(&mut d, "granularity", b.granularity.as_ref());
            let mut out = Document::new();
            for (k, v) in bucket_output(b.output.as_ref()) {
                out.push(&k, v);
            }
            d.push("output", out);
            one(d)
        }
        Kind::Facet(branches) => {
            let mut d = Document::new();
            for (k, stages) in branches {
                let tee = Document::new().with("$internalFacetTeeConsumer", Document::new());
                let list: Vec<Value> = std::iter::once(tee)
                    .chain(stages.iter().flat_map(|s| forms_with(s, vars)))
                    .map(Value::Document)
                    .collect();
                d.push(k, Value::Array(list));
            }
            one(d)
        }
        Kind::Lookup(l) => {
            let mut d = Document::new();
            let from = l.from.clone().unwrap_or(COLLECTIONLESS.to_owned());
            d.push("from", from);
            d.push("as", l.as_field.as_str());
            put_some(&mut d, "localField", l.local_field.as_ref());
            put_some(&mut d, "foreignField", l.foreign_field.as_ref());
            if let Some(stages) = &l.pipeline {
                d.push("let", l.let_vars.clone().unwrap_or(Value::Document(Document::new())));
                let list: Vec<Value> =
                    stages.iter().flat_map(|s| forms_with(s, vars)).map(Value::Document).collect();
                d.push("pipeline", Value::Array(list));
            }
            one(d)
        }
        Kind::GraphLookup(g) => {
            let mut d = Document::new();
            for k in [
                "from",
                "as",
                "connectToField",
                "connectFromField",
                "startWith",
                "maxDepth",
                "depthField",
                "restrictSearchWithMatch",
            ] {
                put_some(&mut d, k, g.get(k));
            }
            one(d)
        }
        Kind::ReplaceRoot(v) => {
            vec![
                Document::new().with("$replaceRoot", Document::new().with("newRoot", optimized(v))),
            ]
        }
        Kind::SetWindowFields(w) => match partition_expression(w.partition_by.as_ref()) {
            Some(e) => keyed_window(w, e),
            None => vec![Document::new().with("$_internalSetWindowFields", window_fields(w))],
        },
        Kind::Densify(dn) => {
            let fields: Vec<Value> =
                dn.partition_by_fields.iter().map(|f| Value::String(f.clone())).collect();
            let d = Document::new()
                .with("field", dn.field.as_str())
                .with("partitionByFields", Value::Array(fields))
                .with("range", dn.range.clone());
            vec![Document::new().with("$_internalDensify", d)]
        }
        Kind::Fill(f) if !f.fills_values() => fill_window(f),
        Kind::Fill(f) => vec![Document::new().with("$addFields", fill_values(f))],
        Kind::Sample(size) => one(Document::new().with("size", int64(*size))),
        Kind::Redact(v) => vec![Document::new().with("$redact", optimized(v))],
        Kind::Match(m) => one(match_form(m, vars)),
        Kind::Limit(n) | Kind::Skip(n) => vec![Document::new().with(name, int64(*n))],
        Kind::Sort(s) => vec![sort_by(s.clone())],
        Kind::Unwind(u) => {
            let mut d = Document::new().with("path", u.path.as_str());
            put_some(&mut d, "preserveNullAndEmptyArrays", u.preserve.as_ref());
            put_some(&mut d, "includeArrayIndex", u.index.clone().map(Value::String).as_ref());
            one(d)
        }
        Kind::UnionWith(_) | Kind::Other => vec![st.doc.clone()],
    }
}

/// The collection a `$lookup` without one names: its `$documents` (measured).
const COLLECTIONLESS: &str = "$cmd.aggregate";

/// The field the reference server computes a `$setWindowFields` partition into when it partitions
/// by an expression.
const PARTITION_KEY: &str = "__internal_setWindowFields_partition_key";

/// Whether an optimised expression is a constant.
fn is_constant(e: &Value) -> bool {
    matches!(e, Value::Document(d) if d.get("$const").is_some())
}

/// The partition expression of a window (`partitionBy`), optimised, when it partitions by an
/// expression rather than by a path or a constant.
pub(super) fn partition_expression(partition_by: Option<&Value>) -> Option<Value> {
    match partition_by? {
        v @ Value::Document(_) => Some(optimized(v)).filter(|e| !is_constant(e)),
        _ => None,
    }
}

/// Whether a `$setWindowFields` partitions by a constant, which partitions nothing.
pub(super) fn constant_partition(w: &Window) -> bool {
    match &w.partition_by {
        Some(Value::String(p)) => !p.starts_with('$'),
        Some(v @ Value::Document(_)) => is_constant(&optimized(v)),
        Some(_) => true,
        None => false,
    }
}

/// A `$setWindowFields` by an expression as the reference server runs it (measured): the partition
/// computed into a field, a sort on it and on the window's order, the window over that field, and
/// the field dropped.
fn keyed_window(w: &Window, e: Value) -> Vec<Document> {
    let mut sort_key = Document::new().with(PARTITION_KEY, 1);
    for (k, v) in w.sort_by.iter().flat_map(|s| s.iter()) {
        sort_key.push(k, v.clone());
    }
    let mut window = Document::new();
    for (k, v) in window_fields(w).iter() {
        let v =
            if k == "partitionBy" { Value::String(format!("${PARTITION_KEY}")) } else { v.clone() };
        window.push(k, v);
    }
    vec![
        Document::new().with("$addFields", Document::new().with(PARTITION_KEY, e)),
        Document::new().with(
            "$sort",
            Document::new().with("sortKey", sort_key).with("outputSortKeyMetadata", true),
        ),
        Document::new().with("$_internalSetWindowFields", window),
        Document::new()
            .with("$project", Document::new().with(PARTITION_KEY, false).with("_id", true)),
    ]
}

/// A `$setWindowFields` as the reference server keeps it: `$count` as a sum of ones over the whole
/// partition.
fn window_fields(w: &Window) -> Document {
    let mut out = Document::new();
    for (k, v) in w.spec.iter() {
        let v = match k {
            "output" => {
                let mut fields = Document::new();
                for (name, f) in &w.output {
                    let f = match f.get("$count") {
                        Some(_) => Document::new()
                            .with("$sum", Document::new().with("$const", 1))
                            .with("window", whole_partition()),
                        None => window_function(f),
                    };
                    fields.push(name, f);
                }
                Value::Document(fields)
            }
            _ => v.clone(),
        };
        out.push(k, v);
    }
    out
}

/// The window functions that take no window.
const WINDOWLESS: [&str; 7] =
    ["$rank", "$denseRank", "$documentNumber", "$shift", "$expMovingAvg", "$locf", "$linearFill"];

/// The window of the whole partition, the default of an accumulator.
fn whole_partition() -> Document {
    Document::new().with("documents", Value::Array(vec!["unbounded".into(), "unbounded".into()]))
}

/// A window function as the reference server keeps it: its argument optimised (named arguments as
/// given) and an accumulator over the whole partition unless it says its window (measured).
fn window_function(d: &Document) -> Document {
    let mut out = Document::new();
    let mut accumulator = false;
    for (k, v) in d.iter() {
        let v = match (k, v) {
            ("window", w) => w.clone(),
            (op, arg) => {
                accumulator = !WINDOWLESS.contains(&op);
                match arg {
                    Value::Document(named) if named.is_empty() => arg.clone(),
                    Value::Document(named)
                        if named.iter().next().is_some_and(|(x, _)| !x.starts_with('$')) =>
                    {
                        Value::Document(named_arguments(named))
                    }
                    other => optimized(other),
                }
            }
        };
        out.push(k, v);
    }
    if accumulator && out.get("window").is_none() {
        out.push("window", whole_partition());
    }
    out
}
