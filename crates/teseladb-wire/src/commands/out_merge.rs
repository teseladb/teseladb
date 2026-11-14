//! What `$out` and `$merge` write (010 FR-012, T029), as a `refrouter` 9.0.4 does (measured
//! 01-11-2026). `$out` writes the documents into a new collection of the target's database,
//! with the target's indexes, and renames it over the target at the end: a reader sees the old
//! documents or all the new ones. `$merge` writes document by document what
//! [`Merge::action`] says for its target.
//!
//! Both go through the node's own commands (`create`, `createIndexes`, `insert`, `update`,
//! `renameCollection`, `drop`), with their checks and index maintenance.

use teseladb_bson::{Document, Value};
use teseladb_exec::pipeline::Env;
use teseladb_exec::pipeline::out_merge::{Merge, MergeAction, Write};
use teseladb_meta::catalog::Namespace;

use super::mutation::from_mql;
use super::{Backend, Ctx, names};
use crate::error::{CommandError, DUPLICATE_KEY, INTERNAL_ERROR};

/// The database `renameCollection` runs on.
const ADMIN: &str = "admin";

/// The error RefDb gives when no unique index covers `$merge`'s `on` fields.
const NO_UNIQUE_INDEX: crate::error::Code =
    crate::error::Code { code: 51190, name: "Location51190" };

/// Writes `docs`, which reached stage `w` of an `aggregate` on database `ctx.db` with
/// variables `vars`; `executor` turns an error of the writes into the command's.
pub(super) fn write<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    w: &Write,
    docs: Vec<Document>,
    vars: Document,
    executor: impl Fn(CommandError) -> CommandError,
) -> Result<(), CommandError> {
    let env = super::engines::VarsEnv::new(vars);
    match w {
        Write::Out(out) => {
            out.check(ctx.db).map_err(from_mql)?;
            let ns = names::ns(out.db.as_deref().unwrap_or(ctx.db), &out.coll);
            replace(ctx, &ns, docs).map_err(executor)
        }
        Write::Merge(m) => {
            let ns = names::ns(m.db.as_deref().unwrap_or(ctx.db), &m.coll);
            let index = unique_index(ctx, &ns, &m.on_fields())?;
            merge(ctx, &ns, m, &index, docs, &env).map_err(executor)
        }
    }
}

/// Runs `cmd` on database `db` with the node's own command and its checks; an error of one of
/// its writes is the command's.
fn run<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    db: &str,
    cmd: &Document,
    command: fn(&mut Ctx<'_, B>) -> Result<Document, CommandError>,
) -> Result<Document, CommandError> {
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db, cmd };
    let reply = command(&mut sub)?;
    match reply.get("writeErrors") {
        Some(Value::Array(errors)) => Err(write_error(errors.first())),
        _ => Ok(reply),
    }
}

/// The first write error of a reply as an error.
fn write_error(e: Option<&Value>) -> CommandError {
    let (code, msg) = match e {
        Some(Value::Document(d)) => (
            match d.get("code") {
                Some(Value::Int32(n)) => crate::error::code(*n),
                _ => None,
            },
            match d.get("errmsg") {
                Some(Value::String(m)) => m.clone(),
                _ => String::new(),
            },
        ),
        _ => (None, String::new()),
    };
    CommandError::new(code.unwrap_or(INTERNAL_ERROR), msg)
}

/// `$out`: `docs` in a new collection with the target's indexes, renamed over the target.
fn replace<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    docs: Vec<Document>,
) -> Result<(), CommandError> {
    let uuid = super::new_uuid(ctx.backend);
    let tag: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = format!("tmp.agg_out.{tag}");
    run(ctx, &ns.db, &Document::new().with("create", tmp.as_str()), super::catalog::create)?;
    let filled = fill(ctx, ns, &tmp, docs);
    let renamed = filled.and_then(|()| {
        let rename = Document::new()
            .with("renameCollection", format!("{}.{tmp}", ns.db))
            .with("to", names::full(ns))
            .with("dropTarget", true);
        run(ctx, ADMIN, &rename, super::catalog::rename).map(drop)
    });
    if renamed.is_err() {
        let gone = Document::new().with("drop", tmp.as_str());
        run(ctx, &ns.db, &gone, super::catalog::drop)?;
    }
    renamed
}

/// The target's indexes and `docs` into collection `tmp`.
fn fill<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    tmp: &str,
    docs: Vec<Document>,
) -> Result<(), CommandError> {
    // Each without `_id` gets one now, in the pipeline's order.
    let docs = docs
        .iter()
        .map(|d| super::crud::with_id(ctx.backend, ctx.shared, d))
        .collect::<Result<Vec<_>, _>>()?;
    let indexes = target_indexes(ctx, ns)?;
    if !indexes.is_empty() {
        let create = Document::new().with("createIndexes", tmp).with("indexes", indexes);
        run(ctx, &ns.db, &create, super::indexes::create_indexes)?;
    }
    for chunk in docs.chunks(crate::MAX_WRITE_BATCH_SIZE as usize) {
        let list: Vec<Value> = chunk.iter().cloned().map(Value::Document).collect();
        let insert = Document::new().with("insert", tmp).with("documents", list);
        run(ctx, &ns.db, &insert, super::crud::insert)?;
    }
    Ok(())
}

/// The indexes of `ns` other than `_id`'s, as `listIndexes` gives them (none when it does not
/// exist).
fn target_indexes<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
) -> Result<Vec<Value>, CommandError> {
    if ctx.backend.collection(ns)?.is_none() {
        return Ok(Vec::new());
    }
    let list = Document::new().with("listIndexes", ns.coll.as_str());
    let reply = run(ctx, &ns.db, &list, super::catalog::list_indexes)?;
    let batch = match reply.get("cursor") {
        Some(Value::Document(c)) => match c.get("firstBatch") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    Ok(batch
        .into_iter()
        .filter(|i| !matches!(i, Value::Document(d) if d.get("name") == Some(&Value::from("_id_"))))
        .collect())
}

/// The name and the key pattern of a unique index of `ns` on exactly the fields `on` (`_id_`
/// for `_id`, which needs no index of its own).
fn unique_index<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    on: &[String],
) -> Result<(String, Document), CommandError> {
    if on == ["_id"] {
        return Ok(("_id_".to_owned(), Document::new().with("_id", 1)));
    }
    for index in target_indexes(ctx, ns)? {
        let Value::Document(d) = index else { continue };
        let unique = matches!(d.get("unique"), Some(Value::Boolean(true)));
        let keys: Vec<String> = match d.get("key") {
            Some(Value::Document(k)) => k.iter().map(|(f, _)| f.to_owned()).collect(),
            _ => Vec::new(),
        };
        let mut sorted = keys.clone();
        sorted.sort();
        let mut wanted = on.to_vec();
        wanted.sort();
        if unique
            && sorted == wanted
            && let (Some(Value::String(name)), Some(Value::Document(pattern))) =
                (d.get("name"), d.get("key"))
        {
            return Ok((name.clone(), pattern.clone()));
        }
    }
    Err(CommandError::new(
        NO_UNIQUE_INDEX,
        "Cannot find index to verify that join fields will be unique",
    ))
}

/// The filter of the target document of `doc` by fields `on`; `None` when `doc` lacks one.
fn on_filter(doc: &Document, on: &[String]) -> Option<Document> {
    let mut filter = Document::new();
    for f in on {
        filter.push(f, at_path(doc, f)?);
    }
    Some(filter)
}

/// The value at dotted path `path` of `doc`, through documents only.
fn at_path(doc: &Document, path: &str) -> Option<Value> {
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (path, None),
    };
    match (doc.get(head), rest) {
        (Some(v), None) => Some(v.clone()),
        (Some(Value::Document(inner)), Some(r)) => at_path(inner, r),
        _ => None,
    }
}

/// `$merge`: each document of `docs` into `ns` as `m` says.
fn merge<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    m: &Merge,
    index: &(String, Document),
    docs: Vec<Document>,
    env: &dyn Env,
) -> Result<(), CommandError> {
    let on = m.on_fields();
    for doc in docs {
        let filter = on_filter(&doc, &on);
        let existing = match &filter {
            Some(f) => {
                let stages = [Document::new().with("$match", f.clone())];
                let found = super::aggregate::read_through(
                    ctx.backend,
                    ns,
                    &stages,
                    None,
                    teseladb_range::read::ReadConcern::Local,
                )?;
                found.into_iter().next()
            }
            None => None,
        };
        match m.action(existing.as_ref(), doc, env).map_err(from_mql)? {
            MergeAction::Insert(d) => {
                let insert = Document::new()
                    .with("insert", ns.coll.as_str())
                    .with("documents", vec![Value::Document(d)]);
                run(ctx, &ns.db, &insert, super::crud::insert)?;
            }
            MergeAction::Replace(d) => {
                let id =
                    existing.as_ref().and_then(|e| e.get("_id")).cloned().unwrap_or(Value::Null);
                let statement =
                    Document::new().with("q", Document::new().with("_id", id)).with("u", d);
                let update = Document::new()
                    .with("update", ns.coll.as_str())
                    .with("updates", vec![Value::Document(statement)]);
                run(ctx, &ns.db, &update, super::crud::update)?;
            }
            MergeAction::Skip => {}
            MergeAction::Duplicate => {
                let key = filter.unwrap_or_default();
                return Err(duplicate(ns, index, &key));
            }
        }
    }
    Ok(())
}

/// `whenMatched: fail` on a document whose target has the same `on` fields `key`, under unique
/// index `(name, pattern)`; the reply carries `keyPattern` and `keyValue` in the index's order,
/// as a `refrouter` 9.0.4's does (measured).
fn duplicate(
    ns: &Namespace,
    (index, pattern): &(String, Document),
    key: &Document,
) -> CommandError {
    let mut value = Document::new();
    for (k, _) in pattern.iter() {
        value.push(k, key.get(k).cloned().unwrap_or(Value::Null));
    }
    let shown: Vec<String> =
        value.iter().map(|(k, v)| format!("{k}: {}", super::display(v))).collect();
    let msg = format!(
        "$merge with whenMatched: fail found an existing document with the same values for the 'on' fields :: caused by :: E11000 duplicate key error collection: {} index: {index} dup key: {{ {} }}",
        names::full(ns),
        shown.join(", ")
    );
    let mut e = CommandError::new(DUPLICATE_KEY, msg);
    e.extra.push("keyPattern", pattern.clone());
    e.extra.push("keyValue", value);
    e
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_carries_the_key_pattern_and_value() {
        // As a `refrouter` 9.0.4 answers it (measured on 03-11-2026), for the drivers to read.
        let ns = names::ns("db", "t");
        let id = ("_id_".to_owned(), Document::new().with("_id", 1));
        let e = duplicate(&ns, &id, &Document::new().with("_id", 2));
        assert_eq!(e.code.code, 11000);
        assert_eq!(e.extra.get("keyPattern"), Some(&Value::Document(id.1.clone())));
        assert_eq!(e.extra.get("keyValue"), Some(&Value::Document(Document::new().with("_id", 2))));
        // The index's order and directions, whatever the order of `on`.
        let ba = ("b_1_a_-1".to_owned(), Document::new().with("b", 1).with("a", -1));
        let e = duplicate(&ns, &ba, &Document::new().with("a", 1).with("b", 2));
        assert_eq!(e.extra.get("keyPattern"), Some(&Value::Document(ba.1.clone())));
        let value = Document::new().with("b", 2).with("a", 1);
        assert_eq!(e.extra.get("keyValue"), Some(&Value::Document(value)));
        assert!(e.errmsg.ends_with("index: b_1_a_-1 dup key: { b: 2, a: 1 }"), "{}", e.errmsg);
    }
}
