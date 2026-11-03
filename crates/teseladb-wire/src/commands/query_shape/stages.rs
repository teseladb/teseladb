//! The stages of a pipeline in a query shape (010 FR-023, measured on a `refrouter` 9.0.4,
//! 01-11-2026): each as RefDb writes it after parsing it, with the stages that are others in
//! disguise as those others (`$documents` as a `$queue` and three stages that unwind it,
//! `$count` and `$sortByCount` as a `$group` and what follows it, `$unset` as a `$project`,
//! `$replaceWith` as `$replaceRoot`, `$setWindowFields` and `$densify` after the `$sort` they
//! need), and literals by policy.

use teseladb_bson::{Document, Value};

use super::expr::{accumulator_argument, expression, operator};
use super::filter::filter;
use super::literal::{Policy, kept_in_representative, literal};

/// The window functions that take no window.
const UNFRAMED: &[&str] =
    &["$rank", "$denseRank", "$documentNumber", "$linearFill", "$expMovingAvg", "$shift", "$locf"];
/// The bound of a window that has no end.
const UNBOUNDED: &str = "unbounded";
/// The field `$documents` unwinds its documents from.
const DOCUMENTS_FIELD: &str = "_tempDocumentsField";

/// Pipeline `stages` in a shape; `None` if a stage is not a one-field document or a filter does
/// not parse.
pub fn pipeline(stages: &[Value], p: Policy) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    for st in stages {
        let Value::Document(d) = st else { return None };
        let (name, spec) = single(d)?;
        out.extend(stage(name, spec, p)?.into_iter().map(Value::Document));
    }
    Some(out)
}

/// The one field of `d`.
fn single(d: &Document) -> Option<(&str, &Value)> {
    let mut it = d.iter();
    match (it.next(), it.next()) {
        (Some((k, v)), None) => Some((k, v)),
        _ => None,
    }
}

/// `{name: spec}`.
fn one(name: &str, spec: impl Into<Value>) -> Document {
    Document::new().with(name, spec.into())
}

/// Stage `name` with specification `spec`, as the stages it is in a shape.
fn stage(name: &str, spec: &Value, p: Policy) -> Option<Vec<Document>> {
    let doc = match spec {
        Value::Document(d) => Some(d),
        _ => None,
    };
    let out = match (name, spec) {
        ("$match", Value::Document(f)) => vec![one(name, filter(f, p)?)],
        ("$project", Value::Document(proj)) => vec![one(name, projection(proj, p))],
        ("$unset", _) => vec![one("$project", unset(spec))],
        ("$addFields" | "$set", Value::Document(fields)) => vec![one(name, computed(fields, p))],
        ("$replaceRoot", Value::Document(d)) => {
            let root = expression(d.get("newRoot")?, p);
            vec![one(name, one("newRoot", root))]
        }
        ("$replaceWith", v) => vec![one("$replaceRoot", one("newRoot", expression(v, p)))],
        ("$group", Value::Document(g)) => vec![one(name, group(g, p))],
        ("$sort", Value::Document(s)) => vec![one(name, sort(s))],
        ("$limit" | "$skip", v) => vec![one(name, literal(v, p))],
        ("$sample", Value::Document(s)) => vec![one(name, one("size", literal(s.get("size")?, p)))],
        ("$unwind", _) => vec![one(name, unwind(spec)?)],
        ("$count", Value::String(field)) => count(field, p),
        ("$sortByCount", v) => sort_by_count(v, p),
        ("$documents", v) => documents(v, p),
        ("$redact", v) => vec![one(name, expression(v, p))],
        ("$bucketAuto", Value::Document(b)) => vec![one(name, bucket_auto(b, p))],
        ("$lookup", Value::Document(l)) => vec![one(name, lookup(l, p)?)],
        ("$graphLookup", Value::Document(g)) => vec![one(name, graph_lookup(g, p))],
        ("$facet", Value::Document(f)) => vec![one(name, facet(f, p)?)],
        ("$unionWith", _) => vec![one(name, union_with(spec, p)?)],
        ("$densify", Value::Document(dn)) => densify(dn, p),
        ("$setWindowFields", Value::Document(w)) => set_window_fields(w, p),
        _ => vec![one(name, doc.map_or_else(|| literal(spec, p), |d| generic(d, p)))],
    };
    Some(out)
}

/// A stage this module does not know: its values as literals, its nested documents walked.
fn generic(d: &Document, p: Policy) -> Value {
    let mut out = Document::new();
    for (k, v) in d.iter() {
        let x = match v {
            Value::Document(n) => generic(n, p),
            other => literal(other, p),
        };
        out.push(k, x);
    }
    Value::Document(out)
}

/// Whether a `$project` value is an inclusion or exclusion flag.
fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Boolean(b) => Some(*b),
        Value::Int32(n) => Some(*n != 0),
        Value::Int64(n) => Some(*n != 0),
        Value::Double(x) => Some(*x != 0.0),
        Value::Decimal128(_) => Some(true),
        _ => None,
    }
}

/// Puts `value` at dotted `path` of `d`, nesting documents.
fn put(d: &mut Document, path: &str, value: Value) {
    match path.split_once('.') {
        None => d.push(path, value),
        Some((head, rest)) => {
            if let Some(Value::Document(inner)) = d.get_mut(head) {
                put(inner, rest, value);
                return;
            }
            let mut inner = Document::new();
            put(&mut inner, rest, value);
            d.push(head, inner);
        }
    }
}

/// A `$project`. An inclusion writes an implicit `_id` first, its flags, its computed fields and
/// then an explicit `_id`; an exclusion writes its fields from the last to the first and then
/// `_id`.
fn projection(proj: &Document, p: Policy) -> Document {
    let fields: Vec<(&str, &Value)> = proj.iter().filter(|(k, _)| *k != "_id").collect();
    let exclusion = if fields.is_empty() {
        proj.get("_id").and_then(flag) == Some(false)
    } else {
        fields.iter().all(|(_, v)| flag(v) == Some(false))
    };
    let id = proj.get("_id").map(|v| match flag(v) {
        Some(b) => Value::Boolean(b),
        None => expression(v, p),
    });
    let mut out = Document::new();
    if exclusion {
        for (k, _) in fields.iter().rev() {
            put(&mut out, k, Value::Boolean(false));
        }
        out.push("_id", id.unwrap_or(Value::Boolean(true)));
        return out;
    }
    if id.is_none() {
        out.push("_id", true);
    }
    for (k, v) in &fields {
        if let Some(b) = flag(v) {
            put(&mut out, k, Value::Boolean(b));
        }
    }
    for (k, v) in &fields {
        if flag(v).is_none() {
            put(&mut out, k, expression(v, p));
        }
    }
    if let Some(v) = id {
        out.push("_id", v);
    }
    out
}

/// `$unset` as the `$project` that drops its fields, `_id` first.
fn unset(spec: &Value) -> Document {
    let fields: Vec<&str> = match spec {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut out = Document::new();
    if fields.contains(&"_id") {
        out.push("_id", false);
    }
    for f in fields.into_iter().filter(|f| *f != "_id") {
        put(&mut out, f, Value::Boolean(false));
    }
    out
}

/// The fields of `$addFields` or `$set`.
fn computed(fields: &Document, p: Policy) -> Document {
    let mut out = Document::new();
    for (k, v) in fields.iter() {
        put(&mut out, k, expression(v, p));
    }
    out
}

/// An accumulator `{$op: arg}`.
fn accumulator(v: &Value, p: Policy) -> Value {
    match v {
        Value::Document(d) => match operator(d) {
            Some((_, Value::Document(named))) if operator(named).is_none() => expression(v, p),
            Some((op, arg)) => Value::Document(one(op, accumulator_argument(arg, p))),
            None => expression(v, p),
        },
        other => expression(other, p),
    }
}

/// A `$group`.
fn group(g: &Document, p: Policy) -> Document {
    let mut out = Document::new();
    for (k, v) in g.iter() {
        let x = if k == "_id" { expression(v, p) } else { accumulator(v, p) };
        out.push(k, x);
    }
    out
}

/// A `$sort`: the directions as given, a `$meta` key under a computed name.
fn sort(s: &Document) -> Document {
    let mut out = Document::new();
    let mut computed = 0;
    for (k, v) in s.iter() {
        match v {
            Value::Document(_) => {
                out.push(format!("$computed{computed}"), v.clone());
                computed += 1;
            }
            other => out.push(k, other.clone()),
        }
    }
    out
}

/// An `$unwind` as its document.
fn unwind(spec: &Value) -> Option<Document> {
    match spec {
        Value::String(path) => Some(one("path", path.as_str())),
        Value::Document(d) => {
            let mut out = one("path", d.get("path")?.clone());
            for k in ["includeArrayIndex", "preserveNullAndEmptyArrays"] {
                if let Some(v) = d.get(k) {
                    out.push(k, v.clone());
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// `$count: field`.
fn count(field: &str, p: Policy) -> Vec<Document> {
    let g = Document::new()
        .with("_id", expression(&Value::Null, p))
        .with(field, one("$sum", accumulator_argument(&Value::Int32(1), p)));
    let proj = Document::new().with(field, true).with("_id", false);
    vec![one("$group", g), one("$project", proj)]
}

/// `$sortByCount: v`.
fn sort_by_count(v: &Value, p: Policy) -> Vec<Document> {
    let g = Document::new()
        .with("_id", expression(v, p))
        .with("count", one("$sum", accumulator_argument(&Value::Int32(1), p)));
    vec![one("$group", g), one("$sort", one("count", -1))]
}

/// `$documents: v`.
fn documents(v: &Value, p: Policy) -> Vec<Document> {
    let field = format!("${DOCUMENTS_FIELD}");
    let proj = Document::new().with("_id", true).with(DOCUMENTS_FIELD, expression(v, p));
    vec![
        one("$queue", literal(v, p)),
        one("$project", proj),
        one("$unwind", one("path", field.as_str())),
        one("$replaceRoot", one("newRoot", field.as_str())),
    ]
}

/// The accumulators of an `output` document.
fn outputs(o: &Document, p: Policy) -> Document {
    let mut out = Document::new();
    for (k, v) in o.iter() {
        out.push(k, accumulator(v, p));
    }
    out
}

/// Pushes field `k` of `out` with `v`, when there is one.
fn push_some(out: &mut Document, k: &str, v: Option<Value>) {
    if let Some(v) = v {
        out.push(k, v);
    }
}

/// A `$bucketAuto`.
fn bucket_auto(b: &Document, p: Policy) -> Document {
    let mut out = Document::new();
    push_some(&mut out, "groupBy", b.get("groupBy").map(|g| expression(g, p)));
    push_some(&mut out, "buckets", b.get("buckets").map(|n| literal(n, p)));
    let granularity = b.get("granularity").map(|g| kept_in_representative(g, p));
    push_some(&mut out, "granularity", granularity);
    match b.get("output") {
        Some(Value::Document(o)) => out.push("output", outputs(o, p)),
        _ => {
            let count = one("$sum", accumulator_argument(&Value::Int32(1), p));
            out.push("output", one("count", count));
        }
    }
    out
}

/// A `$lookup`.
fn lookup(l: &Document, p: Policy) -> Option<Document> {
    let mut out = Document::new();
    for k in ["from", "as", "localField", "foreignField"] {
        push_some(&mut out, k, l.get(k).cloned());
    }
    if let Some(Value::Array(stages)) = l.get("pipeline") {
        let vars = match l.get("let") {
            Some(Value::Document(vars)) => computed(vars, p),
            _ => Document::new(),
        };
        out.push("let", vars);
        out.push("pipeline", pipeline(stages, p)?);
    }
    Some(out)
}

/// A `$graphLookup`.
fn graph_lookup(g: &Document, p: Policy) -> Document {
    let mut out = Document::new();
    for k in ["from", "as", "connectToField", "connectFromField"] {
        push_some(&mut out, k, g.get(k).cloned());
    }
    push_some(&mut out, "startWith", g.get("startWith").map(|v| expression(v, p)));
    push_some(&mut out, "maxDepth", g.get("maxDepth").map(|v| literal(v, p)));
    push_some(&mut out, "depthField", g.get("depthField").cloned());
    if let Some(Value::Document(f)) = g.get("restrictSearchWithMatch")
        && let Some(r) = filter(f, p)
    {
        out.push("restrictSearchWithMatch", r);
    }
    out
}

/// A `$facet`.
fn facet(f: &Document, p: Policy) -> Option<Document> {
    let mut out = Document::new();
    for (k, v) in f.iter() {
        let Value::Array(stages) = v else { return None };
        out.push(k, pipeline(stages, p)?);
    }
    Some(out)
}

/// A `$unionWith`.
fn union_with(spec: &Value, p: Policy) -> Option<Document> {
    match spec {
        Value::String(coll) => Some(one("coll", coll.as_str())),
        Value::Document(d) => {
            let mut out = Document::new();
            push_some(&mut out, "coll", d.get("coll").cloned());
            if let Some(Value::Array(stages)) = d.get("pipeline") {
                out.push("pipeline", pipeline(stages, p)?);
            }
            Some(out)
        }
        _ => None,
    }
}

/// A `$densify`, after the `$sort` by its partitions and field.
fn densify(dn: &Document, p: Policy) -> Vec<Document> {
    let field = match dn.get("field") {
        Some(Value::String(f)) => f.clone(),
        _ => String::new(),
    };
    let parts: Vec<Value> = match dn.get("partitionByFields") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    // The partitions sort first, unless the bounds are the whole range (measured on a `refrouter`
    // and a `refnode` 9.0.4).
    let full = matches!(dn.get("range"), Some(Value::Document(r)) if r.get("bounds") == Some(&Value::from("full")));
    let mut sort_key = Document::new();
    for part in parts.iter().filter(|_| !full).filter_map(|v| match v {
        Value::String(s) => Some(s),
        _ => None,
    }) {
        sort_key.push(part.as_str(), 1);
    }
    sort_key.push(field.as_str(), 1);
    let mut range = Document::new();
    if let Some(Value::Document(r)) = dn.get("range") {
        push_some(&mut range, "step", r.get("step").map(|s| literal(s, p)));
        match r.get("bounds") {
            Some(Value::Array(b)) => {
                range.push("bounds", Value::Array(b.iter().map(|x| literal(x, p)).collect()));
            }
            Some(other) => range.push("bounds", other.clone()),
            None => {}
        }
        push_some(&mut range, "unit", r.get("unit").cloned());
    }
    let d = Document::new()
        .with("field", field.as_str())
        .with("partitionByFields", Value::Array(parts))
        .with("range", range);
    vec![one("$sort", sort_key), one("$_internalDensify", d)]
}

/// A window function's `window`: named bounds as given, numbers as literals.
fn window(w: &Document, p: Policy) -> Document {
    let mut out = Document::new();
    for (k, v) in w.iter() {
        let x = match v {
            Value::Array(bounds) => Value::Array(
                bounds
                    .iter()
                    .map(|b| match b {
                        Value::String(_) => b.clone(),
                        other => literal(other, p),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        out.push(k, x);
    }
    out
}

/// A `$setWindowFields`, after the `$sort` by its partition and order.
fn set_window_fields(w: &Document, p: Policy) -> Vec<Document> {
    let mut sort_key = Document::new();
    if let Some(Value::String(path)) = w.get("partitionBy")
        && let Some(f) = path.strip_prefix('$')
    {
        sort_key.push(f, 1);
    }
    if let Some(Value::Document(s)) = w.get("sortBy") {
        for (k, v) in s.iter() {
            sort_key.push(k, v.clone());
        }
    }
    let mut d = Document::new();
    push_some(&mut d, "partitionBy", w.get("partitionBy").map(|v| expression(v, p)));
    push_some(&mut d, "sortBy", w.get("sortBy").cloned());
    let mut output = Document::new();
    if let Some(Value::Document(o)) = w.get("output") {
        for (k, f) in o.iter() {
            let Value::Document(fd) = f else { continue };
            let mut fun = Document::new();
            let mut framed = false;
            for (fk, fv) in fd.iter() {
                match (fk, fv) {
                    ("window", Value::Document(win)) => {
                        framed = true;
                        fun.push("window", window(win, p));
                    }
                    _ => {
                        if let Value::Document(a) =
                            accumulator(&Value::Document(one(fk, fv.clone())), p)
                        {
                            for (ak, av) in a.iter() {
                                fun.push(ak, av.clone());
                            }
                        }
                    }
                }
            }
            if !framed && !fd.iter().any(|(fk, _)| UNFRAMED.contains(&fk)) {
                let whole = Value::Array(vec![Value::from(UNBOUNDED), Value::from(UNBOUNDED)]);
                fun.push("window", one("documents", whole));
            }
            output.push(k, fun);
        }
    }
    d.push("output", output);
    let mut out = Vec::new();
    if !sort_key.is_empty() {
        out.push(one("$sort", sort_key));
    }
    out.push(one("$_internalSetWindowFields", d));
    out
}
