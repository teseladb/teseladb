//! `explain` of a read on a view (010 FR-003, 015 FR-001), as a `refrouter` 9.0.4 gives it (measured
//! 01-11-2026): the explain of the aggregation the read becomes on the collection the view reads
//! (the view's stages, then the read's), which it echoes as `command` in the form RefDb keeps
//! its stages (a `$match` as given), with the view's collation. A write on a view fails, after
//! the shard's name for `update` and `delete`.

use teseladb_meta::catalog::Namespace;
use teseladb_plan::views::Resolved;
use teseladb_bson::{Document, RawDoc, Value};

use super::super::{Backend, Ctx, names, views};
use super::{Verbosity, on_node};
use crate::error::{COMMAND_NOT_SUPPORTED_ON_VIEW, CommandError};

/// The explain of command `name` (`inner`) on view `ns`, which resolves to `view`.
pub(super) fn on_view<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    view: &Resolved,
    (name, inner): (&str, &Document),
    verbosity: Verbosity,
) -> Result<Document, CommandError> {
    let refused = CommandError::new(
        COMMAND_NOT_SUPPORTED_ON_VIEW,
        format!("Namespace {} is a view, not a collection", names::full(ns)),
    );
    let read: Vec<Document> = match name {
        "update" | "delete" => return Err(on_node(ctx, refused)),
        "findAndModify" | "findandmodify" => return Err(refused),
        "find" => views::find_stages(inner, &names::full(ns))?,
        "count" => views::count_stages(inner),
        "distinct" => match inner.get("query") {
            Some(Value::Document(q)) => views::distinct_stages(q),
            _ => Vec::new(),
        },
        _ => match inner.get("pipeline") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|i| match i {
                    Value::Document(d) => Some(d.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
    };
    let mut stages = Vec::new();
    for bytes in &view.pipeline {
        stages.push(Document::from(RawDoc::from_bytes(bytes)?));
    }
    stages.extend(read);
    let key = match inner.get("key") {
        Some(Value::String(k)) => k.clone(),
        _ => String::new(),
    };
    let (planned, shown) = match name {
        "count" => {
            let g = Document::new()
                .with("_id", Document::new().with("$const", Value::Null))
                .with("count", Document::new().with("$sum", Document::new().with("$const", 1)));
            let p = Document::new().with("count", true).with("_id", false);
            let mut planned = stages.clone();
            planned.push(Document::new().with("$group", g));
            planned.push(Document::new().with("$project", p));
            (planned.clone(), echoed(&planned))
        }
        "distinct" => {
            let mut shown = echoed(&stages);
            shown.extend(distinct_tail(&key));
            (stages, shown)
        }
        _ => (stages.clone(), echoed(&stages)),
    };
    let source = names::ns(&ns.db, &view.collection);
    let aggregate = Document::new()
        .with("aggregate", view.collection.as_str())
        .with("pipeline", Value::Array(planned.into_iter().map(Value::Document).collect()))
        .with("cursor", Document::new());
    let collation = match &view.collation {
        Some(bytes) => Document::from(RawDoc::from_bytes(bytes)?),
        None => Document::new().with("locale", "simple"),
    };
    let mut command = Document::new()
        .with("aggregate", view.collection.as_str())
        .with("pipeline", Value::Array(shown.iter().cloned().map(Value::Document).collect()))
        .with("cursor", Document::new())
        .with("collation", collation);
    if name != "count" {
        command.push(
            "readConcern",
            Document::new().with("level", "local").with("provenance", "implicitDefault"),
        );
    }
    let mut reply = super::aggregate::aggregate(ctx, &source, &aggregate, verbosity, command)?;
    if name == "distinct" {
        let indexes = match ctx.backend.collection(&source) {
            Ok(Some(c)) => super::super::plan_exec::plan_indexes(ctx.backend, &c)
                .map(|i| super::super::plan_exec::key_indexes(&i))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        reply = with_distinct_tail(reply, &distinct_tail(&key), &key, &indexes);
    }
    Ok(reply)
}

/// The stages as RefDb echoes them: `$match` and `$sort` as given, the others in the form it
/// keeps.
fn echoed(stages: &[Document]) -> Vec<Document> {
    stages
        .iter()
        .map(|st| super::steps::Step::new(st.clone()))
        .flat_map(|st| match st.kind {
            super::steps::Kind::Match(_) | super::steps::Kind::Sort(_) => vec![st.doc],
            _ => super::forms::forms(&st),
        })
        .collect()
}

/// The stages a `distinct` of `key` on a view ends with in RefDb: every value at the path,
/// unwound, gathered in one set.
fn distinct_tail(key: &str) -> Vec<Document> {
    let values = Document::new().with(
        "$_internalFindAllValuesAtPath",
        Value::Array(vec![Value::Document(Document::new().with("$const", key))]),
    );
    vec![
        Document::new().with(
            "$replaceRoot",
            Document::new().with("newRoot", Document::new().with("_internalUnwoundArray", values)),
        ),
        Document::new().with(
            "$unwind",
            Document::new()
                .with("path", "$_internalUnwoundArray")
                .with("preserveNullAndEmptyArrays", true),
        ),
        Document::new().with(
            "$group",
            Document::new()
                .with("_id", Document::new().with("$const", Value::Null))
                .with("distinct", Document::new().with("$addToSet", "$_internalUnwoundArray"))
                .with("$willBeMerged", false),
        ),
    ]
}

/// The reply of the explain of a `distinct` of `key` on a view: after the plan, which keeps
/// the key alone, come RefDb's own stages (`tail`).
fn with_distinct_tail(
    reply: Document,
    tail: &[Document],
    key: &str,
    indexes: &[super::super::plan_hash::KeyIndex],
) -> Document {
    let tail: Vec<Value> = tail.iter().cloned().map(Value::Document).collect();
    let mut out = Document::new();
    for (k, v) in reply.iter() {
        let v = match (k, v) {
            ("shards", Value::Document(s)) => {
                let mut shards = Document::new();
                for (node, section) in s.iter() {
                    if let Value::Document(sec) = section {
                        shards.push(node, distinct_section(sec, &tail, key, indexes));
                    }
                }
                Value::Document(shards)
            }
            (_, other) => other.clone(),
        };
        out.push(k, v);
    }
    out
}

/// A node's section of a `distinct` on a view: its plan under a `$cursor` keeping only `key`,
/// then RefDb's stages. The plan cache hashes are those of a read of `key` alone (measured on a
/// `refrouter` 9.0.4: a view on `{$match: {g: "a"}}` gives the shape `eqg||s||ftr`).
fn distinct_section(
    sec: &Document,
    tail: &[Value],
    key: &str,
    indexes: &[super::super::plan_hash::KeyIndex],
) -> Document {
    let mut out = Document::new();
    for (k, v) in sec.iter() {
        match k {
            "queryPlanner" | "executionStats" | "stages" | "explainVersion" => {}
            _ => out.push(k, v.clone()),
        }
    }
    out.push("explainVersion", "1");
    let mut planner = match sec.get("queryPlanner") {
        Some(Value::Document(q)) => q.clone(),
        _ => Document::new(),
    };
    let keep = Document::new().with(key, 1).with("_id", 0);
    let hashes = match planner.get("parsedQuery") {
        Some(Value::Document(parsed)) => {
            let input = super::super::plan_hash::ShapeInput {
                parsed: Some(parsed),
                original: Some(parsed),
                projection: Some(&keep),
                ..Default::default()
            };
            super::super::plan_hash::hashes(&input, indexes).to_vec()
        }
        _ => Vec::new(),
    };
    {
        let mut renamed = Document::new();
        for (k, v) in planner.iter() {
            match (k, v) {
                ("optimizedPipeline", _) => renamed.push("cursorType", "regular"),
                ("queryHash" | "planCacheShapeHash" | "planCacheKey", _) => {
                    match hashes.iter().find(|(h, _)| *h == k) {
                        Some((_, value)) => renamed.push(k, value.as_str()),
                        None => renamed.push(k, v.clone()),
                    }
                }
                ("winningPlan", Value::Document(w)) => {
                    let mut plan = Document::new().with("isCached", false);
                    let mut input = Document::new();
                    for (x, y) in w.iter().filter(|(x, _)| *x != "isCached") {
                        input.push(x, y.clone());
                    }
                    plan.push("stage", "PROJECTION_SIMPLE");
                    plan.push("transformBy", Document::new().with(key, 1).with("_id", 0));
                    plan.push("inputStage", input);
                    renamed.push(k, plan);
                }
                _ => renamed.push(k, v.clone()),
            }
        }
        planner = renamed;
    }
    let mut stages = vec![Value::Document(
        Document::new().with("$cursor", Document::new().with("queryPlanner", planner)),
    )];
    stages.extend(tail.iter().cloned());
    out.push("stages", Value::Array(stages));
    out
}
