//! Reads on a view (015 FR-001): `find`, `count` and `distinct` on a view become the view's
//! pipeline followed by the stages of the read, as a `refrouter` 9.0.4 runs them (measured
//! 31-10-2026): the options an aggregation cannot take are refused, and the read cannot change
//! the view's collation.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_collation::{CollationSpec, ParsedCollation, parse_collation};
use teseladb_meta::catalog::Namespace;
use teseladb_plan::views::Resolved;

use super::{Backend, names};
use crate::error::{
    BAD_VALUE, CommandError, INDEX_NOT_FOUND, INVALID_PIPELINE_OPERATOR,
    OPTION_NOT_SUPPORTED_ON_VIEW,
};

/// The view `ns` resolved down to the collection it reads, `None` if `ns` is no view.
pub fn resolve<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Option<Resolved>, CommandError> {
    let mut failure: Option<CommandError> = None;
    let resolved = teseladb_plan::views::resolve(&ns.coll, |name| {
        let entry = backend.collection(&names::ns(&ns.db, name));
        let kind = entry.and_then(|c| c.map(|c| super::kinds::kind_of(&c)).transpose());
        kind.unwrap_or_else(|e| {
            failure.get_or_insert(e);
            None
        })
    });
    if let Some(e) = failure {
        return Err(e);
    }
    resolved
        .map_err(|e| CommandError::new(crate::error::code(e.code).unwrap_or(BAD_VALUE), e.message))
}

/// The complete form of a collation document, `None` for the simple one or none.
fn spec_of(collation: Option<&Document>) -> Result<Option<CollationSpec>, CommandError> {
    let Some(d) = collation else { return Ok(None) };
    let bytes = d.to_bytes()?;
    match parse_collation(RawDoc::from_bytes(&bytes)?) {
        Ok(ParsedCollation::Simple) => Ok(None),
        Ok(ParsedCollation::Collation(_, spec)) => Ok(Some(spec)),
        Err(e) => {
            Err(CommandError::new(crate::error::code(e.code).unwrap_or(BAD_VALUE), e.message))
        }
    }
}

/// Refuses a `collation` that is not the view's own (compared in their complete form, so
/// `{locale: "simple"}` is the same as none).
pub fn check_collation(view: &Resolved, given: Option<&Document>) -> Result<(), CommandError> {
    if given.is_none() {
        return Ok(());
    }
    let own = match &view.collation {
        Some(b) => Some(Document::from(RawDoc::from_bytes(b)?)),
        None => None,
    };
    if spec_of(given)? == spec_of(own.as_ref())? {
        return Ok(());
    }
    Err(CommandError::new(
        OPTION_NOT_SUPPORTED_ON_VIEW,
        "Cannot override a view's default collation",
    ))
}

/// Whether `filter` holds `$text`, at its top level or inside `$and`, `$or` or `$nor`.
fn has_text(filter: &Document) -> bool {
    filter.iter().any(|(k, v)| match (k, v) {
        ("$text", _) => true,
        ("$and" | "$or" | "$nor", Value::Array(items)) => {
            items.iter().any(|i| matches!(i, Value::Document(d) if has_text(d)))
        }
        _ => false,
    })
}

/// Whether option `name` of `cmd` asks for something: `true`, or a non-empty document.
fn asks(cmd: &Document, name: &str) -> bool {
    match cmd.get(name) {
        Some(Value::Boolean(b)) => *b,
        Some(Value::Document(d)) => !d.is_empty(),
        _ => false,
    }
}

/// The options of `find` an aggregation cannot take, in the order RefDb checks them.
const FIND_OPTIONS: [&str; 6] =
    ["showRecordId", "min", "max", "returnKey", "singleBatch", "noCursorTimeout"];

/// The stages a `find` on view `ns_text` becomes: `$match`, `$sort`, `$skip`, `$limit` and
/// `$project`, each when the command has it. The `$text` of its filter needs a text index,
/// which a view has not.
pub fn find_stages(cmd: &Document, ns_text: &str) -> Result<Vec<Document>, CommandError> {
    if asks(cmd, "tailable") {
        return Err(CommandError::new(
            INVALID_PIPELINE_OPERATOR,
            "Tailable cursors are not supported in aggregation.",
        ));
    }
    if let Some(option) = FIND_OPTIONS.iter().find(|o| asks(cmd, o)) {
        return Err(CommandError::new(
            INVALID_PIPELINE_OPERATOR,
            format!("Option {option} not supported in aggregation."),
        ));
    }
    let mut stages = Vec::new();
    if let Some(Value::Document(f)) = cmd.get("filter") {
        if has_text(f) {
            return Err(CommandError::new(
                INDEX_NOT_FOUND,
                format!("text index required for $text query (no such collection '{ns_text}')"),
            ));
        }
        if !f.is_empty() {
            stages.push(Document::new().with("$match", f.clone()));
        }
    }
    if let Some(Value::Document(s)) = cmd.get("sort")
        && !s.is_empty()
        && s.get("$natural").is_none()
    {
        stages.push(Document::new().with("$sort", s.clone()));
    }
    push_number(&mut stages, cmd, "skip", "$skip");
    push_number(&mut stages, cmd, "limit", "$limit");
    if let Some(Value::Document(p)) = cmd.get("projection")
        && !p.is_empty()
    {
        stages.push(Document::new().with("$project", p.clone()));
    }
    Ok(stages)
}

/// Adds `{stage: n}` for a positive number `field` of `cmd` (the command checked its type).
fn push_number(stages: &mut Vec<Document>, cmd: &Document, field: &str, stage: &str) {
    let n = match cmd.get(field) {
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        Some(Value::Double(x)) => *x as i64,
        _ => 0,
    };
    if n != 0 {
        stages.push(Document::new().with(stage, Value::Int64(n.abs())));
    }
}

/// The stages a `count` on a view becomes: `$match` of its query, `$skip`, `$limit`.
pub fn count_stages(cmd: &Document) -> Vec<Document> {
    let mut stages = Vec::new();
    if let Some(Value::Document(q)) = cmd.get("query")
        && !q.is_empty()
    {
        stages.push(Document::new().with("$match", q.clone()));
    }
    push_number(&mut stages, cmd, "skip", "$skip");
    push_number(&mut stages, cmd, "limit", "$limit");
    stages
}

/// The stage a `distinct` on a view runs before collecting its values: `$match` of its query.
pub fn distinct_stages(filter: &Document) -> Vec<Document> {
    if filter.is_empty() {
        return Vec::new();
    }
    vec![Document::new().with("$match", filter.clone())]
}
