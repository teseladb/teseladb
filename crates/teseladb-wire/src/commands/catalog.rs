//! `create`, `drop`, `renameCollection`, `collMod`, `listDatabases`, `listCollections` and
//! `dropDatabase` over the catalog in Group 0 (FR-006, FR-027), with the answers of a `refrouter`
//! 7.0 (measured): `drop` of a missing collection and a repeated `create` with the same options
//! succeed. Dropping a collection deletes its documents before Group 0 forgets its identifier.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_mql::validation::{ValidationAction, ValidationLevel};
use teseladb_mql::{Expr, Filter, FilterUse, MatchContext, Pred};
use teseladb_range::read::ReadConcern;

use super::args::{self, missing};
use super::authz_collections::Generated;
use super::crud::Writer;
use super::mutation::{Bound, DocMutation, Image, Rules, from_mql};
use super::names::{self, full};
use super::query::{QueryCursor, documents_of, for_each_match, keep, target};
use super::{Backend, Ctx, index_spec, kinds, uuid, uuid_text};
use crate::error::{
    BAD_VALUE, COLLECTION_UUID_MISMATCH, CommandError, DATABASE_DIFFER_CASE, ILLEGAL_OPERATION,
    INVALID_NAMESPACE, NAMESPACE_EXISTS, NAMESPACE_NOT_FOUND,
};
use crate::types::wrong_type;
use teseladb_collation::ParsedCollation;

/// `create`.
pub const CREATE: &str = "create";
/// `drop`.
pub const DROP: &str = "drop";
/// `renameCollection`.
pub const RENAME_COLLECTION: &str = "renameCollection";
/// `renameCollection`'s `dropTarget`: a boolean, or the UUID the target must have.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DropTarget {
    No,
    Yes,
    Uuid([u8; 16]),
}

fn drop_target(cmd: &Document) -> Result<DropTarget, CommandError> {
    match cmd.get("dropTarget") {
        None | Some(Value::Boolean(false)) => Ok(DropTarget::No),
        Some(Value::Boolean(true)) => Ok(DropTarget::Yes),
        Some(Value::Binary { subtype, bytes }) => match <[u8; 16]>::try_from(bytes.as_slice()) {
            Ok(u) if *subtype == uuid::UUID_SUBTYPE => Ok(DropTarget::Uuid(u)),
            _ => Err(uuid::invalid_uuid()),
        },
        Some(v) => Err(crate::types::wrong_types(
            &format!("{RENAME_COLLECTION}.dropTarget"),
            v,
            &["bool", "binData"],
        )),
    }
}

/// `collMod`.
pub const COLL_MOD: &str = "collMod";
/// The option of `collMod` that changes an index.
const COLL_MOD_INDEX: &str = "index";
/// The option of `collMod` that only checks a conversion to `unique` (009 FR-007).
const DRY_RUN: &str = "dryRun";
/// `listDatabases`.
pub const LIST_DATABASES: &str = "listDatabases";
/// `listCollections`.
pub const LIST_COLLECTIONS: &str = "listCollections";
/// `dropDatabase`.
pub const DROP_DATABASE: &str = "dropDatabase";

/// The options `collMod` changes.
const MODIFIABLE: [&str; 6] = [
    "validator",
    "prepareConstraintValidationLevel",
    "validationLevel",
    "validationAction",
    "changeStreamPreAndPostImages",
    "expireAfterSeconds",
];

fn options_from(cmd: &Document) -> Document {
    let mut o = Document::new();
    for (k, v) in cmd.iter().skip(1).filter(|(k, _)| {
        !crate::generic::is_generic(k) && !k.starts_with('$') && *k != uuid::COLLECTION_UUID
    }) {
        o.push(k, v.clone());
    }
    o
}

pub(crate) fn options_of(c: &CollectionEntry) -> Result<Document, CommandError> {
    let mut options = stored_options(c)?;
    options.remove(ID_INDEX_VERSION);
    Ok(options)
}

/// The options as the catalog keeps them, with the version of the `_id` index when it is not
/// the default one.
fn stored_options(c: &CollectionEntry) -> Result<Document, CommandError> {
    Ok(Document::from(RawDoc::from_bytes(&c.options)?))
}

/// The version of the `_id` index that `create` was given in `idIndex.v` (what `refrestore`
/// sends for a collection dumped with a version 1 index), kept with the options when it is not
/// the default one. It is not a collection option: `listCollections` shows it in `idIndex`.
const ID_INDEX_VERSION: &str = "$_idIndexVersion";

/// Checks the validator options of `create` and `collMod` (`cmd`), with the messages of a
/// `refrouter` 9.0.4 (measured).
fn check_validation(o: &Document, cmd: &str) -> Result<(), CommandError> {
    let enumeration = |value: &str, field: &str| {
        let msg =
            format!("Enumeration value '{value}' for field '{cmd}.{field}' is not a valid value.");
        CommandError::new(BAD_VALUE, msg)
    };
    for field in ["validationLevel", "validationAction"] {
        let name = match o.get(field) {
            Some(Value::String(s)) => s.as_str(),
            Some(v) => return Err(wrong_type(&format!("{cmd}.{field}"), v, "string")),
            None => continue,
        };
        let known = match field {
            "validationLevel" => teseladb_mql::validation::ValidationLevel::parse(name).is_ok(),
            _ => teseladb_mql::validation::ValidationAction::parse(name).is_ok(),
        };
        if !known {
            return Err(enumeration(name, field));
        }
    }
    match o.get("validator") {
        Some(Value::Document(v)) => {
            let b = v.to_bytes()?;
            let raw = RawDoc::from_bytes(&b)?;
            Filter::parse(raw, FilterUse::Validator).map_err(from_mql)?;
            Ok(())
        }
        Some(v) => Err(wrong_type(&format!("{cmd}.validator"), v, "object")),
        None => Ok(()),
    }
}

/// The `collation` option of a collection.
const COLLATION: &str = "collation";

/// The `_id` index of collection `c`, with the collection's collation (`listCollections`'
/// `idIndex`, `listIndexes`).
pub(crate) fn id_index(c: &CollectionEntry) -> Result<Document, CommandError> {
    let version = match stored_options(c)?.get(ID_INDEX_VERSION) {
        Some(Value::Int32(v)) => *v,
        _ => index_spec::DEFAULT_VERSION,
    };
    let mut index = Document::new()
        .with("v", version)
        .with("key", Document::new().with("_id", 1))
        .with("name", "_id_");
    if let Some(collation) = options_of(c)?.get(COLLATION) {
        index.push(COLLATION, collation.clone());
    }
    Ok(index)
}

/// `create`.
pub fn create<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    names::check_writable(&ns)?;
    let mut options = options_from(ctx.cmd);
    let asked = kinds::check(&ns, &options)?;
    let given_collation = match options.get(COLLATION) {
        Some(Value::Document(c)) => Some(c.clone()),
        _ => None,
    };
    check_validation(&options, CREATE)?;
    check_constraint_action(&options)?;
    let mut shown = options.clone();
    if let Some(c) = &options.get(COLLATION).and_then(|v| match v {
        Value::Document(d) => Some(d.clone()),
        _ => None,
    }) {
        let parsed = parse_collation_doc(c)?;
        // The collection keeps the complete collation, and none for `simple` (measured).
        match parsed {
            ParsedCollation::Simple => {
                options.remove(COLLATION);
                shown.remove(COLLATION);
            }
            ParsedCollation::Collation(_, spec) => {
                options.insert(COLLATION, spec.to_document());
                shown.insert(COLLATION, requested_collation(&spec.to_document(), c));
            }
        }
    }
    let mut version = index_spec::DEFAULT_VERSION;
    if let Some(index) = options.remove(ID_INDEX) {
        shown.remove(ID_INDEX);
        let Value::Document(spec) = &index else {
            return Err(wrong_type(&format!("{CREATE}.{ID_INDEX}"), &index, "object"));
        };
        version = index_spec::check_id_index(spec, options.get(COLLATION), |c| {
            Ok(match parse_collation_doc(c)? {
                ParsedCollation::Simple => None,
                ParsedCollation::Collation(_, s) => Some(Value::Document(s.to_document())),
            })
        })?;
    }
    let asked_collation = match (options.get(COLLATION), &given_collation) {
        (Some(Value::Document(c)), Some(given)) => Some(requested_collation(c, given)),
        _ => None,
    };
    let options = kinds::stored(&asked, &options);
    if asked != kinds::Asked::Normal {
        shown = options.clone();
    }
    if let Some(existing) = ctx.backend.collection(&ns)? {
        let have = options_of(&existing)?;
        if let Some(answer) = kinds::existing(&ns, &have, &options, asked_collation.as_ref()) {
            return answer;
        }
        return same_options(&ns, &existing, &options, &shown);
    }
    let is_view = matches!(asked, kinds::Asked::View { .. });
    if is_view {
        kinds::check_view_definition(ctx.backend, &ns, &options)?;
    }
    let mut stored = options.clone();
    if version != index_spec::DEFAULT_VERSION {
        stored.push(ID_INDEX_VERSION, version);
    }
    let bytes = stored.to_bytes()?;
    let uuid = super::new_uuid(ctx.backend);
    let cmd = CatalogCommand::Create { ns: ns.clone(), uuid, options: bytes, if_absent: true };
    let created = write_adding_database(ctx.backend, &ns.db, cmd)?;
    if is_view {
        kinds::ensure_system_views(ctx.backend, &ns.db)?;
    }
    match created {
        // Another `create` won the race with other options.
        Some(c) if c.uuid != uuid => same_options(&ns, &c, &options, &shown),
        Some(c) => {
            // A time series with a meta field starts with its index (measured on 9.0.4).
            if let Some(index) =
                super::timeseries::spec_of(&c)?.as_ref().and_then(super::timeseries::default_index)
            {
                super::indexes::create_internal(ctx, &ns, &c, &index)?;
            }
            Ok(Document::new().with("ok", 1.0))
        }
        None => Ok(Document::new().with("ok", 1.0)),
    }
}

/// The options of schema validation.
const VALIDATOR: &str = "validator";
const VALIDATION_LEVEL: &str = "validationLevel";
const VALIDATION_ACTION: &str = "validationAction";
const PREPARE_CONSTRAINT: &str = "prepareConstraintValidationLevel";
const VALIDATION_FIELDS: [&str; 4] =
    [VALIDATOR, VALIDATION_LEVEL, VALIDATION_ACTION, PREPARE_CONSTRAINT];

/// The words before a `collMod` error of its validation options.
const SET_VALIDATION: &str = "Failed to set validation options";

/// `constraint` refuses the action `warn` (measured on a `refrouter` 9.0.4).
fn check_constraint_action(options: &Document) -> Result<(), CommandError> {
    let constraint = options.get(VALIDATION_LEVEL) == Some(&Value::from(CONSTRAINT));
    if constraint
        && options.get(VALIDATION_ACTION) == Some(&Value::from(ValidationAction::Warn.name()))
    {
        return Err(CommandError::new(
            BAD_VALUE,
            "Validation action of 'warn' is not allowed when Validation level is 'constraint'",
        ));
    }
    Ok(())
}

/// The validation level that keeps every document of the collection valid (9.0).
const CONSTRAINT: &str = "constraint";

/// A `collMod` to `validationLevel: "constraint"` as a `refrouter` 9.0.4 checks it (measured):
/// first every document against the validator, then that the level was `strict`.
fn check_constraint<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
    options: &Document,
    changes: &Document,
) -> Result<(), CommandError> {
    let validator = changes.get(VALIDATOR).or_else(|| options.get(VALIDATOR));
    if let Some(Value::Document(v)) =
        validator.filter(|v| !matches!(v, Value::Document(d) if d.is_empty()))
    {
        let nor = Document::new().with("$nor", vec![Value::Document(v.clone())]);
        let mut first = None;
        for_each_match(backend, documents_of(c), &nor, None, ReadConcern::Local, |_, d| {
            first = RawDoc::from_bytes(d).ok().and_then(|d| d.get("_id")).map(Value::from);
            Ok(false)
        })?;
        if let Some(id) = first {
            let msg = format!(
                "Cannot upgrade validationLevel to 'constraint': the collection contains documents that do not satisfy the validator. First offending document _id: {} Run db.{}.find({{\"$nor\": [{}]}}) to find non-compliant documents.",
                super::display(&id),
                ns.coll,
                super::display(&Value::Document(v.clone()))
            );
            return Err(CommandError::new(crate::error::CONSTRAINT_UPGRADE, msg));
        }
    }
    let strict =
        options.get(VALIDATION_LEVEL) == Some(&Value::from(ValidationLevel::Strict.name()));
    if !strict {
        return Err(CommandError::new(
            BAD_VALUE,
            "Failed to set validation options :: caused by :: validationLevel can only be changed to 'constraint' from 'strict'",
        ));
    }
    let prepared = options.get(PREPARE_CONSTRAINT).and_then(crate::types::truthy) == Some(true);
    if !prepared {
        return Err(CommandError::new(
            BAD_VALUE,
            "Failed to set validation options :: caused by :: Cannot upgrade validationLevel to 'constraint' without first setting prepareConstraintValidationLevel to true",
        ));
    }
    Ok(())
}

/// `idIndex` of `create`.
const ID_INDEX: &str = "idIndex";
/// A collation document read as RefDb reads it.
fn parse_collation_doc(c: &Document) -> Result<ParsedCollation, CommandError> {
    let b = c.to_bytes()?;
    let raw = RawDoc::from_bytes(&b)?;
    teseladb_collation::parse_collation(raw)
        .map_err(|e| CommandError::new(crate::error::code(e.code).unwrap_or(BAD_VALUE), e.message))
}

/// A collation as the error of a repeated `create` shows the request: complete, without its
/// `version`, and with `backwards` only if the client gave it (measured on a `refrouter` 9.0.4).
fn requested_collation(full: &Document, given: &Document) -> Document {
    let mut shown = full.clone();
    shown.remove("version");
    shown.remove("backwards");
    // The collation parser has already refused a `backwards` that is not a boolean.
    if let Some(Value::Boolean(b)) = given.get("backwards") {
        shown.push("backwards", *b);
    }
    shown
}

fn same_options(
    ns: &Namespace,
    c: &CollectionEntry,
    asked: &Document,
    asked_shown: &Document,
) -> Result<Document, CommandError> {
    let have = options_of(c)?;
    if &have == asked {
        return Ok(Document::new().with("ok", 1.0));
    }
    let shown = kinds::shown_existing(&have, asked, &uuid_text(&c.uuid));
    let mut asked_text = String::from("{");
    for (i, (k, v)) in asked_shown.iter().enumerate() {
        let sep = if i == 0 { " " } else { ", " };
        asked_text.push_str(&format!("{sep}{k}: {}", super::display(v)));
    }
    // An empty request is `{}` (measured on a `refrouter` 9.0.4).
    asked_text.push_str(if asked_shown.is_empty() { "}" } else { " }" });
    let msg = format!(
        "namespace '{}' already exists, but with different options: {shown}, requested: {asked_text}",
        full(ns)
    );
    Err(CommandError::new(NAMESPACE_EXISTS, msg))
}

/// Deletes the documents of dropped collection `c`, then tells Group 0 (`Purged`).
pub fn purge<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
) -> Result<(), CommandError> {
    super::indexes::drop_all(backend, ns, c)?;
    let (start, end) = documents_of(c);
    let mut w = Writer::internal();
    let rules = Rules { ns: full(ns), unlogged: true, ..Rules::default() };
    loop {
        let rows =
            backend.scan(&start, &end, super::crud::MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
        if rows.is_empty() {
            break;
        }
        let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
        let mutation = DocMutation::Delete {
            filter: Document::new(),
            multi: true,
            image: Image::None,
            rules: rules.clone(),
        };
        w.write(backend, &Bound { mutation, keys }, 0)?;
    }
    super::capped::purge(backend, ns, c)?;
    backend.catalog_write(CatalogCommand::Purged { id: c.id })?;
    Ok(())
}

/// `drop`.
pub fn drop<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    names::check_droppable(&ns)?;
    if let Some(u) = uuid::expected(ctx.cmd, DROP)? {
        uuid::check(ctx.backend, &ns, u)?;
    }
    let Some(c) = ctx.backend.collection(&ns)? else {
        // Measured on a `refrouter` 9.0.4: the database has no entry either.
        if !has_database(ctx.backend, &ns.db)? {
            return Ok(Document::new().with("info", "database does not exist").with("ok", 1.0));
        }
        return Ok(Document::new().with("ok", 1.0));
    };
    super::write_block::ddl(ctx.backend, &ns.db)?;
    ctx.backend.catalog_write(CatalogCommand::Drop { ns: ns.clone() })?;
    purge(ctx.backend, &ns, &c)?;
    // `system.views` holds the views of its database: they go with it (measured on 9.0.4).
    if ns.coll == kinds::SYSTEM_VIEWS {
        for view in kinds::views_of(ctx.backend, &ns.db)? {
            ctx.backend.catalog_write(CatalogCommand::Drop { ns: view })?;
        }
    }
    Ok(Document::new().with("ok", 1.0))
}

/// `renameCollection` (on `admin`).
pub fn rename<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let from_text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type("renameCollection.renameCollection", v, "string")),
    };
    if ctx.db != super::admin::ADMIN_DB {
        return Err(CommandError::new(
            crate::error::UNAUTHORIZED,
            "renameCollection may only be run against the admin database.",
        ));
    }
    let to_text = args::string(ctx.cmd, RENAME_COLLECTION, "to")?
        .ok_or_else(|| missing(RENAME_COLLECTION, "to"))?
        .to_owned();
    let drop_target = drop_target(ctx.cmd)?;
    let invalid = |t: &str| {
        CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified '{t}'"))
    };
    let from = names::parse_full(&from_text).ok_or_else(|| invalid(&from_text))?;
    let to = names::parse_full(&to_text).ok_or_else(|| invalid(&to_text))?;
    names::check(&from)?;
    names::check(&to)?;
    if from.db == CONFIG_DB || to.db == CONFIG_DB {
        let msg = "Can't rename a collection in the config database";
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    names::check_writable(&to)?;
    if from == to {
        return Err(CommandError::new(ILLEGAL_OPERATION, "Can't rename a collection to itself"));
    }
    if let Some(u) = uuid::expected(ctx.cmd, RENAME_COLLECTION)? {
        uuid::check(ctx.backend, &from, u)?;
    }
    let Some(source) = ctx.backend.collection(&from)? else {
        let msg = format!("Collection {from_text} doesn't exist.");
        return Err(missing_namespace(ctx.backend, &from, msg)?);
    };
    let target_coll = ctx.backend.collection(&to)?;
    if let DropTarget::Uuid(u) = drop_target
        && target_coll.as_ref().is_none_or(|t| t.uuid != u)
    {
        return Err(CommandError::new(
            COLLECTION_UUID_MISMATCH,
            "Collection UUID does not match that specified",
        ));
    }
    let drop_target = drop_target != DropTarget::No;
    if target_coll.is_some() && !drop_target {
        return Err(CommandError::new(
            NAMESPACE_EXISTS,
            format!("Target collection {to_text} exists but dropTarget is not set"),
        ));
    }
    super::write_block::ddl(ctx.backend, &from.db)?;
    let uuid = super::new_uuid(ctx.backend);
    let rename = CatalogCommand::Rename { from, to: to.clone(), drop_target, uuid };
    write_adding_database(ctx.backend, &to.db, rename)?;
    if let Some(t) = target_coll.filter(|t| t.id != source.id) {
        purge(ctx.backend, &to, &t)?;
    }
    Ok(Document::new().with("ok", 1.0))
}

/// `collMod`.
pub fn coll_mod<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    let expected_uuid = uuid::expected(ctx.cmd, COLL_MOD)?;
    let mut changes = options_from(ctx.cmd);
    // `index` changes one index (009 FR-007); the other options change the collection.
    let index_change = changes.remove(COLL_MOD_INDEX);
    let dry_run =
        super::index_mod::dry_run(changes.remove(DRY_RUN).as_ref(), index_change.is_some())?;
    for (k, _) in changes.iter() {
        if !MODIFIABLE.contains(&k) {
            return Err(CommandError::new(
                crate::error::IDL_UNKNOWN_FIELD,
                format!("BSON field 'collMod.{k}' is an unknown field."),
            ));
        }
    }
    check_validation(&changes, COLL_MOD)?;
    if let Some(u) = expected_uuid {
        uuid::check(ctx.backend, &ns, u)?;
    }
    let Some(c) = ctx.backend.collection(&ns)? else {
        let msg = format!("namespace does not exist: {}", full(&ns));
        return Err(missing_namespace(ctx.backend, &ns, msg)?);
    };
    let index_reply = match &index_change {
        Some(change) => {
            let how = super::index_mod::Modify { dry_run, collection_changes: !changes.is_empty() };
            super::index_mod::modify_index(ctx.backend, &ns, &c, change, how)?
        }
        None => Document::new(),
    };
    if index_change.is_some() && changes.is_empty() {
        return Ok(index_reply.with("ok", 1.0));
    }
    let mut options = options_of(&c)?;
    let constraint_now = options.get(VALIDATION_LEVEL) == Some(&Value::from(CONSTRAINT));
    if constraint_now && changes.get(VALIDATOR).is_some() {
        return Err(CommandError::new(
            BAD_VALUE,
            "Failed to set validation options :: caused by :: Validator cannot be changed when Validation level is 'constraint'",
        ));
    }
    let upgrade = changes.get(VALIDATION_LEVEL) == Some(&Value::from(CONSTRAINT));
    if upgrade && !constraint_now {
        check_constraint(ctx.backend, &ns, &c, &options, &changes)?;
    }
    for (k, v) in changes.iter() {
        options.insert(k, v.clone());
    }
    check_constraint_action(&options).map_err(|e| {
        CommandError::new(BAD_VALUE, format!("{SET_VALIDATION} :: caused by :: {}", e.errmsg))
    })?;
    // A change of validation writes both its level and its action, and drops an empty
    // validator (measured on a `refrouter` 9.0.4).
    if VALIDATION_FIELDS.iter().any(|f| changes.get(f).is_some()) {
        if matches!(options.get(VALIDATOR), Some(Value::Document(v)) if v.is_empty()) {
            options.remove(VALIDATOR);
        }
        for (field, default) in [
            (VALIDATION_LEVEL, ValidationLevel::Strict.name()),
            (VALIDATION_ACTION, ValidationAction::Error.name()),
        ] {
            let value = options.remove(field).unwrap_or_else(|| Value::from(default));
            options.push(field, value);
        }
    }
    // The preparation of `constraint` is kept while true and ends with the upgrade.
    let prepared = options.get(PREPARE_CONSTRAINT).and_then(crate::types::truthy) == Some(true);
    options.remove(PREPARE_CONSTRAINT);
    if prepared && !upgrade {
        options.push(PREPARE_CONSTRAINT, true);
    }
    if let Some(version) = stored_options(&c)?.get(ID_INDEX_VERSION) {
        options.push(ID_INDEX_VERSION, version.clone());
    }
    let bytes = options.to_bytes()?;
    super::write_block::ddl(ctx.backend, &ns.db)?;
    ctx.backend.catalog_write(CatalogCommand::Modify { ns, options: bytes })?;
    Ok(index_reply.with("ok", 1.0))
}

/// Whether `doc` matches the command's `filter`, if it has one.
pub(crate) fn passes(filter: Option<&Document>, doc: &Document) -> Result<bool, CommandError> {
    let Some(f) = filter else { return Ok(true) };
    let fb = f.to_bytes_with(crate::msg::command_limits())?;
    let db = doc.to_bytes()?;
    let raw = RawDoc::from_bytes_with(&fb, crate::msg::command_limits())?;
    let filter = Filter::parse(raw, FilterUse::Query).map_err(from_mql)?;
    let ctx = MatchContext { collation: None, ext: &super::engines::NodeExtensions, vars: None };
    filter.matches(RawDoc::from_bytes(&db)?, &ctx).map_err(from_mql)
}

/// Bytes of a storage block: a collection's size is counted in whole blocks.
pub const BLOCK_BYTES: u64 = 4096;

/// Databases a `refrouter` always lists.
const ALWAYS_LISTED: [&str; 2] = [super::admin::ADMIN_DB, CONFIG_DB];

/// The `sizeOnDisk` of a collection storing `stored` bytes of keys and documents: whole blocks,
/// at least one, and one block for its `_id` index (an empty collection takes 8192 bytes in
/// RefEngine, measured on 9.0.4).
pub fn collection_size(stored: u64) -> u64 {
    stored.div_ceil(BLOCK_BYTES).max(1) * BLOCK_BYTES + BLOCK_BYTES
}

/// Whether the catalog has database `db`, with or without collections (D-077).
pub(crate) fn has_database<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<bool, CommandError> {
    Ok(backend.databases()?.iter().any(|d| d == db))
}

/// The error of a command on a collection that does not exist: `msg`, or, when its database is
/// not in the catalog either, the one a `refrouter` 9.0.4 gives (measured).
pub(crate) fn missing_namespace<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    msg: String,
) -> Result<CommandError, CommandError> {
    let msg =
        if has_database(backend, &ns.db)? { msg } else { format!("database {} not found", ns.db) };
    Ok(CommandError::new(NAMESPACE_NOT_FOUND, msg))
}

/// Runs `cmd`, which may add database `db` to the catalog. When Group 0 refuses it because
/// another database differs from `db` only in case, the error names both, as RefDb's 13297;
/// if that database is gone by then, Group 0's refusal goes as it is.
pub(crate) fn write_adding_database<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    cmd: CatalogCommand,
) -> Result<Option<CollectionEntry>, CommandError> {
    super::write_block::ddl(backend, db)?;
    let refused = match backend.catalog_write(cmd) {
        Err(e) if e.code == DATABASE_DIFFER_CASE => e,
        other => return other,
    };
    let have = backend.databases()?.into_iter().find(|d| d != db && d.eq_ignore_ascii_case(db));
    Err(have.map_or(refused, |have| {
        let msg = format!(
            "Database {db} could not be created :: caused by :: can't have 2 databases that \
             just differ on case  have: {have} want to add: {db}"
        );
        CommandError::new(DATABASE_DIFFER_CASE, msg)
    }))
}

/// Records database `db` before a write to it runs, as `refrouter` creates its entry in
/// `config.databases` for any write, also one that writes nothing (D-077, measured on 9.0.4).
pub(crate) fn ensure_database<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<(), CommandError> {
    if has_database(backend, db)? {
        return Ok(());
    }
    write_adding_database(backend, db, CatalogCommand::CreateDatabase { db: db.to_owned() })
        .map(|_| ())
}

/// Whether a `refrouter` lists a database without collections under `filter` (measured on 9.0.4):
/// with no filter, or with one that parses to a single predicate on `name` that is neither a
/// negation (`$ne`, `$nin`, `$not`) nor a conjunction (`$and`, two operators). The predicate still
/// has to match the entry.
fn lists_empty(filter: Option<&Document>) -> Result<bool, CommandError> {
    let Some(f) = filter else { return Ok(true) };
    let bytes = f.to_bytes_with(crate::msg::command_limits())?;
    let raw = RawDoc::from_bytes_with(&bytes, crate::msg::command_limits())?;
    let parsed = Filter::parse(raw, FilterUse::Query).map_err(from_mql)?;
    Ok(matches!(
        parsed.expr(),
        Expr::Field { path, pred } if path == "name" && !matches!(pred, Pred::And(_) | Pred::Not(_))
    ))
}

/// `listDatabases` (on `admin`): `admin` and `config` always, and each database of the catalog
/// with the size of its collections in blocks of stored bytes, not of compressed files; with
/// `nameOnly` the sizes are not read. A database without collections (D-077) shows
/// `sizeOnDisk: 0` and `empty: true`, as `refrouter` shows a database of `config.databases` that no
/// shard reports.
pub fn list_databases<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let name_only = args::safe_bool(
        ctx.cmd,
        LIST_DATABASES,
        "nameOnly",
        &crate::types::LIST_DATABASES_SAFE_BOOL_TYPES,
    )?
    .unwrap_or(false);
    let filter = args::doc(ctx.cmd, LIST_DATABASES, "filter")?.cloned();
    // After its fields are read (measured).
    if ctx.db != super::admin::ADMIN_DB {
        return Err(CommandError::new(
            crate::error::UNAUTHORIZED,
            "listDatabases may only be run against the admin database.",
        ));
    }
    let mut sizes: BTreeMap<String, Option<u64>> =
        ALWAYS_LISTED.iter().map(|db| ((*db).to_owned(), Some(0))).collect();
    if lists_empty(filter.as_ref())? {
        for db in ctx.backend.databases()? {
            sizes.entry(db).or_insert(None);
        }
    }
    for (ns, c) in ctx.backend.collections()? {
        let size = if name_only {
            0
        } else {
            let (start, end) = documents_of(&c);
            collection_size(ctx.backend.stored_bytes(&start, &end)?)
        };
        let sum = sizes.entry(ns.db).or_insert(None).get_or_insert(0);
        *sum += size;
    }
    let only = super::authorize::database_filter(ctx.backend, ctx.cmd);
    sizes.retain(|db, _| super::authorize::database_shown(only.as_deref(), db));
    // With sizes `refrouter` lists in name order; with `nameOnly`, `admin` and `config` and then
    // `config.databases` in the order its entries were added (measured on 9.0.4).
    let mut order: Vec<String> = Vec::new();
    if name_only {
        let added = ctx.backend.databases()?;
        for db in ALWAYS_LISTED.iter().map(|d| (*d).to_owned()).chain(added) {
            if sizes.contains_key(&db) && !order.contains(&db) {
                order.push(db);
            }
        }
    } else {
        order.extend(sizes.keys().cloned());
    }
    let mut out = Vec::new();
    let mut total = 0u64;
    for db in order {
        let size = sizes.get(&db).copied().flatten();
        let on_config = ALWAYS_LISTED.contains(&db.as_str());
        let entry = if name_only {
            Document::new().with("name", db)
        } else if let Some(size) = size {
            // `admin` and `config` hold internal collections even when the catalog has none.
            let size = size.max(collection_size(0));
            Document::new()
                .with("name", db)
                .with("sizeOnDisk", i64::try_from(size).unwrap_or(i64::MAX))
                .with("empty", false)
        } else {
            Document::new().with("name", db).with("sizeOnDisk", 0i64).with("empty", true)
        };
        if passes(filter.as_ref(), &entry)? {
            // `totalSize` counts only what the filter lets through (measured).
            let mut entry = entry;
            if let (false, Some(size)) = (name_only, size) {
                let size = size.max(collection_size(0));
                total += size;
                // Where its data is, after the filter (measured): `admin` and `config` on the
                // config server, the others on the node (D-102).
                let shard = match on_config {
                    true => super::admin::CONFIG_SHARD.to_owned(),
                    false => super::shard_name(ctx.backend.node()),
                };
                let at = i64::try_from(size).unwrap_or(i64::MAX);
                entry.push("shards", Document::new().with(shard.as_str(), at));
            }
            out.push(Value::Document(entry));
        }
    }
    let mut reply = Document::new().with("databases", out);
    if !name_only {
        reply.push("totalSize", i64::try_from(total).unwrap_or(i64::MAX));
        reply.push("totalSizeMb", i64::try_from(total >> 20).unwrap_or(i64::MAX));
    }
    reply.push("ok", 1.0);
    Ok(reply)
}

/// `listCollections`.
pub fn list_collections<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name_only = args::bool_field(ctx.cmd, LIST_COLLECTIONS, "nameOnly")?.unwrap_or(false);
    let filter = args::doc(ctx.cmd, LIST_COLLECTIONS, "filter")?.cloned();
    let batch = match args::doc(ctx.cmd, LIST_COLLECTIONS, "cursor")? {
        Some(c) => args::int(c, "cursor", "batchSize")?.map(|n| usize::try_from(n).unwrap_or(0)),
        None => None,
    };
    let db = ctx.db.to_owned();
    let only = super::authorize::collection_filter(ctx.backend, &db, ctx.cmd);
    let mut docs = Vec::new();
    for (ns, c) in ctx.backend.collections()?.into_iter().filter(|(ns, _)| ns.db == db) {
        if !super::authorize::collection_shown(only.as_deref(), &db, &ns.coll) {
            continue;
        }
        let kind = kinds::kind_of(&c)?;
        let mut d = Document::new().with("name", ns.coll.as_str()).with("type", kind.type_name());
        if !name_only {
            d.push("options", options_of(&c)?);
            let uuid = Value::Binary { subtype: 4, bytes: c.uuid.to_vec() };
            // A view has no UUID, and only normal and capped collections an `_id` index
            // (measured on a `refrouter` 9.0.4).
            match kind {
                CollectionKind::View { .. } => {
                    d.push("info", Document::new().with("readOnly", true))
                }
                _ => d.push("info", Document::new().with("readOnly", false).with("uuid", uuid)),
            }
            if matches!(kind, CollectionKind::Normal | CollectionKind::Capped { .. }) {
                d.push("idIndex", id_index(&c)?);
            }
        }
        if passes(filter.as_ref(), &d)? {
            docs.push(d);
        }
    }
    // The users, roles and versions of `admin`, generated from Group 0 (013 FR-030).
    if db == super::admin::ADMIN_DB {
        for g in Generated::ALL {
            if !g.exists(ctx.backend)?
                || !super::authorize::collection_shown(only.as_deref(), &db, g.name())
            {
                continue;
            }
            let mut d = Document::new().with("name", g.name()).with("type", "collection");
            if !name_only {
                let uuid = Value::Binary { subtype: 4, bytes: g.uuid().to_vec() };
                d.push("options", Document::new());
                d.push("info", Document::new().with("readOnly", false).with("uuid", uuid));
                d.push("idIndex", g.indexes().remove(0));
            }
            if passes(filter.as_ref(), &d)? {
                docs.push(d);
            }
        }
    }
    // The collections of `config` that show the cluster, generated from Group 0 (007 FR-017).
    if db == super::config_db::CONFIG_DB {
        for name in super::config_db::LISTED {
            let shown = super::authorize::collection_shown(only.as_deref(), &db, name);
            let d = super::config_db::listed(name, name_only);
            if shown && passes(filter.as_ref(), &d)? {
                docs.push(d);
            }
        }
    }
    let ns_text = format!("{db}.$cmd.listCollections");
    let mut state = QueryCursor::buffered(docs);
    let (first, short) = state.batch(ctx.backend, batch.or(Some(usize::MAX)))?;
    let id = keep(ctx, &ns_text, state, short, false)?;
    // `listCollections` writes the cursor's fields in this order (measured on a `refrouter` 9.0).
    let docs: Vec<Value> = first.into_iter().map(Value::Document).collect();
    let cursor = Document::new().with("id", id).with("ns", ns_text).with("firstBatch", docs);
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// The cluster's own database.
const CONFIG_DB: &str = "config";

/// `listIndexes`.
pub const LIST_INDEXES: &str = "listIndexes";

/// `listIndexes`: the `_id` index, then the secondary indexes that are ready (009).
pub fn list_indexes<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = match ctx.first() {
        Value::String(s) => {
            let ns = names::ns(ctx.db, s);
            names::check(&ns)?;
            ns
        }
        v => {
            let msg = format!("Collection name has invalid type {}", crate::types::type_name(v));
            return Err(CommandError::new(crate::error::INVALID_NAMESPACE, msg));
        }
    };
    let batch = match args::doc(ctx.cmd, LIST_INDEXES, "cursor")? {
        Some(c) => args::int(c, "cursor", "batchSize")?.map(|n| usize::try_from(n).unwrap_or(0)),
        None => None,
    };
    let build_info =
        args::bool_field(ctx.cmd, LIST_INDEXES, "includeIndexBuildInfo")?.unwrap_or(false);
    let ns_text = full(&ns);
    if let Some(g) = Generated::of(&ns) {
        let mut state = QueryCursor::buffered(g.indexes());
        let (first, short) = state.batch(ctx.backend, batch.or(Some(usize::MAX)))?;
        let id = keep(ctx, &ns_text, state, short, false)?;
        let docs: Vec<Value> = first.into_iter().map(Value::Document).collect();
        let cursor = Document::new().with("id", id).with("ns", ns_text).with("firstBatch", docs);
        return Ok(Document::new().with("cursor", cursor).with("ok", 1.0));
    }
    let Some(c) = ctx.backend.collection(&ns)? else {
        let msg = format!("ns does not exist: {ns_text}");
        return Err(missing_namespace(ctx.backend, &ns, msg)?);
    };
    kinds::not_a_view(&ns, &c)?;
    let raw = matches!(ctx.cmd.get("rawData"), Some(Value::Boolean(true)));
    let specs = match super::timeseries::spec_of(&c)? {
        // A time series has no `_id_` index; without `rawData` its indexes name the
        // measurements' fields (015 FR-008).
        Some(ts) => super::indexes::listed(ctx.backend, &c)?
            .into_iter()
            .map(|s| if raw { s } else { super::timeseries::user_index(&ts, &s) })
            .collect(),
        None => {
            let mut specs = vec![id_index(&c)?];
            specs.extend(super::indexes::listed(ctx.backend, &c)?);
            specs
        }
    };
    let entries = specs
        .into_iter()
        .map(|index| if build_info { Document::new().with("spec", index) } else { index })
        .collect();
    let mut state = QueryCursor::buffered(entries);
    let (first, short) = state.batch(ctx.backend, batch.or(Some(usize::MAX)))?;
    let id = keep(ctx, &ns_text, state, short, false)?;
    // The cursor's fields in this order (measured on a `refrouter` 9.0.4).
    let docs: Vec<Value> = first.into_iter().map(Value::Document).collect();
    let cursor = Document::new().with("id", id).with("ns", ns_text).with("firstBatch", docs);
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// `dropDatabase`.
pub fn drop_database<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let db = ctx.db.to_owned();
    names::check(&names::ns(&db, "x"))?;
    names::check_db_droppable(&db)?;
    let doomed: Vec<(Namespace, CollectionEntry)> =
        ctx.backend.collections()?.into_iter().filter(|(ns, _)| ns.db == db).collect();
    // A database without collections is still in the catalog until it is dropped (D-077).
    if !doomed.is_empty() || has_database(ctx.backend, &db)? {
        if !doomed.is_empty() {
            super::write_block::ddl(ctx.backend, &db)?;
        }
        ctx.backend.catalog_write(CatalogCommand::DropDatabase { db })?;
        for (ns, c) in &doomed {
            purge(ctx.backend, ns, c)?;
        }
    }
    Ok(Document::new().with("ok", 1.0))
}
