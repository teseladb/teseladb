//! The fields a leading `$sort` holds (010 FR-010, FR-028): RefDb 9.0.4 runs the `$match`es
//! and the `$sort` a pipeline starts with in its query plan, and under the `SORT` it puts the
//! projection of the fields the rest of the pipeline uses when that projection keeps the sort's
//! fields (its dependency analysis, measured in the shape of `explain`), or the user's `$project`
//! that only keeps or drops fields and keeps the sort's, right after a sort without `$limit`
//! in a classic plan.
//! The sort then holds those fields only, and its memory counts them, so a pipeline over a
//! collection projects the documents the same way before its `$sort` takes them. The output does
//! not change: the stages after it read no other field.

use teseladb_mql::MqlError;
use teseladb_bson::{Document, RawValue, Value};

use super::shape::Projection;
use super::{Pipeline, Stage, deps};
use crate::expr::Ctx;

/// Whether a `$project` value keeps its field.
fn keeps(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Int32(n) => *n != 0,
        Value::Int64(n) => *n != 0,
        Value::Double(x) => *x != 0.0,
        _ => false,
    }
}

/// Whether a `$project` value only keeps or drops its field.
fn flag(v: &Value) -> bool {
    matches!(v, Value::Boolean(_) | Value::Int32(_) | Value::Int64(_) | Value::Double(_))
}

/// Whether path `field` is `path` or holds it.
fn holds(field: &str, path: &str) -> bool {
    path == field || path.strip_prefix(field).is_some_and(|rest| rest.starts_with('.'))
}

/// Whether path `a` is `b`, holds it or lies under it.
fn touches(a: &str, b: &str) -> bool {
    holds(a, b) || holds(b, a)
}

/// Whether `$project` `spec`, of fields kept or dropped only, leaves the sort's `paths` as they
/// are.
fn keeps_sort(spec: &Document, paths: &[&str]) -> bool {
    if !spec.iter().all(|(_, v)| flag(v)) {
        return false;
    }
    // `{_id: 1}` alone keeps `_id` only.
    let only_id = spec.iter().all(|(k, _)| k == "_id");
    let inclusion = spec.iter().any(|(k, v)| (k != "_id" || only_id) && keeps(v));
    match inclusion {
        true => paths.iter().all(|p| spec.iter().any(|(f, v)| keeps(v) && holds(f, p))),
        false => !spec.iter().any(|(f, v)| !keeps(v) && paths.iter().any(|p| touches(f, p))),
    }
}

/// The projection of fields `fields`, `_id` dropped unless used.
fn of_fields(fields: &[String]) -> Document {
    let mut d = Document::new();
    for f in fields {
        d.push(f.clone(), Value::Int32(1));
    }
    if !fields.iter().any(|f| f == "_id") {
        d.push("_id", Value::Int32(0));
    }
    d
}

fn parsed(spec: &Document) -> Result<Projection, MqlError> {
    crate::raw::with_raw_doc(spec, |raw| {
        Projection::parse(RawValue::Document(raw), &Ctx::checked(std::iter::empty()))
    })
}

impl Pipeline {
    /// Sets the projection the documents go through before the `$sort` that follows the
    /// `$match`es the pipeline starts with, as RefDb's query plan puts it under its `SORT`;
    /// `stages` are its stages as the command gave them, the pipeline reads a collection.
    pub fn narrow_sort_input(&mut self, stages: &[Document]) {
        self.narrow = self.sort_input(stages).map(Box::new);
    }

    /// Whether RefDb's query plan is its slot-based engine's: a `$group` or `$lookup` it runs
    /// with every stage before it.
    fn engine_plan(&self) -> bool {
        self.stages[..self.sbe_prefix()]
            .iter()
            .any(|s| matches!(s, Stage::Group(_) | Stage::Lookup(_)))
    }

    fn sort_input(&self, stages: &[Document]) -> Option<(usize, Projection)> {
        if stages.len() != self.stages.len() || self.reads_sort_key || self.reads_index_key {
            return None;
        }
        let n = self.leading_matches();
        let Some(Stage::Sort(sort)) = self.stages.get(n) else { return None };
        let paths = sort.paths()?;
        // The `$limit`s and `$skip`s right after the sort run in the plan too.
        let after = n
            + 1
            + self.stages[n + 1..]
                .iter()
                .take_while(|s| matches!(s, Stage::Limit(_) | Stage::Skip(_)))
                .count();
        let limited = after > n + 1;
        if self.stages[n + 1..after].iter().any(|s| matches!(s, Stage::Skip(_))) {
            return None;
        }
        let user: Option<Document> = stages.get(after).and_then(|s| match s.iter().next() {
            Some(("$project", Value::Document(p))) => Some(p.clone()),
            Some(("$unset", Value::String(f))) => Some(Document::new().with(f.clone(), 0)),
            Some(("$unset", Value::Array(fs))) => {
                let mut d = Document::new();
                for f in fs {
                    if let Value::String(f) = f {
                        d.push(f.clone(), Value::Int32(0));
                    }
                }
                Some(d)
            }
            _ => None,
        });
        let spec = match user {
            Some(p) if !limited && !self.engine_plan() && keeps_sort(&p, &paths) => p,
            // A projection that keeps or computes fields stands for those the rest uses, above
            // the sort.
            Some(p) if p.iter().any(|(k, v)| k != "_id" && !(flag(v) && !keeps(v))) => {
                return None;
            }
            _ => {
                let fields = deps::needed(&stages[n + 1..]).filter(|f| !f.is_empty())?;
                if !paths.iter().all(|p| fields.iter().any(|f| f == p)) {
                    return None;
                }
                of_fields(&fields)
            }
        };
        parsed(&spec).ok().map(|p| (n, p))
    }
}
