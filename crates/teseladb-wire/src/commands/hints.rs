//! The `hint` of a read (010 FR-001): an index name or key pattern that no index of the
//! collection has fails as RefDb's planner fails (measured on a `refrouter` 9.0.4), with the
//! query's tree, its sort and its projection in the message.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::IndexState as EntryState;

use super::{Backend, names};
use crate::error::{BAD_VALUE, CommandError, FAILED_TO_PARSE};

/// The name of the index of `_id`.
const ID_INDEX: &str = "_id_";

/// What a hinted read asks.
pub(crate) struct Hinted<'a> {
    /// The read's filter.
    pub filter: &'a Document,
    /// Its sort, if any.
    pub sort: Option<&'a Document>,
    /// Its projection, if any.
    pub projection: Option<&'a Document>,
    /// Its `let` variables.
    pub vars: Option<&'a Document>,
}

/// Refuses a `hint` that is neither a name nor a key pattern, as a `refrouter` parses it.
pub(crate) fn check_hint_type(cmd: &Document) -> Result<(), CommandError> {
    match cmd.get("hint") {
        None | Some(Value::String(_) | Value::Document(_)) => Ok(()),
        Some(_) => Err(CommandError::new(FAILED_TO_PARSE, "Hint must be a string or an object")),
    }
}

/// Refuses `hint` of command `cmd` on `coll` when no index of the collection answers to it.
pub(crate) fn check_hint<B: Backend + ?Sized>(
    backend: &mut B,
    (ns, coll): (&Namespace, &CollectionEntry),
    cmd: &Document,
    read: &Hinted<'_>,
) -> Result<(), CommandError> {
    let found = match cmd.get("hint") {
        Some(Value::String(name)) => {
            name == ID_INDEX || indexes(backend, coll)?.iter().any(|(n, _)| n == name)
        }
        Some(Value::Document(k)) => {
            k.is_empty()
                || k.get("$natural").is_some()
                || *k == Document::new().with("_id", 1)
                || indexes(backend, coll)?.iter().any(|(_, p)| p == k)
        }
        _ => true,
    };
    if found {
        return Ok(());
    }
    let parsed = super::explain::parsed(read.filter, read.vars)?;
    let shown = |d: Option<&Document>| -> Result<String, CommandError> {
        let bytes = d.cloned().unwrap_or_default().to_bytes()?;
        Ok(teseladb_mql::shell(teseladb_bson::RawValue::Document(RawDoc::from_bytes(&bytes)?)))
    };
    Err(CommandError::new(
        BAD_VALUE,
        format!(
            "error enumerating plans for query: ns={}Tree: {}Sort: {}\nProj: {}\n planner returned \
             error :: caused by :: hint provided does not correspond to an existing index",
            names::full(ns),
            teseladb_mql::debug_tree(&parsed),
            shown(read.sort)?,
            shown(read.projection)?,
        ),
    ))
}

/// The names and key patterns of the ready indexes of `coll`.
fn indexes<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
) -> Result<Vec<(String, Document)>, CommandError> {
    let mut out = Vec::new();
    for entry in backend.indexes(coll.id)? {
        if entry.state != EntryState::Ready {
            continue;
        }
        let spec = Document::from(RawDoc::from_bytes(&entry.spec)?);
        let key = match spec.get("key") {
            Some(Value::Document(k)) => k.clone(),
            _ => Document::new(),
        };
        out.push((entry.name.clone(), key));
    }
    Ok(out)
}

/// The position among `indexes` of the index `hint` names, if the planner may scan it; `None`
/// without a hint and for `$natural`.
pub(crate) fn position(
    hint: Option<&Value>,
    indexes: &[super::plan_exec::PlanIndex],
) -> Result<Option<usize>, CommandError> {
    Ok(match hint {
        Some(Value::String(name)) => indexes.iter().position(|i| i.entry.name == *name),
        Some(Value::Document(k)) if !k.is_empty() && k.get("$natural").is_none() => {
            indexes.iter().position(|i| i.pattern == *k)
        }
        _ => None,
    })
}
