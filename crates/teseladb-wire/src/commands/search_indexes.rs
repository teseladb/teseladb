//! The search index commands (011 FR-008): `createSearchIndexes`, `listSearchIndexes`,
//! `updateSearchIndex` and `dropSearchIndex`, with the answers and errors of a 9.0.4 replica set
//! with `refsearch` 1.70.6 (measured 01-11-2026). The indexes live in Group 0's catalog under the
//! collection; an index asked for on a view is kept under the collection the view reads, as
//! RefDb does. The definition's own rules are the search engine's; here only what decides the
//! command's answer is checked.

use teseladb_bson::{Document, Value};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, SearchIndexEntry, SearchIndexKind, SearchIndexStatus};

use super::{Backend, Ctx, args, names, views};
use crate::error::{
    COMMAND_FAILED, CommandError, INDEX_ALREADY_EXISTS, INDEX_NOT_FOUND, INVALID_NAMESPACE,
    INVALID_OPTIONS, NAMESPACE_NOT_FOUND, UNKNOWN_ERROR,
};

/// `createSearchIndexes`.
pub const CREATE_SEARCH_INDEXES: &str = "createSearchIndexes";
/// `listSearchIndexes`.
pub const LIST_SEARCH_INDEXES: &str = "listSearchIndexes";
/// `updateSearchIndex`.
pub const UPDATE_SEARCH_INDEX: &str = "updateSearchIndex";
/// `dropSearchIndex`.
pub const DROP_SEARCH_INDEX: &str = "dropSearchIndex";

/// The name of an index created without one.
const DEFAULT_NAME: &str = "default";
/// The type names of the definitions.
const SEARCH: &str = "search";
const VECTOR_SEARCH: &str = "vectorSearch";

const INDEXES: &str = "indexes";
const NAME: &str = "name";
const ID: &str = "id";
const TYPE: &str = "type";
const DEFINITION: &str = "definition";
const MAPPINGS: &str = "mappings";
const DYNAMIC: &str = "dynamic";
const FIELDS: &str = "fields";
/// How `refsearch` names the command in its errors.
const USER_COMMAND: &str = "userCommand";
/// The error when both `name` and `id` are given.
const BOTH: &str = "Cannot set both 'name' and 'id'.";

/// The collection holding the search indexes of the namespace a command names, and that
/// namespace (a view's name stays what the client gave).
struct Target {
    ns: Namespace,
    coll: CollectionEntry,
}

/// The collection the command's first field names, through its views.
fn target<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Target, CommandError> {
    let named = match ctx.first() {
        Value::String(s) => names::ns(ctx.db, s),
        v => {
            let msg = format!("Collection name has invalid type {}", crate::types::type_name(v));
            return Err(CommandError::new(INVALID_NAMESPACE, msg));
        }
    };
    names::check(&named)?;
    let ns = match views::resolve(ctx.backend, &named)? {
        Some(view) => names::ns(&named.db, &view.collection),
        None => named.clone(),
    };
    match ctx.backend.collection(&ns)? {
        Some(coll) => Ok(Target { ns, coll }),
        None => Err(CommandError::new(
            NAMESPACE_NOT_FOUND,
            format!(
                "Error retrieving collection UUID and view info :: caused by :: Collection '{}.{}' does not exist.",
                named.db, named.coll
            ),
        )),
    }
}

/// Refuses a field of the command other than `allowed` and the ones every command may carry.
fn only(cmd: &Document, name: &str, allowed: &[&str]) -> Result<(), CommandError> {
    for (k, _) in cmd.iter().skip(1) {
        if !allowed.contains(&k) && !crate::generic::GENERIC_ARGUMENTS.contains(&k) {
            return Err(unknown_field(name, k));
        }
    }
    Ok(())
}

fn unknown_field(name: &str, k: &str) -> CommandError {
    CommandError::new(
        crate::error::IDL_UNKNOWN_FIELD,
        format!("BSON field '{name}.{k}' is an unknown field."),
    )
}

/// The index an `id` or a `name` picks, with RefDb's errors when both or neither are given.
pub enum Pick {
    /// By name.
    Name(String),
    /// By identifier.
    Id([u8; 12]),
}

/// `id` and `name` of `fields` of command `cmd`; `id_path` names the `id` in its format error,
/// `both` is the error when both are given.
fn pick(
    fields: &Document,
    cmd: &str,
    id_path: &str,
    both: &str,
) -> Result<Option<Pick>, CommandError> {
    let name = args::string(fields, cmd, NAME)?;
    let id = match args::string(fields, cmd, ID)? {
        None => None,
        Some(s) => Some(parse_id(s).ok_or_else(|| bad_id(id_path))?),
    };
    match (name, id) {
        (Some(_), Some(_)) => Err(CommandError::new(INVALID_OPTIONS, both)),
        (Some(n), None) => Ok(Some(Pick::Name(n.to_owned()))),
        (None, Some(i)) => Ok(Some(Pick::Id(i))),
        (None, None) => Ok(None),
    }
}

fn bad_id(field: &str) -> CommandError {
    CommandError::new(UNKNOWN_ERROR, format!("\"{field}.id\" is invalid ObjectId format"))
}

/// The 12 bytes of an `ObjectId` written as 24 hexadecimal digits.
fn parse_id(text: &str) -> Option<[u8; 12]> {
    let digits = text.as_bytes();
    if digits.len() != 24 {
        return None;
    }
    let mut id = [0; 12];
    for (i, b) in id.iter_mut().enumerate() {
        let hex = std::str::from_utf8(&digits[2 * i..2 * i + 2]).ok()?;
        *b = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(id)
}

/// The 24 hexadecimal digits of an `ObjectId`.
fn hex_id(id: &[u8; 12]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn matches(entry: &SearchIndexEntry, which: &Pick) -> bool {
    match which {
        Pick::Name(n) => &entry.name == n,
        Pick::Id(i) => &entry.id == i,
    }
}

/// RefDb's error for an index `which` names that is not there.
fn not_found(which: &Pick, ns: &Namespace) -> CommandError {
    let (what, value) = match which {
        Pick::Name(n) => ("name", n.clone()),
        Pick::Id(i) => ("id", hex_id(i)),
    };
    CommandError::new(
        INDEX_NOT_FOUND,
        format!("No index with {what} {value} exists in namespace {}.{}", ns.db, ns.coll),
    )
}

/// The first problem of a definition of type `kind`: the path of the field inside the
/// definition and what is wrong with it, as `refsearch` words them.
fn check_definition(kind: SearchIndexKind, def: &Document) -> Option<(String, &'static str)> {
    match kind {
        SearchIndexKind::Search => match def.get(MAPPINGS) {
            None => Some((MAPPINGS.to_owned(), "is required")),
            Some(Value::Document(m)) => match m.get(DYNAMIC) {
                None | Some(Value::Boolean(_) | Value::Document(_)) => None,
                Some(_) => Some((format!("{MAPPINGS}.{DYNAMIC}"), "must be a boolean or document")),
            },
            Some(_) => Some((MAPPINGS.to_owned(), "must be a document")),
        },
        SearchIndexKind::VectorSearch => match def.get(FIELDS) {
            None => Some((FIELDS.to_owned(), "is required")),
            Some(Value::Array(_)) => None,
            Some(_) => Some((FIELDS.to_owned(), "must be an array")),
        },
    }
}

/// One index of `createSearchIndexes`, checked.
struct Wanted {
    name: String,
    kind: SearchIndexKind,
    definition: Document,
}

/// Reads index `i` of `createSearchIndexes`.
fn wanted(i: usize, v: &Value) -> Result<Wanted, CommandError> {
    let path = format!("{CREATE_SEARCH_INDEXES}.{INDEXES}");
    let Value::Document(spec) = v else {
        return Err(crate::types::wrong_type(&path, v, "object"));
    };
    for (k, _) in spec.iter() {
        if ![NAME, TYPE, DEFINITION].contains(&k) {
            return Err(unknown_field(&path, k));
        }
    }
    let name = args::string(spec, &path, NAME)?.unwrap_or(DEFAULT_NAME).to_owned();
    let definition =
        args::doc(spec, &path, DEFINITION)?.ok_or_else(|| args::missing(&path, DEFINITION))?;
    let kind = match spec.get(TYPE) {
        None => SearchIndexKind::Search,
        Some(Value::String(t)) if t == SEARCH => SearchIndexKind::Search,
        Some(Value::String(t)) if t == VECTOR_SEARCH => SearchIndexKind::VectorSearch,
        Some(_) => {
            let msg = format!(
                "\"userCommand.{INDEXES}[{i}].{TYPE}\" must be one of [{SEARCH}, {VECTOR_SEARCH}]"
            );
            return Err(CommandError::new(UNKNOWN_ERROR, msg));
        }
    };
    if let Some((at, what)) = check_definition(kind, definition) {
        let msg = format!("\"userCommand.{INDEXES}[{i}].{at}\" {what}");
        return Err(CommandError::new(UNKNOWN_ERROR, msg));
    }
    Ok(Wanted { name, kind, definition: definition.clone() })
}

/// `createSearchIndexes`: every index is checked before any is created; one with the name and
/// definition of an existing index gives that index back.
pub fn create<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let t = target(ctx)?;
    only(ctx.cmd, CREATE_SEARCH_INDEXES, &[INDEXES])?;
    let given = args::array(ctx.cmd, CREATE_SEARCH_INDEXES, INDEXES)?
        .ok_or_else(|| args::missing(CREATE_SEARCH_INDEXES, INDEXES))?;
    let all: Vec<Wanted> =
        given.iter().enumerate().map(|(i, v)| wanted(i, v)).collect::<Result<_, _>>()?;
    let existing = ctx.backend.search_indexes(t.coll.id)?;
    let mut created = Vec::new();
    let mut new = Vec::new();
    for w in all {
        let bytes = w.definition.to_bytes()?;
        match existing.iter().find(|e| e.name == w.name) {
            Some(e) if e.definition == bytes => created.push((e.id, w.name)),
            Some(_) => {
                let msg = format!(
                    "Index {} already exists with a different definition. Drop it first if needed.",
                    w.name
                );
                return Err(CommandError::new(INDEX_ALREADY_EXISTS, msg));
            }
            None => new.push((w, bytes)),
        }
    }
    for (w, definition) in new {
        let id = super::new_object_id(ctx.backend, ctx.shared);
        let at_ms = i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX);
        let cmd = IndexCommand::CreateSearch {
            id,
            at_ms,
            collection: t.coll.id,
            name: w.name.clone(),
            kind: w.kind,
            definition,
        };
        ctx.backend.index_write(cmd)?;
        created.push((id, w.name));
    }
    let list: Vec<Value> = created
        .into_iter()
        .map(|(id, name)| Document::new().with(ID, hex_id(&id)).with(NAME, name).into())
        .collect();
    Ok(Document::new().with("indexesCreated", list).with("ok", 1.0))
}

/// The status name `$listSearchIndexes` shows.
fn status_name(s: SearchIndexStatus) -> &'static str {
    match s {
        SearchIndexStatus::Pending => "PENDING",
        SearchIndexStatus::Building => "BUILDING",
        SearchIndexStatus::Ready => "READY",
        SearchIndexStatus::Failed => "FAILED",
        SearchIndexStatus::Stale => "STALE",
        SearchIndexStatus::Deleting => "DELETING",
    }
}

/// Whether queries can use the index: when ready, and while a new definition is built over one
/// that was (measured: `BUILDING` after an update is queryable).
fn queryable(e: &SearchIndexEntry) -> bool {
    match e.status {
        SearchIndexStatus::Ready | SearchIndexStatus::Stale => true,
        SearchIndexStatus::Building => e.version > 0,
        SearchIndexStatus::Pending | SearchIndexStatus::Failed | SearchIndexStatus::Deleting => {
            false
        }
    }
}

/// The document of an index that `listSearchIndexes` and `$listSearchIndexes` give.
pub fn described(e: &SearchIndexEntry) -> Result<Document, CommandError> {
    // `createdAt` keeps whole seconds (measured).
    let created = e.defined_ms - e.defined_ms.rem_euclid(1000);
    let version = Document::new()
        .with("version", i32::try_from(e.version).unwrap_or(i32::MAX))
        .with("createdAt", Value::DateTime(created));
    Ok(Document::new()
        .with(ID, hex_id(&e.id))
        .with(NAME, e.name.clone())
        .with("status", status_name(e.status))
        .with("queryable", queryable(e))
        .with("latestDefinitionVersion", version)
        .with("latestDefinition", Document::from(teseladb_bson::RawDoc::from_bytes(&e.definition)?))
        .with("statusDetail", Vec::<Value>::new()))
}

/// The indexes of `coll` that `id` or `name` of `filter` pick, described.
pub fn listed<B: Backend + ?Sized>(
    backend: &mut B,
    coll: u32,
    which: Option<&Pick>,
) -> Result<Vec<Value>, CommandError> {
    let mut all = backend.search_indexes(coll)?;
    // In the order they were created (measured), which is their identifiers' order.
    all.sort_by_key(|e| e.id);
    all.iter()
        .filter(|e| which.is_none_or(|w| matches(e, w)))
        .map(|e| described(e).map(Value::from))
        .collect()
}

/// `listSearchIndexes`: the indexes in one batch.
pub fn list<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let t = target(ctx)?;
    only(ctx.cmd, LIST_SEARCH_INDEXES, &[ID, NAME])?;
    let which = pick(ctx.cmd, LIST_SEARCH_INDEXES, USER_COMMAND, BOTH)?;
    let batch = listed(ctx.backend, t.coll.id, which.as_ref())?;
    let cursor = Document::new()
        .with("id", Value::Int64(0))
        .with("ns", format!("{}.{}", t.ns.db, t.ns.coll))
        .with("firstBatch", batch);
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// The index of `t` that `which` picks.
fn find<B: Backend + ?Sized>(
    backend: &mut B,
    t: &Target,
    which: &Pick,
) -> Result<SearchIndexEntry, CommandError> {
    let all = backend.search_indexes(t.coll.id)?;
    all.into_iter().find(|e| matches(e, which)).ok_or_else(|| not_found(which, &t.ns))
}

/// `updateSearchIndex`: a new version of the definition, built while the old one serves.
pub fn update<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let t = target(ctx)?;
    only(ctx.cmd, UPDATE_SEARCH_INDEX, &[ID, NAME, DEFINITION])?;
    let definition = args::doc(ctx.cmd, UPDATE_SEARCH_INDEX, DEFINITION)?
        .ok_or_else(|| args::missing(UPDATE_SEARCH_INDEX, DEFINITION))?;
    let which = pick(ctx.cmd, UPDATE_SEARCH_INDEX, USER_COMMAND, BOTH)?
        .ok_or_else(|| CommandError::new(INVALID_OPTIONS, "Must set either 'name' or 'id'."))?;
    let e = find(ctx.backend, &t, &which)?;
    if let Some((at, what)) = check_definition(e.kind, definition) {
        return Err(CommandError::new(COMMAND_FAILED, format!("\"{at}\" {what}")));
    }
    let at_ms = i64::try_from(ctx.backend.now_ms()).unwrap_or(i64::MAX);
    ctx.backend.index_write(IndexCommand::UpdateSearch {
        at_ms,
        collection: t.coll.id,
        name: e.name,
        definition: definition.to_bytes()?,
    })?;
    Ok(Document::new().with("ok", 1.0))
}

/// `dropSearchIndex`.
pub fn drop<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let t = target(ctx)?;
    only(ctx.cmd, DROP_SEARCH_INDEX, &[ID, NAME])?;
    let which = pick(ctx.cmd, DROP_SEARCH_INDEX, USER_COMMAND, BOTH)?.ok_or_else(|| {
        CommandError::new(UNKNOWN_ERROR, "\"userCommand\" one of [id, name] must be present")
    })?;
    let e = find(ctx.backend, &t, &which)?;
    ctx.backend.index_write(IndexCommand::DropSearch { collection: t.coll.id, name: e.name })?;
    Ok(Document::new().with("ok", 1.0))
}

/// The documents of the `$listSearchIndexes` stage on `ns` (its specification already checked
/// by the pipeline): none when the collection does not exist (measured).
pub fn stage<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    spec: &Document,
) -> Result<Vec<Document>, CommandError> {
    let stage = teseladb_exec::pipeline::LIST_SEARCH_INDEXES;
    let which = pick(spec, stage, &format!("{USER_COMMAND}.{stage}"), BOTH)?;
    let coll_ns = match views::resolve(backend, ns)? {
        Some(view) => names::ns(&ns.db, &view.collection),
        None => ns.clone(),
    };
    let Some(coll) = backend.collection(&coll_ns)? else { return Ok(Vec::new()) };
    listed(backend, coll.id, which.as_ref())?
        .into_iter()
        .map(|v| match v {
            Value::Document(d) => Ok(d),
            _ => Err(CommandError::new(crate::error::INTERNAL_ERROR, "an index is a document")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_read_and_write_as_24_hex_digits() {
        let id = parse_id("6ae78b5a64236d09bf4d2903").expect("id");
        assert_eq!(hex_id(&id), "6ae78b5a64236d09bf4d2903");
        assert_eq!(parse_id("abc"), None);
        assert_eq!(parse_id("6ae78b5a64236d09bf4d29zz"), None);
    }

    #[test]
    fn definitions_are_checked_as_refsearch_words_it() {
        let d = |v: Value| Document::new().with(MAPPINGS, v);
        assert_eq!(check_definition(SearchIndexKind::Search, &d(Document::new().into())), None);
        assert_eq!(
            check_definition(SearchIndexKind::Search, &Document::new()),
            Some((MAPPINGS.to_owned(), "is required"))
        );
        let bad = Document::new().with(DYNAMIC, "x");
        assert_eq!(
            check_definition(SearchIndexKind::Search, &d(bad.into())),
            Some(("mappings.dynamic".to_owned(), "must be a boolean or document"))
        );
        assert_eq!(
            check_definition(SearchIndexKind::VectorSearch, &d(Document::new().into())),
            Some((FIELDS.to_owned(), "is required"))
        );
    }
}
