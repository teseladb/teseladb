//! The stages of a pipeline that `explain` checked (010 FR-003, T018), each read once into its
//! kind with the parts of its specification the plan uses, in the types the check guarantees:
//! a `$match` holds its filter, an `$unwind` its path, a `$group` its key and accumulators. The
//! functions that build the plan match on the kind and never meet a stage RefDb refuses.
//!
//! A stage keeps the document it came from too, which the engine reads when `explain` runs the
//! pipeline's first stages.

use teseladb_bson::{Document, Value};

use super::whole;

/// One stage of a checked pipeline.
#[derive(Clone, Debug)]
pub(super) struct Step {
    /// The stage as written.
    pub(super) doc: Document,
    /// What it is.
    pub(super) kind: Kind,
}

/// What a stage is, with the parts of its specification the plan uses.
#[derive(Clone, Debug)]
pub(super) enum Kind {
    Match(Document),
    Sort(Document),
    Skip(u64),
    Limit(u64),
    Project(Document),
    /// `$addFields` and `$set`.
    AddFields(Document),
    Unset(Vec<String>),
    /// `$replaceRoot`'s `newRoot` and `$replaceWith`'s expression.
    ReplaceRoot(Value),
    Group(Vec<GroupEntry>),
    Count(String),
    SortByCount(Value),
    Bucket(Bucket),
    BucketAuto(BucketAuto),
    Facet(Vec<(String, Vec<Step>)>),
    Lookup(Lookup),
    GraphLookup(Document),
    Unwind(Unwind),
    UnionWith(UnionWith),
    SetWindowFields(Window),
    Fill(Fill),
    Densify(Densify),
    Sample(u64),
    Redact(Value),
    /// A stage the plan shows as written.
    Other,
}

/// An accumulator: its operator and argument.
#[derive(Clone, Debug)]
pub(super) struct Accumulator {
    pub(super) op: String,
    pub(super) arg: Value,
}

/// An entry of a `$group`, in its place.
#[derive(Clone, Debug)]
pub(super) enum GroupEntry {
    Key(Value),
    Field(String, Accumulator),
}

/// `$bucket`.
#[derive(Clone, Debug)]
pub(super) struct Bucket {
    pub(super) group_by: Value,
    pub(super) boundaries: Vec<Value>,
    pub(super) default: Option<Value>,
    pub(super) output: Option<Vec<(String, Accumulator)>>,
}

/// `$bucketAuto`.
#[derive(Clone, Debug)]
pub(super) struct BucketAuto {
    pub(super) group_by: Value,
    pub(super) buckets: Value,
    pub(super) granularity: Option<Value>,
    pub(super) output: Option<Vec<(String, Accumulator)>>,
}

/// `$lookup`.
#[derive(Clone, Debug)]
pub(super) struct Lookup {
    pub(super) from: Option<String>,
    pub(super) as_field: String,
    pub(super) local_field: Option<Value>,
    pub(super) foreign_field: Option<Value>,
    pub(super) let_vars: Option<Value>,
    pub(super) pipeline: Option<Vec<Step>>,
}

impl Lookup {
    /// Whether it is an equality of fields without a pipeline of its own.
    pub(super) fn is_equality(&self) -> bool {
        self.local_field.is_some()
            && self.foreign_field.is_some()
            && self.pipeline.as_ref().is_none_or(Vec::is_empty)
    }
}

/// `$unwind`.
#[derive(Clone, Debug)]
pub(super) struct Unwind {
    /// The path, with its `$`.
    pub(super) path: String,
    pub(super) preserve: Option<Value>,
    pub(super) index: Option<String>,
}

impl Unwind {
    /// The path without its `$`.
    pub(super) fn field(&self) -> &str {
        self.path.strip_prefix('$').unwrap_or(&self.path)
    }
}

/// `$unionWith`.
#[derive(Clone, Debug)]
pub(super) struct UnionWith {
    pub(super) coll: Option<String>,
    pub(super) pipeline: Vec<Value>,
}

/// `$setWindowFields`: its fields in their order, the output's functions read.
#[derive(Clone, Debug)]
pub(super) struct Window {
    pub(super) partition_by: Option<Value>,
    pub(super) sort_by: Option<Document>,
    pub(super) output: Vec<(String, Document)>,
    /// The specification as written.
    pub(super) spec: Document,
}

/// What a `$fill` writes in a field.
#[derive(Clone, Debug)]
pub(super) enum FillWith {
    Value(Value),
    Linear,
    Locf,
}

/// `$fill`.
#[derive(Clone, Debug)]
pub(super) struct Fill {
    pub(super) partition_by: Option<Value>,
    pub(super) sort_by: Option<Document>,
    pub(super) output: Vec<(String, FillWith)>,
}

impl Fill {
    /// Whether it only sets values (no method).
    pub(super) fn fills_values(&self) -> bool {
        self.output.iter().all(|(_, w)| matches!(w, FillWith::Value(_)))
    }
}

/// `$densify`.
#[derive(Clone, Debug)]
pub(super) struct Densify {
    pub(super) field: String,
    pub(super) partition_by_fields: Vec<String>,
    pub(super) range: Value,
}

/// The documents among `items`.
pub(super) fn documents(items: &[Value]) -> impl Iterator<Item = &Document> {
    items.iter().filter_map(|i| match i {
        Value::Document(d) => Some(d),
        _ => None,
    })
}

/// The strings among `items`.
fn strings(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// The stages of `items`, each read, as RefDb rewrites them before planning.
pub(super) fn of_values(items: &[Value]) -> Vec<Step> {
    super::aggregate::swapped(documents(items).map(|d| Step::new(d.clone())).collect())
}

/// The field a `$lookup` without a collection holds its `$documents` in.
const DOCUMENTS_FIELD: &str = "_tempDocumentsField";

/// A pipeline that starts with `$documents` as RefDb runs it inside a `$lookup` (measured):
/// one empty document whose field holds the documents, unwound and made the root.
fn without_documents(items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, st) in items.iter().enumerate() {
        match (i, st) {
            (0, Value::Document(d)) if d.get("$documents").is_some() => {
                let docs = d.get("$documents").cloned().unwrap_or(Value::Null);
                let path = format!("${DOCUMENTS_FIELD}");
                out.extend(
                    [
                        Document::new().with("$queue", vec![Value::Document(Document::new())]),
                        Document::new().with(
                            "$project",
                            Document::new().with("_id", true).with(DOCUMENTS_FIELD, docs),
                        ),
                        Document::new()
                            .with("$unwind", Document::new().with("path", path.as_str())),
                        Document::new()
                            .with("$replaceRoot", Document::new().with("newRoot", path.as_str())),
                    ]
                    .map(Value::Document),
                );
            }
            (_, other) => out.push(other.clone()),
        }
    }
    out
}

/// A document's first field (a stage's or an operator's).
fn first(d: &Document) -> (String, Value) {
    d.iter().next().map(|(k, v)| (k.to_owned(), v.clone())).unwrap_or((String::new(), Value::Null))
}

fn accumulator(v: &Value) -> Accumulator {
    let (op, arg) = match v {
        Value::Document(d) => first(d),
        other => (String::new(), other.clone()),
    };
    Accumulator { op, arg }
}

fn accumulators(d: &Document) -> Vec<(String, Accumulator)> {
    d.iter().map(|(k, v)| (k.to_owned(), accumulator(v))).collect()
}

fn output(d: &Document) -> Option<Vec<(String, Accumulator)>> {
    match d.get("output") {
        Some(Value::Document(o)) => Some(accumulators(o)),
        _ => None,
    }
}

fn string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn document(v: Option<&Value>) -> Option<Document> {
    match v {
        Some(Value::Document(d)) => Some(d.clone()),
        _ => None,
    }
}

fn fill_with(v: &Value) -> FillWith {
    match v {
        Value::Document(m) => match (m.get("value"), m.get("method")) {
            (Some(value), _) => FillWith::Value(value.clone()),
            (None, Some(Value::String(x))) if x == "linear" => FillWith::Linear,
            (None, _) => FillWith::Locf,
        },
        _ => FillWith::Locf,
    }
}

/// A window's specification `w`, read.
pub(super) fn window(w: &Document) -> Window {
    let output = match w.get("output") {
        Some(Value::Document(o)) => o
            .iter()
            .filter_map(|(k, f)| match f {
                Value::Document(d) => Some((k.to_owned(), d.clone())),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Window {
        partition_by: w.get("partitionBy").cloned(),
        sort_by: document(w.get("sortBy")),
        output,
        spec: w.clone(),
    }
}

fn kind(name: &str, arg: &Value) -> Kind {
    match (name, arg) {
        ("$match", Value::Document(m)) => Kind::Match(m.clone()),
        ("$sort", Value::Document(s)) => Kind::Sort(s.clone()),
        ("$skip", n) => Kind::Skip(whole(n)),
        ("$limit", n) => Kind::Limit(whole(n)),
        ("$project", Value::Document(p)) => Kind::Project(p.clone()),
        ("$addFields" | "$set", Value::Document(p)) => Kind::AddFields(p.clone()),
        ("$unset", Value::String(f)) => Kind::Unset(vec![f.clone()]),
        ("$unset", Value::Array(items)) => Kind::Unset(strings(items)),
        ("$replaceRoot", Value::Document(r)) => {
            Kind::ReplaceRoot(r.get("newRoot").cloned().unwrap_or(Value::Null))
        }
        ("$replaceWith", v) => Kind::ReplaceRoot(v.clone()),
        ("$group", Value::Document(g)) => Kind::Group(
            g.iter()
                .map(|(k, v)| match k {
                    "_id" => GroupEntry::Key(v.clone()),
                    _ => GroupEntry::Field(k.to_owned(), accumulator(v)),
                })
                .collect(),
        ),
        ("$count", Value::String(f)) => Kind::Count(f.clone()),
        ("$sortByCount", v) => Kind::SortByCount(v.clone()),
        ("$bucket", Value::Document(b)) => Kind::Bucket(Bucket {
            group_by: b.get("groupBy").cloned().unwrap_or(Value::Null),
            boundaries: match b.get("boundaries") {
                Some(Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            },
            default: b.get("default").cloned(),
            output: output(b),
        }),
        ("$bucketAuto", Value::Document(b)) => Kind::BucketAuto(BucketAuto {
            group_by: b.get("groupBy").cloned().unwrap_or(Value::Null),
            buckets: b.get("buckets").cloned().unwrap_or(Value::Null),
            granularity: b.get("granularity").cloned(),
            output: output(b),
        }),
        ("$facet", Value::Document(f)) => Kind::Facet(
            f.iter()
                .map(|(k, p)| {
                    let stages = match p {
                        Value::Array(items) => of_values(items),
                        _ => Vec::new(),
                    };
                    (k.to_owned(), stages)
                })
                .collect(),
        ),
        ("$lookup", Value::Document(l)) => Kind::Lookup(Lookup {
            from: string(l.get("from")),
            as_field: string(l.get("as")).unwrap_or_default(),
            local_field: l.get("localField").cloned(),
            foreign_field: l.get("foreignField").cloned(),
            let_vars: l.get("let").cloned(),
            pipeline: match l.get("pipeline") {
                Some(Value::Array(items)) => Some(of_values(&without_documents(items))),
                _ => None,
            },
        }),
        ("$graphLookup", Value::Document(g)) => Kind::GraphLookup(g.clone()),
        ("$unwind", Value::String(p)) => {
            Kind::Unwind(Unwind { path: p.clone(), preserve: None, index: None })
        }
        ("$unwind", Value::Document(u)) => Kind::Unwind(Unwind {
            path: string(u.get("path")).unwrap_or_default(),
            preserve: u.get("preserveNullAndEmptyArrays").cloned(),
            index: string(u.get("includeArrayIndex")),
        }),
        ("$unionWith", Value::String(c)) => {
            Kind::UnionWith(UnionWith { coll: Some(c.clone()), pipeline: Vec::new() })
        }
        ("$unionWith", Value::Document(d)) => Kind::UnionWith(UnionWith {
            coll: string(d.get("coll")),
            pipeline: match d.get("pipeline") {
                Some(Value::Array(p)) => p.clone(),
                _ => Vec::new(),
            },
        }),
        ("$setWindowFields", Value::Document(w)) => Kind::SetWindowFields(window(w)),
        ("$fill", Value::Document(f)) => Kind::Fill(Fill {
            partition_by: f.get("partitionBy").cloned(),
            sort_by: document(f.get("sortBy")),
            output: match f.get("output") {
                Some(Value::Document(o)) => {
                    o.iter().map(|(k, v)| (k.to_owned(), fill_with(v))).collect()
                }
                _ => Vec::new(),
            },
        }),
        ("$densify", Value::Document(d)) => Kind::Densify(Densify {
            field: string(d.get("field")).unwrap_or_default(),
            partition_by_fields: match d.get("partitionByFields") {
                Some(Value::Array(items)) => strings(items),
                _ => Vec::new(),
            },
            range: d.get("range").cloned().unwrap_or(Value::Null),
        }),
        ("$sample", Value::Document(d)) => Kind::Sample(d.get("size").map_or(0, whole)),
        ("$redact", v) => Kind::Redact(v.clone()),
        _ => Kind::Other,
    }
}

impl Step {
    /// Stage `doc`, read.
    pub(super) fn new(doc: Document) -> Step {
        let (name, arg) = first(&doc);
        let kind = kind(&name, &arg);
        Step { doc, kind }
    }

    /// The stage's name.
    pub(super) fn name(&self) -> &str {
        self.doc.iter().next().map_or("", |(k, _)| k)
    }

    /// Whether it is a projection a query plan takes (`$project`, `$addFields`, `$set`,
    /// `$unset`, `$replaceRoot`, `$replaceWith`, `$fill`).
    pub(super) fn is_projection(&self) -> bool {
        matches!(
            self.kind,
            Kind::Project(_)
                | Kind::AddFields(_)
                | Kind::Unset(_)
                | Kind::ReplaceRoot(_)
                | Kind::Fill(_)
        )
    }

    /// Whether it changes each document on its own, so that `$skip` and `$limit` move ahead of
    /// it.
    pub(super) fn is_one_to_one(&self) -> bool {
        match &self.kind {
            Kind::Project(_) | Kind::AddFields(_) | Kind::Unset(_) | Kind::ReplaceRoot(_) => true,
            Kind::Fill(f) => f.fills_values(),
            _ => false,
        }
    }
}

/// The documents of `steps`.
pub(super) fn docs(steps: &[Step]) -> Vec<Document> {
    steps.iter().map(|s| s.doc.clone()).collect()
}
