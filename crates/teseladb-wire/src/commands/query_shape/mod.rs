//! Query shapes (010 FR-023, measured on the reference router 9.0.4, 01-11-2026): what a `find`,
//! `aggregate`, `count` or `distinct` asks once its literals are set aside. Its debug form
//! (`cmdNs`, `collation`, `let`, `command` and the command's own parts) is the key of
//! `$queryStats` and is shown by `$querySettings`; the SHA-256 of its representative form, in
//! upper-case hexadecimal, is the `queryShapeHash` that `explain`, `$queryStats` and the query
//! settings use.

mod expr;
mod filter;
pub mod literal;
mod stages;

use teseladb_bson::{Document, Value};

use literal::{Policy, literal};

/// The collection a collectionless `aggregate: 1` reads in its shape.
const COLLECTIONLESS: &str = "$cmd.aggregate";

/// The commands that have a query shape.
pub const SHAPED: &[&str] = &["find", "aggregate", "count", "distinct"];

/// A command's query shape.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    /// The debug form.
    pub debug: Document,
    /// The SHA-256 of the representative form, in upper-case hexadecimal.
    pub hash: String,
}

/// The query shape of command `cmd` on database `db`; `None` for a command without one or one
/// that does not parse.
pub fn shape(db: &str, cmd: &Document) -> Option<Shape> {
    let debug = form(db, cmd, Policy::Debug)?;
    let representative = form(db, cmd, Policy::Representative)?;
    Some(Shape { debug, hash: hash(&representative)? })
}

/// The `queryShapeHash` `explain` shows for command `cmd` on `db`: none for a `find` by `_id`
/// alone, which runs without planning (measured).
pub fn explained_hash(db: &str, cmd: &Document) -> Option<String> {
    let by_id = cmd.iter().next().is_some_and(|(k, _)| k == "find")
        && matches!(cmd.get("filter"), Some(Value::Document(f))
            if f.len() == 1 && matches!(f.get("_id"), Some(v) if !matches!(v,
                Value::Document(_) | Value::Array(_) | Value::Regex { .. })));
    if by_id {
        return None;
    }
    shape(db, cmd).map(|s| s.hash)
}

/// The representative form of command `cmd`'s shape on database `db`.
pub fn representative(db: &str, cmd: &Document) -> Option<Document> {
    form(db, cmd, Policy::Representative)
}

/// The upper-case hexadecimal SHA-256 of the BSON of `d`.
fn hash(d: &Document) -> Option<String> {
    let bytes = d.to_bytes().ok()?;
    Some(teseladb_hash::sha256(&bytes).iter().map(|b| format!("{b:02X}")).collect())
}

/// The form of command `cmd`'s shape under policy `p`.
fn form(db: &str, cmd: &Document, p: Policy) -> Option<Document> {
    let (name, target) = cmd.iter().next()?;
    if !SHAPED.contains(&name) {
        return None;
    }
    let coll = match target {
        Value::String(c) => c.clone(),
        _ if name == "aggregate" => COLLECTIONLESS.to_owned(),
        _ => return None,
    };
    let mut out =
        Document::new().with("cmdNs", Document::new().with("db", db).with("coll", coll.as_str()));
    if let Some(c) = cmd.get("collation") {
        out.push("collation", c.clone());
    }
    if let Some(Value::Document(vars)) = cmd.get("let")
        && (name == "aggregate" || !vars.is_empty())
    {
        let mut shaped = Document::new();
        for (k, v) in vars.iter() {
            shaped.push(k, expr::expression(v, p));
        }
        out.push("let", shaped);
    }
    out.push("command", name);
    match name {
        "aggregate" => aggregate_parts(cmd, &mut out, p)?,
        "find" => find_parts(cmd, &mut out, p)?,
        "count" => count_parts(cmd, &mut out, p)?,
        _ => distinct_parts(cmd, &mut out, p)?,
    }
    Some(out)
}

/// `pipeline` and `allowDiskUse` of an `aggregate`.
fn aggregate_parts(cmd: &Document, out: &mut Document, p: Policy) -> Option<()> {
    let Some(Value::Array(stages)) = cmd.get("pipeline") else { return None };
    out.push("pipeline", stages::pipeline(stages, p)?);
    if let Some(v @ Value::Boolean(_)) = cmd.get("allowDiskUse") {
        out.push("allowDiskUse", v.clone());
    }
    Some(())
}

/// A filter under `key`, `{}` when absent.
fn filter_of(cmd: &Document, key: &str, p: Policy) -> Option<Document> {
    match cmd.get(key) {
        Some(Value::Document(f)) => filter::filter(f, p),
        None | Some(Value::Null) => Some(Document::new()),
        Some(_) => None,
    }
}

/// The parts of a `find`.
fn find_parts(cmd: &Document, out: &mut Document, p: Policy) -> Option<()> {
    out.push("filter", filter_of(cmd, "filter", p)?);
    for (k, v) in cmd.iter() {
        match (k, v) {
            ("projection", Value::Document(proj)) if !proj.is_empty() => {
                out.push(k, stages_projection(proj, p)?);
            }
            ("sort", Value::Document(s)) if !s.is_empty() => out.push(k, s.clone()),
            ("skip" | "limit", _) => out.push(k, literal(v, p)),
            (
                "singleBatch"
                | "allowDiskUse"
                | "returnKey"
                | "showRecordId"
                | "tailable"
                | "awaitData"
                | "noCursorTimeout"
                | "allowPartialResults",
                Value::Boolean(_),
            ) => {
                out.push(k, v.clone());
            }
            ("min" | "max", Value::Document(_)) => out.push(k, literal(v, p)),
            _ => {}
        }
    }
    Some(())
}

/// A `find` projection as the `$project` stage shows it.
fn stages_projection(proj: &Document, p: Policy) -> Option<Value> {
    let st = Value::Document(Document::new().with("$project", proj.clone()));
    let mut shaped = stages::pipeline(std::slice::from_ref(&st), p)?;
    match shaped.pop() {
        Some(Value::Document(d)) => d.get("$project").cloned(),
        _ => None,
    }
}

/// The parts of a `count`.
fn count_parts(cmd: &Document, out: &mut Document, p: Policy) -> Option<()> {
    if let Some(Value::Document(q)) = cmd.get("query")
        && !q.is_empty()
    {
        out.push("query", filter::filter(q, p)?);
    }
    for k in ["limit", "skip"] {
        if let Some(v) = cmd.get(k) {
            out.push(k, literal(v, p));
        }
    }
    Some(())
}

/// The parts of a `distinct`.
fn distinct_parts(cmd: &Document, out: &mut Document, p: Policy) -> Option<()> {
    out.push("key", cmd.get("key")?.clone());
    if let Some(Value::Document(q)) = cmd.get("query")
        && !q.is_empty()
    {
        out.push("query", filter::filter(q, p)?);
    }
    Some(())
}
