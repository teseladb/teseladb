//! The part of the first `$match` after `$changeStream` that goes down to the reads of the ranges
//! (014 FR-012): the namespaces and the event types it allows, which depend on no field the
//! coordinator computes, decide which collections' ranges a read asks. The `$match` still runs
//! whole on each event, so what it cannot decide here reads everything.

use std::collections::BTreeSet;

use teseladb_bson::{Document, Value};

/// The event types that come from documents' versions; the others come from the catalog.
const DOCUMENT_EVENTS: [&str; 4] = ["insert", "update", "replace", "delete"];

/// What a stream's first `$match` lets the reads leave out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pushdown {
    /// The databases it allows (`None`: any).
    dbs: Option<BTreeSet<String>>,
    /// The collections it allows (`None`: any).
    colls: Option<BTreeSet<String>>,
    /// The event types it allows (`None`: any).
    kinds: Option<BTreeSet<String>>,
}

/// The strings a condition on one field allows: a string, `{$eq: s}` or `{$in: [s, …]}`.
fn allowed(v: &Value) -> Option<BTreeSet<String>> {
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    };
    match v {
        Value::String(s) => Some(BTreeSet::from([s.clone()])),
        Value::Document(d) if d.len() == 1 => match d.iter().next() {
            Some(("$eq", v)) => text(v).map(|s| BTreeSet::from([s])),
            Some(("$in", Value::Array(a))) => a.iter().map(text).collect(),
            _ => None,
        },
        _ => None,
    }
}

/// Narrows `slot` to `set`.
fn narrow(slot: &mut Option<BTreeSet<String>>, set: BTreeSet<String>) {
    *slot = Some(match slot.take() {
        Some(old) => old.intersection(&set).cloned().collect(),
        None => set,
    });
}

impl Pushdown {
    /// What the first of `stages` (the stages after `$changeStream`) pushes down.
    pub fn of(stages: &[Document]) -> Pushdown {
        let mut p = Pushdown::default();
        if let Some(Value::Document(filter)) = stages.first().and_then(|s| s.get("$match")) {
            p.add(filter);
        }
        p
    }

    /// Adds the conditions of `filter` that the reads can decide.
    fn add(&mut self, filter: &Document) {
        for (k, v) in filter.iter() {
            match (k, v) {
                ("ns.db", v) => allowed(v).into_iter().for_each(|s| narrow(&mut self.dbs, s)),
                ("ns.coll", v) => allowed(v).into_iter().for_each(|s| narrow(&mut self.colls, s)),
                ("operationType", v) => {
                    allowed(v).into_iter().for_each(|s| narrow(&mut self.kinds, s));
                }
                ("ns", Value::Document(ns)) => {
                    if let (Some(Value::String(db)), Some(Value::String(coll)), 2) =
                        (ns.get("db"), ns.get("coll"), ns.len())
                    {
                        narrow(&mut self.dbs, BTreeSet::from([db.clone()]));
                        narrow(&mut self.colls, BTreeSet::from([coll.clone()]));
                    }
                }
                ("$and", Value::Array(all)) => {
                    for c in all {
                        if let Value::Document(c) = c {
                            self.add(c);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether a read asks the ranges of collection `coll` of database `db`.
    pub fn reads(&self, db: &str, coll: &str) -> bool {
        self.dbs.as_ref().is_none_or(|s| s.contains(db))
            && self.colls.as_ref().is_none_or(|s| s.contains(coll))
    }

    /// Whether a read asks the ranges of documents at all.
    pub fn reads_documents(&self) -> bool {
        self.kinds.as_ref().is_none_or(|k| DOCUMENT_EVENTS.iter().any(|e| k.contains(*e)))
    }
}
