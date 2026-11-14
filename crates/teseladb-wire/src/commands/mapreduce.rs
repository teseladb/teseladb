//! `mapReduce` (016 FR-014, D-036, DV-021), as the reference router 9.0.4 runs it (measured on
//! 02-11): `query`, `sort` and `limit` choose the documents, `map` emits pairs with each document
//! as `this`, `reduce` runs on the values of every key (one value too), `finalize` gets each key's
//! result, `scope` gives the functions globals, and `out` is `{inline: 1}` or a collection the
//! results replace, merge into (by `_id`) or reduce into (with the value already there).
//! `jsMode`, `verbose`, `collation`, `bypassDocumentValidation` and `writeConcern` are read and
//! change nothing in the result.

use teseladb_bson::{Document, Value};
use teseladb_meta::catalog::Namespace;
use teseladb_range::read::ReadConcern;

use super::{Backend, Ctx};
use crate::error::{self, CommandError};

/// The command's name.
pub const MAP_REDUCE: &str = "mapReduce";

/// Where the results go.
enum Out {
    Inline,
    Replace(Namespace),
    Merge(Namespace),
    Reduce(Namespace),
}

fn missing(field: &str) -> CommandError {
    CommandError::new(
        error::IDL_FAILED_TO_PARSE,
        format!("BSON field 'mapReduce.{field}' is missing but a required field"),
    )
}

fn out_of(db: &str, v: &Value) -> Result<Out, CommandError> {
    let ns = |coll: &str, d: &Document| Namespace {
        db: match d.get("db") {
            Some(Value::String(other)) => other.clone(),
            _ => db.to_owned(),
        },
        coll: coll.to_owned(),
    };
    match v {
        Value::String(coll) => {
            Ok(Out::Replace(Namespace { db: db.to_owned(), coll: coll.clone() }))
        }
        Value::Document(d) => {
            for (k, x) in d.iter() {
                match (k, x) {
                    ("inline", _) => return Ok(Out::Inline),
                    ("replace", Value::String(c)) => return Ok(Out::Replace(ns(c, d))),
                    ("merge", Value::String(c)) => return Ok(Out::Merge(ns(c, d))),
                    ("reduce", Value::String(c)) => return Ok(Out::Reduce(ns(c, d))),
                    _ => {}
                }
            }
            Err(CommandError::new(error::BAD_VALUE, "'out' has to be a string or an object"))
        }
        _ => Err(CommandError::new(error::BAD_VALUE, "'out' has to be a string or an object")),
    }
}

/// The error of a function that failed, as the reference router words it.
fn js_error(ns: &Namespace, e: teseladb_mql::MqlError) -> CommandError {
    let mut c = super::mutation::from_mql(e);
    if c.code.code == super::js::JS_DISABLED.code {
        c.errmsg = format!(
            "Executor error during MapReduce command on namespace: {}.{} :: caused by :: {}",
            ns.db, ns.coll, c.errmsg
        );
    } else if c.code.code != error::MAX_TIME_MS_EXPIRED.code {
        c.errmsg = format!(
            "MapReduce internal error :: caused by :: Executor error during aggregate command on namespace: {}.{} :: caused by :: {}",
            ns.db, ns.coll, c.errmsg
        );
    }
    c
}

/// `mapReduce`.
pub fn map_reduce<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let cmd = ctx.cmd;
    let coll = match cmd.get(MAP_REDUCE) {
        Some(Value::String(c)) => c.clone(),
        _ => return Err(missing(MAP_REDUCE)),
    };
    let map = cmd.get("map").cloned().ok_or_else(|| missing("map"))?;
    let reduce = cmd.get("reduce").cloned().ok_or_else(|| missing("reduce"))?;
    let out = out_of(ctx.db, cmd.get("out").ok_or_else(|| missing("out"))?)?;
    let finalize = cmd.get("finalize").filter(|v| !matches!(v, Value::Null)).cloned();
    let scope = match cmd.get("scope") {
        Some(Value::Document(d)) => Some(d.clone()),
        _ => None,
    };
    let ns = Namespace { db: ctx.db.to_owned(), coll };
    if !super::js::is_enabled() {
        let e = teseladb_mql::MqlError::new(
            super::js::JS_DISABLED,
            "Cannot run server-side javascript without the javascript engine enabled",
        );
        return Err(js_error(&ns, e));
    }
    if let Out::Replace(t) | Out::Merge(t) | Out::Reduce(t) = &out
        && t.db != ctx.db
        && !super::catalog::has_database(ctx.backend, &t.db)?
    {
        return Err(CommandError::new(
            error::NAMESPACE_NOT_FOUND,
            format!("database {} not found", t.db),
        ));
    }
    let docs = chosen(ctx.backend, &ns, cmd)?;
    let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
    for d in &docs {
        let mut emitted = Vec::new();
        let mut emit = |k: Value, v: Value| emitted.push((k, v));
        super::js::call_map_reduce(&map, Some(d), &[], Some(&mut emit), scope.as_ref())
            .map_err(|e| js_error(&ns, e))?;
        for (k, v) in emitted {
            match groups.iter_mut().find(|(g, _)| *g == k) {
                Some((_, vs)) => vs.push(v),
                None => groups.push((k, vec![v])),
            }
        }
    }
    let reduce_values = |key: &Value, values: Vec<Value>| {
        super::js::call_map_reduce(
            &reduce,
            None,
            &[key.clone(), Value::Array(values)],
            None,
            scope.as_ref(),
        )
        .map_err(|e| js_error(&ns, e))
    };
    let mut results: Vec<(Value, Value)> = Vec::with_capacity(groups.len());
    for (k, vs) in groups {
        let v = reduce_values(&k, vs)?;
        results.push((k, v));
    }
    if let Out::Reduce(t) = &out {
        let existing = read(ctx.backend, t, &[])?;
        for (k, v) in &mut results {
            if let Some(old) = existing.iter().find(|d| d.get("_id") == Some(k)) {
                let before = old.get("value").cloned().unwrap_or(Value::Null);
                *v = reduce_values(k, vec![before, v.clone()])?;
            }
        }
    }
    if let Some(f) = &finalize {
        for (k, v) in &mut results {
            *v = super::js::call_map_reduce(f, None, &[k.clone(), v.clone()], None, scope.as_ref())
                .map_err(|e| js_error(&ns, e))?;
        }
    }
    let docs: Vec<Value> = results
        .into_iter()
        .map(|(k, v)| Value::Document(Document::new().with("_id", k).with("value", v)))
        .collect();
    let target = match out {
        Out::Inline => {
            return Ok(Document::new().with("results", Value::Array(docs)).with("ok", 1.0));
        }
        Out::Replace(t) => {
            let drop = Document::new().with("drop", t.coll.as_str());
            match super::catalog::drop(&mut sub(ctx, &t.db, &drop)) {
                Ok(_) => {}
                Err(e) if e.code == error::NAMESPACE_NOT_FOUND => {}
                Err(e) => return Err(e),
            }
            if !docs.is_empty() {
                let insert = Document::new()
                    .with("insert", t.coll.as_str())
                    .with("documents", Value::Array(docs));
                checked(super::crud::insert(&mut sub(ctx, &t.db, &insert))?)?;
            }
            t
        }
        Out::Merge(t) | Out::Reduce(t) => {
            let updates: Vec<Value> = docs
                .into_iter()
                .map(|d| {
                    let id = match &d {
                        Value::Document(x) => x.get("_id").cloned().unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                    Value::Document(
                        Document::new()
                            .with("q", Document::new().with("_id", id))
                            .with("u", d)
                            .with("upsert", true),
                    )
                })
                .collect();
            if !updates.is_empty() {
                let update = Document::new()
                    .with("update", t.coll.as_str())
                    .with("updates", Value::Array(updates));
                checked(super::crud::update(&mut sub(ctx, &t.db, &update))?)?;
            }
            t
        }
    };
    let result = if target.db == ctx.db {
        Value::from(target.coll.as_str())
    } else {
        Value::Document(
            Document::new().with("db", target.db.as_str()).with("collection", target.coll.as_str()),
        )
    };
    Ok(Document::new().with("result", result).with("ok", 1.0))
}

/// A command `cmd` on database `db` with the node of `ctx`.
fn sub<'a, B: Backend + ?Sized>(
    ctx: &'a mut Ctx<'_, B>,
    db: &'a str,
    cmd: &'a Document,
) -> Ctx<'a, B> {
    Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db, cmd }
}

/// The reply of a write, or its first write error.
fn checked(reply: Document) -> Result<(), CommandError> {
    match reply.get("writeErrors") {
        Some(Value::Array(errs)) if !errs.is_empty() => {
            let (code, msg) = match &errs[0] {
                Value::Document(e) => (
                    match e.get("code") {
                        Some(Value::Int32(c)) => error::code(*c).unwrap_or(error::INTERNAL_ERROR),
                        _ => error::INTERNAL_ERROR,
                    },
                    match e.get("errmsg") {
                        Some(Value::String(m)) => m.clone(),
                        _ => String::new(),
                    },
                ),
                _ => (error::INTERNAL_ERROR, String::new()),
            };
            Err(CommandError::new(code, msg))
        }
        _ => Ok(()),
    }
}

/// The documents `query`, `sort` and `limit` of `cmd` choose in `ns`.
fn chosen<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    cmd: &Document,
) -> Result<Vec<Document>, CommandError> {
    let mut stages = Vec::new();
    if let Some(Value::Document(q)) = cmd.get("query") {
        stages.push(Document::new().with("$match", q.clone()));
    }
    if let Some(Value::Document(s)) =
        cmd.get("sort").filter(|s| matches!(s, Value::Document(d) if !d.is_empty()))
    {
        stages.push(Document::new().with("$sort", s.clone()));
    }
    match cmd.get("limit") {
        Some(Value::Int32(n)) if *n > 0 => {
            stages.push(Document::new().with("$limit", i64::from(*n)))
        }
        Some(Value::Int64(n)) if *n > 0 => stages.push(Document::new().with("$limit", *n)),
        Some(Value::Double(x)) if *x >= 1.0 => {
            stages.push(Document::new().with("$limit", *x as i64))
        }
        _ => {}
    }
    read(backend, ns, &stages)
}

fn read<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    stages: &[Document],
) -> Result<Vec<Document>, CommandError> {
    super::aggregate::read_through(backend, ns, stages, None, ReadConcern::Local)
}
