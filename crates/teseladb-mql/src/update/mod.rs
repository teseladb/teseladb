//! Updates (FR-012, FR-013): operator documents, replacement documents and pipelines, applied to
//! a document on the leader of its range (D-009). Operators run in the reference server's path
//! order; paths that overlap are a conflict and `_id` cannot change.
//!
//! ```
//! use teseladb_bson::{Document, RawDoc};
//! use teseladb_mql::update::{Now, Update, UpdateContext};
//! use teseladb_mql::{MatchContext, NoExtensions};
//!
//! let u = Document::new().with("u", Document::new().with("$inc", Document::new().with("n", 1)));
//! let (u, d) = (u.to_bytes().unwrap(), Document::new().with("n", 1).to_bytes().unwrap());
//! let update = Update::parse(RawDoc::from_bytes(&u).unwrap().get("u").unwrap(), None).unwrap();
//! let ctx = UpdateContext {
//!     matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
//!     position: None,
//!     now: Now { millis: 0, timestamp: (0, 0) },
//!     insert: false,
//! };
//! let out = update.apply(RawDoc::from_bytes(&d).unwrap(), &ctx).unwrap();
//! assert_eq!(out.doc, Document::new().with("n", 2));
//! assert!(out.modified);
//! ```

mod array;
mod field;
mod positional;
mod tree;
mod upsert;

#[cfg(test)]
mod lazy_tests;

use teseladb_bson::{Document, RawArray, RawDoc, RawValue, Value};

use crate::error::{MqlError, Result, codes, failed_to_parse};
use crate::matcher::MatchContext;
use crate::ops;
use crate::value::{OwnedDoc, OwnedValue, bson_error};
use positional::{ArrayFilters, UpdatePath, Walk, is_prefix, path_order};
use tree::{MDoc, MVal};

pub use upsert::{check_filter as check_upsert_filter, upsert};

/// The current time of the leader, for `$currentDate` (the logic does not read the clock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Now {
    /// Milliseconds since 1970, for dates.
    pub millis: i64,
    /// Seconds and increment, for timestamps.
    pub timestamp: (u32, u32),
}

/// What applying an update needs.
#[derive(Clone, Copy)]
pub struct UpdateContext<'a> {
    /// Collation and extensions.
    pub matcher: MatchContext<'a>,
    /// The filter's match position, for `$` ([`crate::Filter::match_position`]).
    pub position: Option<usize>,
    /// The current time.
    pub now: Now,
    /// The update inserts a document (upsert): `$setOnInsert` applies.
    pub insert: bool,
}

/// The result of an update.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateOutcome {
    /// The new document.
    pub doc: Document,
    /// Whether it differs from the old one (`nModified`).
    pub modified: bool,
}

/// What an operator does to its path, for the description of the change (014 FR-009).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathChange {
    /// Gives it a value.
    Assign,
    /// Removes it.
    Unset,
    /// Moves it to this path.
    Rename(String),
    /// Adds elements at the end of the array.
    Append,
    /// Rewrites the array.
    Rewrite,
}

/// A parsed update.
#[derive(Debug)]
pub struct Update {
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Replacement(OwnedDoc),
    Operators { entries: Vec<Entry>, filters: ArrayFilters },
    Pipeline(OwnedValue),
}

/// One operator on one path.
#[derive(Debug)]
pub(crate) struct Entry {
    pub op: &'static str,
    pub path: UpdatePath,
    pub arg: OwnedValue,
    pub action: Action,
}

/// What an operator does, as parsed.
#[derive(Debug)]
pub(crate) enum Action {
    Set,
    SetOnInsert,
    Unset,
    Arith(crate::arith::Op),
    MinMax(std::cmp::Ordering),
    Rename(UpdatePath),
    CurrentDate { timestamp: bool },
    AddToSet(Vec<Value>),
    Pop { first: bool },
    Pull(array::PullCond),
    PullAll(Vec<OwnedValue>),
    Push(array::PushSpec),
    Bit(Vec<(field::BitOp, Value)>),
}

/// The operators the reference server accepts in an update document.
const OPERATORS: [&str; 15] = [
    ops::SET,
    ops::SET_ON_INSERT,
    ops::UNSET,
    ops::INC,
    ops::MUL,
    ops::MIN,
    ops::MAX,
    ops::RENAME,
    ops::CURRENT_DATE,
    ops::ADD_TO_SET,
    ops::POP,
    ops::PULL,
    ops::PULL_ALL,
    ops::PUSH,
    ops::BIT,
];

impl Update {
    /// Reads an update: a document of operators, a replacement document or a pipeline (an
    /// array), with its `arrayFilters`.
    pub fn parse(update: RawValue<'_>, array_filters: Option<RawArray<'_>>) -> Result<Update> {
        let doc = match update {
            RawValue::Array(_) => {
                return Ok(Update { kind: Kind::Pipeline(OwnedValue::from_raw(update)?) });
            }
            RawValue::Document(d) => d,
            _ => {
                return Err(failed_to_parse(
                    "Update argument must be either an object or an array",
                ));
            }
        };
        // Operators when the first field is one (measured: `{$set: …, b: 1}` has an unknown
        // modifier `b`, `{a: 1, $set: …}` is a replacement).
        let operators = doc.iter().next().is_some_and(|(k, _)| k.starts_with('$'));
        if !operators {
            return Ok(Update { kind: Kind::Replacement(OwnedDoc::new(&Document::from(doc))?) });
        }
        let filters = ArrayFilters::parse(array_filters)?;
        let mut entries = Vec::new();
        for (op, arg) in doc.iter() {
            let Some(op) = OPERATORS.iter().copied().find(|o| *o == op) else {
                return Err(failed_to_parse(format!(
                    "Unknown modifier: {op}. Expected a valid update modifier or pipeline-style update specified as an array"
                )));
            };
            let RawValue::Document(fields) = arg else {
                return Err(failed_to_parse(format!(
                    "Modifiers operate on fields but we found type {} instead. For example: {{$mod: {{<field>: ...}}}} not {{{op}: {}}}",
                    crate::types::name_of(arg.element_type()),
                    crate::fmt::shell(arg)
                )));
            };
            for (path, value) in fields.iter() {
                let path = UpdatePath::parse(path)?;
                let action = parse_action(op, &path, value)?;
                entries.push(Entry { op, path, arg: OwnedValue::from_raw(value)?, action });
            }
        }
        check_filters(&filters, &entries, doc)?;
        check_conflicts(&entries)?;
        entries.sort_by(|a, b| path_order(&a.path.text, &b.path.text));
        Ok(Update { kind: Kind::Operators { entries, filters } })
    }

    /// Whether applying it reads the current time (`$currentDate`): what it writes then depends
    /// on when it is applied.
    pub fn reads_now(&self) -> bool {
        match &self.kind {
            Kind::Operators { entries, .. } => {
                entries.iter().any(|e| matches!(e.action, Action::CurrentDate { .. }))
            }
            Kind::Replacement(_) | Kind::Pipeline(_) => false,
        }
    }

    /// Whether this is a pipeline-style update.
    pub fn is_pipeline(&self) -> bool {
        matches!(self.kind, Kind::Pipeline(_))
    }

    /// Whether this is a replacement document.
    pub fn is_replacement(&self) -> bool {
        matches!(self.kind, Kind::Replacement(_))
    }

    /// What each operator does to its path as written (`$` and array filters unresolved), in the
    /// order they apply; `insert` says whether `$setOnInsert` applies. `None` for a replacement or
    /// a pipeline, which change the whole document.
    pub fn path_changes(&self, insert: bool) -> Option<Vec<(PathChange, String)>> {
        let Kind::Operators { entries, .. } = &self.kind else { return None };
        let change = |e: &Entry| match &e.action {
            Action::SetOnInsert if !insert => None,
            Action::Set
            | Action::SetOnInsert
            | Action::Arith(_)
            | Action::MinMax(_)
            | Action::CurrentDate { .. }
            | Action::Bit(_) => Some(PathChange::Assign),
            Action::Unset => Some(PathChange::Unset),
            Action::Rename(to) => Some(PathChange::Rename(to.text.clone())),
            Action::AddToSet(_) => Some(PathChange::Append),
            Action::Push(spec) if spec.appends() => Some(PathChange::Append),
            Action::Push(_) | Action::Pop { .. } | Action::Pull(_) | Action::PullAll(_) => {
                Some(PathChange::Rewrite)
            }
        };
        Some(entries.iter().filter_map(|e| Some((change(e)?, e.path.text.clone()))).collect())
    }

    /// Applies the update to `doc`.
    pub fn apply(&self, doc: RawDoc<'_>, ctx: &UpdateContext<'_>) -> Result<UpdateOutcome> {
        let old_id = doc.get(ops::ID).map(Value::from);
        let new = match &self.kind {
            Kind::Replacement(r) => replace(r.get(), old_id.as_ref())?,
            Kind::Pipeline(p) => {
                return self.apply_pipeline(p.get(), doc, old_id.as_ref(), ctx);
            }
            // The stored document is read only where the operators walk (003 FR-020).
            Kind::Operators { entries, filters } => {
                let root = MVal::Doc(MDoc::from_raw_lazy(doc));
                operators(entries, filters, root, doc, old_id.as_ref(), ctx)?
            }
        };
        top_level_names(&new)?;
        // The stored limits are the server's to apply, with the reference server's words for each
        // write.
        let bytes = new.to_bytes_with(crate::value::VALUE_LIMITS).map_err(bson_error)?;
        Ok(UpdateOutcome { modified: bytes != doc.as_bytes(), doc: new })
    }

    /// A pipeline's document, as a reference router 9.0.4 stores it (measured): the stored `_id`
    /// first (another one is an error), top-level names with `$` allowed (`$setField` writes them),
    /// and a change whenever the pipeline's document differs from the stored one, before the
    /// `_id` is put back.
    fn apply_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        old_id: Option<&Value>,
        ctx: &UpdateContext<'_>,
    ) -> Result<UpdateOutcome> {
        let piped = ctx.matcher.ext.update_pipeline(pipeline, doc, ctx.matcher.vars)?;
        let piped_bytes = piped.to_bytes_with(crate::value::VALUE_LIMITS).map_err(bson_error)?;
        let piped = OwnedDoc::new(&piped)?;
        let new = replace(piped.get(), old_id)?;
        new.to_bytes_with(crate::value::VALUE_LIMITS).map_err(bson_error)?;
        Ok(UpdateOutcome { modified: piped_bytes != doc.as_bytes(), doc: new })
    }
}

/// The document `root` (the copy of `doc`) becomes after the operators `entries`.
fn operators(
    entries: &[Entry],
    filters: &ArrayFilters,
    mut root: MVal,
    doc: RawDoc<'_>,
    old_id: Option<&Value>,
    ctx: &UpdateContext<'_>,
) -> Result<Document> {
    for e in entries {
        if matches!(e.action, Action::SetOnInsert) && !ctx.insert {
            continue;
        }
        apply_entry(e, &mut root, filters, ctx, doc)?;
    }
    let new = match root {
        MVal::Doc(d) => d.to_document(),
        _ => Document::new(),
    };
    if old_id.is_some() && new.get(ops::ID) != old_id {
        return Err(MqlError::new(
            codes::IMMUTABLE_FIELD,
            "Performing an update on the path '_id' would modify the immutable field '_id'",
        ));
    }
    Ok(new)
}

/// The replacement, keeping the old `_id` first.
fn replace(r: RawDoc<'_>, old_id: Option<&Value>) -> Result<Document> {
    let mut out = Document::new();
    let new_id = r.get(ops::ID).map(Value::from);
    match (old_id, &new_id) {
        (Some(old), Some(new)) if old != new => {
            let shown = OwnedDoc::new(&Document::new().with(ops::ID, new.clone()))?;
            return Err(MqlError::new(
                codes::IMMUTABLE_FIELD,
                format!(
                    "After applying the update, the (immutable) field '_id' was found to have been altered to {}",
                    crate::fmt::shell_doc(shown.get())
                        .trim_start_matches('{')
                        .trim_end_matches('}')
                ),
            ));
        }
        (Some(old), _) => out.push(ops::ID, old.clone()),
        (None, Some(new)) => out.push(ops::ID, new.clone()),
        (None, None) => {}
    }
    for (k, v) in r.iter() {
        if k != ops::ID {
            out.push(k, Value::from(v));
        }
    }
    Ok(out)
}

/// The new document's top-level names: none starts with `$` (deeper ones may, as in 5.0 and
/// later); checked while the update runs, as a reference router 9.0.4 does (measured).
fn top_level_names(doc: &Document) -> Result<()> {
    match doc.iter().find(|(k, _)| k.starts_with('$')) {
        Some((k, _)) => Err(MqlError::new(
            codes::DOLLAR_PREFIXED_FIELD_NAME,
            format!(
                "The dollar ($) prefixed field '{k}' in '{k}' is not allowed in the context of an update's replacement document. Consider using an aggregation pipeline with $replaceWith."
            ),
        )),
        None => Ok(()),
    }
}

fn parse_action(op: &str, path: &UpdatePath, v: RawValue<'_>) -> Result<Action> {
    Ok(match op {
        ops::SET => Action::Set,
        ops::SET_ON_INSERT => Action::SetOnInsert,
        ops::UNSET => Action::Unset,
        ops::INC | ops::MUL => field::arith(op, &path.text, v)?,
        ops::MIN => Action::MinMax(std::cmp::Ordering::Less),
        ops::MAX => Action::MinMax(std::cmp::Ordering::Greater),
        ops::RENAME => field::rename(path, v)?,
        ops::CURRENT_DATE => field::current_date(v)?,
        ops::BIT => field::bit(v)?,
        _ => array::parse(op, &path.text, v)?,
    })
}

/// Every identifier used has a filter and every filter is used.
fn check_filters(filters: &ArrayFilters, entries: &[Entry], doc: RawDoc<'_>) -> Result<()> {
    for e in entries {
        for id in e.path.identifiers() {
            if !filters.filters.iter().any(|(i, _)| i == id) {
                return Err(crate::error::bad_value(format!(
                    "No array filter found for identifier '{id}' in path '{}'",
                    e.path.text
                )));
            }
        }
    }
    for (id, _) in &filters.filters {
        if !entries.iter().any(|e| e.path.identifiers().any(|i| i == id)) {
            return Err(failed_to_parse(format!(
                "The array filter for identifier '{id}' was not used in the update {}",
                crate::fmt::shell(RawValue::Document(doc))
            )));
        }
    }
    Ok(())
}

/// Where two paths may meet through a positional segment (`$`, `$[]`, `$[id]`), which can be any
/// element: the path before it (measured: `docs.$[].x` and `docs.0.x` meet at `docs`).
fn positional_overlap(a: &str, b: &str) -> Option<String> {
    let mut common = Vec::new();
    for (x, y) in a.split('.').zip(b.split('.')) {
        let dynamic = |s: &str| s == "$" || (s.starts_with("$[") && s.ends_with(']'));
        if dynamic(x) || dynamic(y) {
            return (x != y).then(|| common.join("."));
        }
        if x != y {
            return None;
        }
        common.push(x);
    }
    None
}

/// Paths that are equal or one inside the other.
fn check_conflicts(entries: &[Entry]) -> Result<()> {
    let mut paths: Vec<&str> = Vec::new();
    for e in entries {
        paths.push(&e.path.text);
        if let Action::Rename(to) = &e.action {
            paths.push(&to.text);
        }
    }
    for (i, p) in paths.iter().enumerate() {
        for q in &paths[..i] {
            let (long, short) = if p.len() >= q.len() { (*p, *q) } else { (*q, *p) };
            if let Some(at) = positional_overlap(p, q) {
                return Err(MqlError::new(
                    codes::CONFLICTING_UPDATE_OPERATORS,
                    format!("Updating the path '{p}' would create a conflict at '{at}'"),
                ));
            }
            if is_prefix(short, long) {
                return Err(MqlError::new(
                    codes::CONFLICTING_UPDATE_OPERATORS,
                    format!("Updating the path '{long}' would create a conflict at '{short}'"),
                ));
            }
        }
    }
    Ok(())
}

fn apply_entry(
    e: &Entry,
    root: &mut MVal,
    filters: &ArrayFilters,
    ctx: &UpdateContext<'_>,
    original: RawDoc<'_>,
) -> Result<()> {
    if let Action::Rename(to) = &e.action {
        return field::apply_rename(root, &e.path, to);
    }
    let create = matches!(
        e.action,
        Action::Set
            | Action::SetOnInsert
            | Action::Arith(_)
            | Action::MinMax(_)
            | Action::CurrentDate { .. }
            | Action::AddToSet(_)
            | Action::Push(_)
            | Action::Bit(_)
    );
    let walk =
        Walk { filters, ctx: &ctx.matcher, position: ctx.position, create, text: &e.path.text };
    let mut leaf = |slot: positional::Slot<'_>| -> Result<()> {
        match &e.action {
            Action::Set | Action::SetOnInsert => {
                let mut slot = slot;
                slot.set(MVal::from_raw(e.arg.get()))
            }
            Action::Unset => {
                let mut slot = slot;
                slot.unset();
                Ok(())
            }
            Action::Pop { .. }
            | Action::Pull(_)
            | Action::PullAll(_)
            | Action::AddToSet(_)
            | Action::Push(_) => array::apply(e, slot, ctx, original),
            _ => field::apply(e, slot, ctx, original),
        }
    };
    walk.run(root, "", &e.path.segs, &mut leaf)
}
