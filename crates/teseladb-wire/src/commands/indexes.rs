//! `createIndexes` and `dropIndexes` (009 T013, FR-006, FR-007, contract `index-commands.md`), with
//! the replies and errors of the reference router 9.0.4 (measured). The build itself, from
//! `deleteOnly` to `ready`, is [`super::index_build`]'s.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry, IndexState};
use teseladb_range::read::ReadConcern;

use super::crud::{MODIFY_CHUNK_KEYS, Writer, ensure_collection};
use super::index_build::{self, BuildOp, Stopped};
use super::index_maintenance::IndexRule;
use super::mutation::{Bound, DocMutation, Rules};
use super::names::{self, full};
use super::{Backend, Ctx, display, index_spec};
use crate::error::{
    BAD_VALUE, CANNOT_CREATE_INDEX, CLUSTERED_INDEX_DROP, CommandError, FAILED_TO_PARSE,
    HASHED_FIELDS, HASHED_UNIQUE, INTERNAL_ERROR, INVALID_INDEX_SPECIFICATION_OPTION,
    INVALID_NAMESPACE, INVALID_OPTIONS, OPERATION_FAILED, TYPE_MISMATCH,
};
use teseladb_collation::{ParsedCollation, parse_collation};
use teseladb_index::build::CommitQuorum;

/// `createIndexes`.
pub const CREATE_INDEXES: &str = "createIndexes";
/// `dropIndexes`.
pub const DROP_INDEXES: &str = "dropIndexes";

/// Fields of a specification.
const KEY: &str = "key";
const NAME: &str = "name";
const VERSION: &str = "v";
/// Buckets of an index (D-008).
const BUCKETS: &str = "teselaBuckets";
/// `dropIndexes` of every index but `_id`.
const ALL: &str = "*";
/// Options the reference server fills in when it stores a specification (measured on the reference
/// router 9.0.4).
const UNIQUE: &str = "unique";
const PREPARE_UNIQUE: &str = "prepareUnique";
const HIDDEN: &str = "hidden";
const SPARSE: &str = "sparse";
const BACKGROUND: &str = "background";
const EXPIRE_AFTER_SECONDS: &str = "expireAfterSeconds";
const COLLATION: &str = "collation";
/// The options that make another index on the same key (measured on the reference router 9.0.4).
const IDENTIFYING: [&str; 5] =
    [UNIQUE, SPARSE, "partialFilterExpression", COLLATION, "wildcardProjection"];
/// The options a requested index shows before its key in a message (measured).
const SHOWN_FIRST: [&str; 5] = [BACKGROUND, UNIQUE, SPARSE, HIDDEN, PREPARE_UNIQUE];
const SPHERE: &str = "2dsphere";
const SPHERE_VERSION: &str = "2dsphereIndexVersion";
/// The namespace field of old specifications.
const NS: &str = "ns";
/// The `2dsphereIndexVersion` of a new index.
const SPHERE_VERSION_DEFAULT: i32 = 4;
const PLANE: &str = "2d";
const PLANE_BOUNDS: [&str; 2] = ["min", "max"];
const HASHED: &str = "hashed";
const TEXT: &str = "text";
const TEXT_KEY: &str = "_fts";
const TEXT_TERMS: &str = "_ftsx";
const WEIGHTS: &str = "weights";
const DEFAULT_LANGUAGE: &str = "default_language";
const LANGUAGE_OVERRIDE: &str = "language_override";
const TEXT_INDEX_VERSION: &str = "textIndexVersion";
/// The defaults of a text index: `english`, the field `language` and version 3.
const DEFAULT_LANGUAGE_VALUE: &str = "english";
const LANGUAGE_OVERRIDE_VALUE: &str = "language";
const TEXT_INDEX_VERSION_VALUE: i32 = 3;
/// Specification errors `createIndexes` shows inside «Error in specification».
const SPEC_ERRORS: [i32; 5] = [
    CANNOT_CREATE_INDEX.code,
    crate::error::PROJECTION_EXCLUSION_IN_INCLUSION.code,
    FAILED_TO_PARSE.code,
    INVALID_INDEX_SPECIFICATION_OPTION.code,
    TYPE_MISMATCH.code,
];
/// The prefix of an error about one specification.
const IN_SPECIFICATION: &str = "Error in specification";

/// The reference server's code of an index with another name and the same key.
pub const INDEX_OPTIONS_CONFLICT: crate::error::Code =
    crate::error::Code { code: 85, name: "IndexOptionsConflict" };
/// The reference server's code of an index with the same name and another key or options.
pub const INDEX_KEY_SPECS_CONFLICT: crate::error::Code =
    crate::error::Code { code: 86, name: "IndexKeySpecsConflict" };

/// The collection a command names first.
fn named<B: Backend + ?Sized>(ctx: &Ctx<'_, B>) -> Result<Namespace, CommandError> {
    match ctx.first() {
        Value::String(s) => {
            let ns = names::ns(ctx.db, s);
            names::check(&ns)?;
            Ok(ns)
        }
        v => {
            let msg = format!("Collection name has invalid type {}", crate::types::type_name(v));
            Err(CommandError::new(INVALID_NAMESPACE, msg))
        }
    }
}

/// The order the reference router 9.0.4 stores the fields of a specification in, whatever order the
/// command gave them in (measured: the order of the fields of the reference server's index
/// specification); `teselaBuckets` (D-008) goes before `prepareUnique`, which is always last.
const STORED_ORDER: [&str; 29] = [
    VERSION,
    KEY,
    NAME,
    "ns",
    BACKGROUND,
    UNIQUE,
    HIDDEN,
    "partialFilterExpression",
    SPARSE,
    EXPIRE_AFTER_SECONDS,
    "storageEngine",
    WEIGHTS,
    DEFAULT_LANGUAGE,
    LANGUAGE_OVERRIDE,
    TEXT_INDEX_VERSION,
    SPHERE_VERSION,
    "bits",
    "min",
    "max",
    "bucketSize",
    COLLATION,
    "wildcardProjection",
    "coarsestIndexedLevel",
    "finestIndexedLevel",
    "dropDups",
    "originalSpec",
    "clustered",
    BUCKETS,
    PREPARE_UNIQUE,
];

/// `spec` with its fields in [`STORED_ORDER`].
pub(super) fn in_stored_order(spec: &Document) -> Document {
    let rank = |k: &str| STORED_ORDER.iter().position(|f| *f == k).unwrap_or(STORED_ORDER.len());
    let mut fields: Vec<(&str, &Value)> = spec.iter().collect();
    fields.sort_by_key(|(k, _)| rank(k));
    fields.into_iter().fold(Document::new(), |d, (k, v)| d.with(k, v.clone()))
}

/// The boolean options as the reference router 9.0.4 stores them (measured): `background` and
/// `sparse` as booleans; `unique`, `hidden` and `prepareUnique` only when true.
fn stored_booleans(spec: &mut Document) {
    for k in [BACKGROUND, SPARSE] {
        if let Some(b) = spec.get(k).and_then(crate::types::truthy) {
            spec.insert(k, b);
        }
    }
    for k in [UNIQUE, HIDDEN, PREPARE_UNIQUE] {
        match spec.get(k).map(crate::types::truthy) {
            Some(Some(true)) => {
                spec.insert(k, true);
            }
            Some(_) => {
                spec.remove(k);
            }
            None => {}
        }
    }
}

/// The specification as Group 0 keeps it and `listIndexes` shows it (measured on the reference
/// router 9.0.4): its fields in [`STORED_ORDER`], the booleans of [`stored_booleans`], a collation
/// in full (none for `simple`), the `2dsphereIndexVersion` of a new `2dsphere` index, the bounds of
/// a `2d` index as doubles, and a text index with its key `_fts`/`_ftsx`, its `weights` and its
/// language options.
fn stored(spec: &Document, version: i32) -> Result<Document, CommandError> {
    let mut out = spec.clone();
    out.insert(VERSION, version);
    // `ns` (old dumps carry it) is dropped, whatever namespace it names (measured).
    out.remove(NS);
    stored_booleans(&mut out);
    if let Some(Value::Document(c)) = out.get(COLLATION).cloned() {
        match parse_collation(RawDoc::from_bytes(&c.to_bytes()?)?)? {
            ParsedCollation::Simple => {
                out.remove(COLLATION);
            }
            ParsedCollation::Collation(_, full) => {
                out.insert(COLLATION, full.to_document());
            }
        }
    }
    let key = match out.get(KEY) {
        Some(Value::Document(k)) => k.clone(),
        _ => Document::new(),
    };
    let kinds: Vec<&Value> = key.iter().map(|(_, v)| v).collect();
    if kinds.contains(&&Value::from(SPHERE)) && out.get(SPHERE_VERSION).is_none() {
        out.push(SPHERE_VERSION, SPHERE_VERSION_DEFAULT);
    }
    if kinds.contains(&&Value::from(PLANE)) {
        for bound in PLANE_BOUNDS {
            if let Some(n) = out.get(bound).and_then(crate::types::number) {
                out.insert(bound, n);
            }
        }
    }
    if kinds.contains(&&Value::from(TEXT)) {
        out = text_spec(out, &key);
    }
    Ok(in_stored_order(&out))
}

/// A text index as the reference server stores it: the text fields become `_fts: "text", _ftsx: 1`
/// in the key, each one gets weight 1 unless `weights` gives another (by field name), and the
/// language options and the version go last.
fn text_spec(spec: Document, key: &Document) -> Document {
    let mut new_key = Document::new();
    let mut weights: std::collections::BTreeMap<String, Value> = match spec.get(WEIGHTS) {
        Some(Value::Document(w)) => w.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        _ => std::collections::BTreeMap::new(),
    };
    for (field, kind) in key.iter() {
        if kind != &Value::from(TEXT) {
            new_key.push(field, kind.clone());
            continue;
        }
        if new_key.get(TEXT_KEY).is_none() {
            new_key.push(TEXT_KEY, TEXT);
            new_key.push(TEXT_TERMS, 1);
        }
        weights.entry(field.to_string()).or_insert(Value::Int32(1));
    }
    let tail = [WEIGHTS, DEFAULT_LANGUAGE, LANGUAGE_OVERRIDE, TEXT_INDEX_VERSION];
    let mut out = Document::new();
    for (k, v) in spec.iter().filter(|(k, _)| !tail.contains(k)) {
        out.push(k, if k == KEY { Value::Document(new_key.clone()) } else { v.clone() });
    }
    let weights = weights.into_iter().fold(Document::new(), |d, (k, v)| d.with(k.as_str(), v));
    out.push(WEIGHTS, weights);
    let given = |k: &str, default: Value| spec.get(k).cloned().unwrap_or(default);
    out.push(DEFAULT_LANGUAGE, given(DEFAULT_LANGUAGE, Value::from(DEFAULT_LANGUAGE_VALUE)));
    out.push(LANGUAGE_OVERRIDE, given(LANGUAGE_OVERRIDE, Value::from(LANGUAGE_OVERRIDE_VALUE)));
    out.push(TEXT_INDEX_VERSION, given(TEXT_INDEX_VERSION, Value::Int32(TEXT_INDEX_VERSION_VALUE)));
    out
}

/// The reference router 9.0.4 forwards the shard's error of `createIndexes` and `dropIndexes` with
/// `code` and `codeName` first, then the other fields, `ok` and `errmsg` (measured).
fn forwarded(r: Result<Document, CommandError>) -> Result<Document, CommandError> {
    r.or_else(|e| match e.origin {
        // The router's own error goes as any error: no shard answered it.
        crate::error::ErrorOrigin::Router => Err(e),
        _ => Ok(code_first(&e)),
    })
}

/// Error `e` with `code` and `codeName` first, then the other fields, `ok` and `errmsg`.
fn code_first(e: &CommandError) -> Document {
    let mut d = Document::new().with("code", e.code.code).with("codeName", e.code.name);
    for (k, v) in e.extra.iter() {
        d.push(k, v.clone());
    }
    d.with("ok", 0.0).with("errmsg", e.errmsg.as_str())
}

/// A specification error of `createIndexes` inside «Error in specification <spec> :: caused by
/// ::», as the reference router 9.0.4 shows it (measured), unless the message already has it.
fn in_specification(spec: &Document, e: CommandError) -> CommandError {
    if e.errmsg.starts_with(IN_SPECIFICATION)
        || !SPEC_ERRORS.contains(&e.code.code)
        || e.errmsg.starts_with("TTL indexes")
    {
        return e;
    }
    let shown = display(&Value::Document(spec.clone()));
    CommandError { errmsg: format!("{IN_SPECIFICATION} {shown} :: caused by :: {}", e.errmsg), ..e }
}

/// The cause of a build of two identical specifications (measured on the reference router 9.0.4).
const IDENTICAL_INDEXES: &str =
    "Cannot build two identical indexes. Try again without duplicate indexes.";

/// «Index build failed» around `cause`, with a new build identifier and the collection's UUID
/// (measured on the reference router 9.0.4).
fn build_failed<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
    cause: &str,
) -> (String, String) {
    let build = super::uuid_text(&super::new_uuid(backend));
    let coll_uuid = super::uuid_text(&coll.uuid);
    let ns = full(ns);
    (
        format!(
            "Index build failed: {build}: Collection {ns} ( {coll_uuid} ) :: caused by :: {cause}"
        ),
        build,
    )
}

/// The specification as the build shows it when it conflicts with another of the same command:
/// `v`, then its fields as given (measured on the reference router 9.0.4 on 03-11-2026).
fn as_given(spec: &Document, version: i32) -> String {
    let mut out = Document::new().with(VERSION, version);
    for (k, v) in spec.iter().filter(|(k, _)| *k != VERSION) {
        out.push(k, v.clone());
    }
    display(&Value::Document(out))
}

/// The specification as the reference router 9.0.4 shows a requested index in its errors: `v`, the
/// options, then `key` and `name` (measured).
fn requested(spec: &Document, version: i32) -> String {
    let mut out = Document::new().with(VERSION, version);
    for (k, v) in spec.iter().filter(|(k, _)| SHOWN_FIRST.contains(k)) {
        out.push(k, v.clone());
    }
    for (k, v) in [KEY, NAME].iter().filter_map(|k| spec.get(k).map(|v| (*k, v))) {
        out.push(k, v.clone());
    }
    let rest = |k: &str| ![VERSION, KEY, NAME].contains(&k) && !SHOWN_FIRST.contains(&k);
    for (k, v) in spec.iter().filter(|(k, _)| rest(k)) {
        out.push(k, v.clone());
    }
    display(&Value::Document(out))
}

/// The options of a stored specification that tell two indexes on the same key apart.
fn identity(spec: &Document) -> Vec<(String, Value)> {
    options(spec).into_iter().filter(|(k, _)| IDENTIFYING.contains(&k.as_str())).collect()
}

/// The options of a stored specification, without `v`, `key` and `name`.
fn options(spec: &Document) -> Vec<(String, Value)> {
    spec.iter()
        .filter(|(k, _)| ![VERSION, KEY, NAME].contains(k))
        .map(|(k, v)| (k.to_owned(), v.clone()))
        .collect()
}

/// An index of the collection: its specification as stored.
struct Existing {
    spec: Document,
    entry: Option<IndexEntry>,
}

/// The number of indexes of collection `c`, as `dropIndexes` (`nIndexesWas`) and
/// `createIndexes` (`numIndexesBefore`) count them: a clustered collection's index is not one
/// of them (measured on the reference router 9.0.4 on 05-11-2026).
pub fn count<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<i32, CommandError> {
    let all = existing(backend, c)?.len();
    let clustered = usize::from(super::catalog::is_clustered(c)?);
    Ok(i32::try_from(all - clustered).unwrap_or(i32::MAX))
}

fn existing<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<Vec<Existing>, CommandError> {
    // A time series has no `_id_` index (measured on the reference router 9.0.4).
    let mut out = match super::timeseries::spec_of(c)? {
        Some(_) => Vec::new(),
        None => vec![Existing { spec: super::catalog::listed_id_index(c)?, entry: None }],
    };
    for e in backend.indexes(c.id)? {
        let spec = Document::from(RawDoc::from_bytes(&e.spec)?);
        out.push(Existing { spec, entry: Some(e) });
    }
    Ok(out)
}

/// What a requested index is next to the existing ones: new, already there, or a conflict.
/// `shown`: the requested index as the error shows it.
fn compare(
    spec: &Document,
    version: i32,
    all: &[Existing],
    shown: &str,
) -> Result<bool, CommandError> {
    let normal = stored(spec, version)?;
    let name = normal.get(NAME);
    let key = normal.get(KEY);
    if let Some(same) = all.iter().find(|e| e.spec.get(NAME) == name) {
        if same.spec.get(KEY) == key && options(&same.spec) == options(&normal) {
            return Ok(false);
        }
        let msg = format!(
            "An existing index has the same name as the requested index. When index names are \
             not specified, they are auto generated and can cause conflicts. Please refer to our \
             documentation. Requested index: {shown}, existing index: {}",
            display(&Value::Document(same.spec.clone()))
        );
        return Err(CommandError::new(INDEX_KEY_SPECS_CONFLICT, msg));
    }
    let equivalent =
        all.iter().find(|e| e.spec.get(KEY) == key && identity(&e.spec) == identity(&normal));
    if let Some(other) = equivalent {
        if options(&other.spec) != options(&normal) {
            let msg = format!(
                "An equivalent index already exists with a different name and options. \
                 Requested index: {shown}, existing index: {}",
                display(&Value::Document(other.spec.clone()))
            );
            return Err(CommandError::new(INDEX_OPTIONS_CONFLICT, msg));
        }
        let other_name = match other.spec.get(NAME) {
            Some(Value::String(n)) => n.clone(),
            _ => String::new(),
        };
        let msg = format!("Index already exists with a different name: {other_name}");
        return Err(CommandError::new(INDEX_OPTIONS_CONFLICT, msg));
    }
    Ok(true)
}

/// `createIndexes`.
pub fn create_indexes<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    match create(ctx) {
        Err(e) if e.origin == crate::error::ErrorOrigin::Shard => Ok(shard_error(ctx.backend, e)),
        other => other,
    }
}

/// A shard's error of `createIndexes` as the reference router 9.0.4 relays it (measured on
/// 03-11-2026): the shard's own reply under `raw`, then `code`, `codeName`, the other fields, `ok`
/// and `errmsg`. An error of the router's parse goes as any other.
pub fn shard_error<B: Backend + ?Sized>(backend: &mut B, e: CommandError) -> Document {
    let raw = Document::new().with(super::raw_name(backend), e.to_document());
    let mut out = Document::new().with("raw", raw);
    for (k, v) in code_first(&e).iter() {
        out.push(k, v.clone());
    }
    out
}

/// Whether a specification asks for a unique index.
fn is_unique(spec: &Document) -> bool {
    spec.get(UNIQUE).and_then(crate::types::truthy) == Some(true)
}

fn create<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = named(ctx)?;
    let specs: Vec<Document> = match ctx.cmd.get("indexes") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Document(d) => Ok(d.clone()),
                other => {
                    let msg = format!(
                        "The elements of the 'indexes' array must be objects, but found {}",
                        crate::types::type_name(other)
                    );
                    Err(CommandError::new(TYPE_MISMATCH, msg))
                }
            })
            .collect::<Result<_, _>>()?,
        Some(other) => {
            let msg = format!(
                "BSON field 'createIndexes.indexes' is the wrong type '{}', expected type 'array'",
                crate::types::type_name(other)
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
        None => {
            let msg = "BSON field 'createIndexes.indexes' is missing but a required field";
            return Err(CommandError::new(crate::error::FAILED_TO_PARSE, msg));
        }
    };
    if specs.is_empty() {
        return Err(CommandError::new(BAD_VALUE, "Must specify at least one index to create"));
    }
    // A time series keeps its indexes on its buckets (015 FR-008): the bucket specifications are
    // the ones checked, as the reference server checks them after translating them; with `rawData`
    // they are given on the buckets already (measured on a reference replica set member 9.0.4 on
    // 02-11-2026).
    let raw = matches!(ctx.cmd.get("rawData"), Some(Value::Boolean(true)));
    let asked_count = specs.len();
    let specs = match ctx.backend.collection(&ns)? {
        Some(c) => match super::timeseries::spec_of(&c)?.filter(|_| !raw) {
            Some(ts) => specs
                .iter()
                .map(|s| super::timeseries::bucket_index(&ts, s))
                .collect::<Result<Vec<_>, _>>()?,
            None if super::catalog::is_clustered(&c)? => {
                let kept: Vec<Document> =
                    specs.into_iter().filter(|s| !is_clustered_id(s)).collect();
                if kept.is_empty() {
                    return already_there(ctx, &c);
                }
                kept
            }
            None => specs,
        },
        None => specs,
    };
    crate::api_version::check_text_indexes(ctx.cmd)?;
    let versions = specs
        .iter()
        .map(|s| index_spec::check(s).map_err(|e| in_specification(s, e)))
        .collect::<Result<Vec<i32>, _>>()?;
    let id_key = Value::Document(Document::new().with("_id", 1));
    for s in specs.iter().filter(|s| s.get(KEY) == Some(&id_key)) {
        index_spec::check_id_fields(s)?;
    }

    let quorum = index_build::commit_quorum(ctx.cmd)?;
    names::check_writable(&ns)?;
    // The reference router creates the collection before it asks the shard for the index, so the
    // shard never creates it (measured on 9.0.4: `createdCollectionAutomatically: false`).
    let created = false;
    let coll = ensure_collection(ctx, &ns)?;
    index_build::check_quorum(ctx.backend, &coll, quorum.asked)?;
    for (spec, version) in specs.iter().zip(&versions) {
        let key = match spec.get(KEY) {
            Some(Value::Document(k)) => k.clone(),
            _ => Document::new(),
        };
        let hashed = key.iter().filter(|(_, v)| *v == &Value::from(HASHED)).count();
        if hashed > 1 {
            let (msg, build) = build_failed(ctx.backend, &ns, &coll, "");
            let detail = format!(
                "Caught exception during index builder ({build}) initialization on namespace {} \
                 ({}). {} index specs provided. First index spec: {} :: caused by :: A maximum \
                 of one index field is allowed to be hashed but found {hashed} for 'key' {}",
                full(&ns),
                super::uuid_text(&coll.uuid),
                specs.len(),
                requested(spec, *version),
                display(&Value::Document(key.clone()))
            );
            return Err(CommandError::new(HASHED_FIELDS, format!("{msg}{detail}")));
        }
        if is_unique(spec) && hashed > 0 {
            let (msg, build) = build_failed(ctx.backend, &ns, &coll, "");
            let detail = format!(
                "Caught exception during index builder ({build}) initialization on namespace {} \
                 ({}). {} index specs provided. First index spec: {} :: caused by :: Currently \
                 hashed indexes cannot guarantee uniqueness. Use a regular index.",
                full(&ns),
                super::uuid_text(&coll.uuid),
                specs.len(),
                requested(spec, *version)
            );
            return Err(CommandError::new(HASHED_UNIQUE, format!("{msg}{detail}")));
        }
    }
    let all = existing(ctx.backend, &coll)?;
    let before = usize::try_from(count(ctx.backend, &coll)?).unwrap_or(0);
    let mut new = Vec::new();
    // The specifications before this one in the command, as the build compares them.
    let mut asked: Vec<Existing> = Vec::new();
    for (spec, version) in specs.iter().zip(&versions) {
        let cause = match compare(spec, *version, &asked, &as_given(spec, *version)) {
            Ok(true) => None,
            Ok(false) => Some(CommandError::new(OPERATION_FAILED, IDENTICAL_INDEXES)),
            Err(e) => Some(e),
        };
        if let Some(e) = cause {
            let (msg, _) = build_failed(ctx.backend, &ns, &coll, &e.errmsg);
            return Err(CommandError::new(e.code, msg));
        }
        asked.push(Existing { spec: stored(spec, *version)?, entry: None });
        if compare(spec, *version, &all, &requested(spec, *version))? {
            new.push((spec.clone(), *version));
        }
    }
    let mut reply = Document::new().with("numIndexesBefore", before as i32);
    if new.is_empty() {
        reply.push("numIndexesAfter", before as i32);
        reply.push("commitQuorum", quorum.shown);
        reply.push("note", "all indexes already exist");
        return Ok(with_raw_first(ctx.backend, reply.with("ok", 1.0)));
    }
    build(ctx, &ns, &coll, &new, quorum.asked)?;
    reply.push("numIndexesAfter", (before + new.len()) as i32);
    reply.push("createdCollectionAutomatically", created);
    reply.push("commitQuorum", quorum.shown);
    if new.len() < asked_count {
        reply.push("note", "index already exists");
    }
    Ok(with_raw_first(ctx.backend, reply.with("ok", 1.0)))
}

/// Whether `spec` asks for the clustered index of `_id`: on a clustered collection it is there
/// already, whatever its name (measured on the reference router 9.0.4 on 05-11-2026).
fn is_clustered_id(spec: &Document) -> bool {
    let id_key = Value::Document(Document::new().with("_id", 1));
    spec.get("clustered").is_some() && spec.get(KEY) == Some(&id_key)
}

/// The reply of a `createIndexes` whose indexes all exist on collection `c`.
fn already_there<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    c: &CollectionEntry,
) -> Result<Document, CommandError> {
    let quorum = index_build::commit_quorum(ctx.cmd)?;
    let before = count(ctx.backend, c)?;
    let reply = Document::new()
        .with("numIndexesBefore", before)
        .with("numIndexesAfter", before)
        .with("commitQuorum", quorum.shown)
        .with("note", "all indexes already exist")
        .with("ok", 1.0);
    Ok(with_raw_first(ctx.backend, reply))
}

/// Reply `reply` after the node's own under `raw`, as the reference router gives its shard's
/// (measured).
fn with_raw_first<B: Backend + ?Sized>(backend: &mut B, reply: Document) -> Document {
    let mut out =
        Document::new().with("raw", Document::new().with(super::raw_name(backend), reply.clone()));
    for (k, v) in reply.iter() {
        out.push(k, v.clone());
    }
    out
}

/// Creates index `index` of time series `coll` (as asked: its bucket specification is made
/// here) for the node itself, as `create` does for a time series' default index.
pub(super) fn create_internal<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    coll: &CollectionEntry,
    index: &Document,
) -> Result<(), CommandError> {
    let spec = match super::timeseries::spec_of(coll)? {
        Some(ts) => super::timeseries::bucket_index(&ts, index)?,
        None => index.clone(),
    };
    let version = index_spec::check(&spec).map_err(|e| in_specification(&spec, e))?;
    build(ctx, ns, coll, &[(spec, version)], teseladb_index::build::CommitQuorum::VotingMembers)
}

/// Creates `new` (delete-only, then write-only), backfills them with their writes waiting for
/// `asked`, and makes them `ready`; on a failure the indexes it created are dropped.
fn build<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    coll: &CollectionEntry,
    new: &[(Document, i32)],
    asked: CommitQuorum,
) -> Result<(), CommandError> {
    let mut made: Vec<IndexEntry> = Vec::new();
    let backend = &mut *ctx.backend;
    let r = (|| {
        let mut specs = Vec::new();
        for (spec, version) in new {
            let doc = stored(spec, *version)?;
            let name = match doc.get(NAME) {
                Some(Value::String(n)) => n.clone(),
                _ => return Err(CommandError::new(INTERNAL_ERROR, "an index without name")),
            };
            let buckets = doc.get(BUCKETS).map(index_spec::buckets).transpose()?.unwrap_or(0);
            let cmd =
                IndexCommand::Create { collection: coll.id, name, spec: doc.to_bytes()?, buckets };
            made.push(index_build::start(backend, coll.id, cmd)?);
            specs.push(doc);
        }
        let now = backend.now_ms();
        let op = BuildOp::begin(ctx.shared, full(ns), ctx.cmd, specs, now);
        let run = index_build::Backfill { asked: Some(asked), op: Some(op) };
        run.run(backend, coll, &rules_of(ns, coll, &made)?).map_err(|stopped| match stopped {
            Stopped::Node(e) => e,
            Stopped::Build(e) => {
                let (errmsg, _) = build_failed(backend, ns, coll, &e.errmsg);
                CommandError { errmsg, ..e }
            }
        })?;
        for e in &made {
            backend.index_write(IndexCommand::SetState { id: e.id, state: IndexState::Ready })?;
        }
        Ok(())
    })();
    if r.is_err() {
        for e in &made {
            drop_one(backend, ns, coll, e)?;
        }
    }
    r
}

/// The rules of the writes of an index build or drop: the collection and `indexes`.
fn rules_of(
    ns: &Namespace,
    coll: &CollectionEntry,
    indexes: &[IndexEntry],
) -> Result<Rules, CommandError> {
    let indexes = indexes
        .iter()
        .map(|e| {
            let spec = Document::from(RawDoc::from_bytes(&e.spec)?);
            Ok(IndexRule { id: e.id, spec, buckets: e.buckets, delete_only: false })
        })
        .collect::<Result<Vec<_>, CommandError>>()?;
    // A capped collection's entries end in the document's `seq` (015 FR-003).
    let capped = super::capped::rule_of(coll)?;
    let (collection, tenant) = (coll.id, coll.tenant);
    // A collection with a shard key's entries end in the document's key (018 FR-001).
    let shard_key = super::sharding::shard_key_of(coll)?.map(|k| k.key);
    Ok(Rules { ns: full(ns), collection, tenant, indexes, capped, shard_key, ..Rules::default() })
}

/// Writes the entries of `indexes` for every document of the collection, with the writes of
/// the node (`w: "majority"`).
pub(super) fn backfill<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
    indexes: &[IndexEntry],
) -> Result<(), CommandError> {
    let rules = rules_of(ns, coll, indexes)?;
    index_build::Backfill::default().run(backend, coll, &rules).map_err(Stopped::error)
}

/// Removes index `e`: state `dropping` (writes stop keeping it), its entries, then its entry in
/// Group 0.
pub(super) fn drop_one<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    coll: &CollectionEntry,
    e: &IndexEntry,
) -> Result<(), CommandError> {
    backend.index_write(IndexCommand::SetState { id: e.id, state: IndexState::Dropping })?;
    let rules =
        Rules { ns: full(ns), collection: coll.id, tenant: coll.tenant, ..Rules::default() };
    let mut w = Writer::internal();
    // Its entries, then the guards of `prepareUnique` (009 FR-004).
    for id in [e.id, teseladb_index::prepared::guards_of(e.id)] {
        let start =
            teseladb_keys::KeyBuilder::index_prefix(coll.tenant, coll.id, id).as_bytes().to_vec();
        let end = teseladb_keys::KeyBuilder::index_prefix(coll.tenant, coll.id, id + 1)
            .as_bytes()
            .to_vec();
        loop {
            let rows = backend.scan(&start, &end, MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
            if rows.is_empty() {
                break;
            }
            let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
            let mutation = DocMutation::Erase { rules: rules.clone() };
            w.write(backend, &Bound { mutation, keys }, 0)?;
        }
    }
    backend.index_write(IndexCommand::Drop { id: e.id })
}

/// `dropIndexes`: by name, by key, a list of names or `"*"`; a name that is not there is not an
/// error on the reference router 9.0.4 (measured).
pub fn drop_indexes<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    forwarded(drop(ctx))
}

fn drop<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let ns = named(ctx)?;
    let Some(coll) = ctx.backend.collection(&ns)? else {
        let msg = format!("ns not found {}", full(&ns));
        return Err(super::catalog::missing_namespace(ctx.backend, &ns, msg)?);
    };
    let all = existing(ctx.backend, &coll)?;
    let was = count(ctx.backend, &coll)?;
    let refuse_id: fn() -> CommandError = match super::catalog::is_clustered(&coll)? {
        // Measured on the reference router 9.0.4 on 05-11-2026.
        true => {
            || CommandError::new(CLUSTERED_INDEX_DROP, "It is illegal to drop the clusteredIndex")
        }
        false => || CommandError::new(INVALID_OPTIONS, "cannot drop _id index"),
    };
    let pick = |e: &Existing, which: &Value| match which {
        Value::String(n) => e.spec.get(NAME) == Some(&Value::from(n.as_str())),
        Value::Document(k) => e.spec.get(KEY) == Some(&Value::Document(k.clone())),
        _ => false,
    };
    let which = match (ctx.cmd.get("index").cloned(), super::timeseries::spec_of(&coll)?) {
        // A key of a time series' index names the measurements' fields.
        (Some(Value::Document(k)), Some(ts)) => {
            let asked = Document::new().with(KEY, k);
            let bucket = super::timeseries::bucket_index(&ts, &asked)?;
            bucket.get(KEY).cloned().unwrap_or(Value::Null)
        }
        (which, _) => which.unwrap_or(Value::Null),
    };
    let targets: Vec<&Existing> = match &which {
        Value::String(s) if s == ALL => all.iter().filter(|e| e.entry.is_some()).collect(),
        Value::String(_) | Value::Document(_) => all.iter().filter(|e| pick(e, &which)).collect(),
        Value::Array(names) => all.iter().filter(|e| names.iter().any(|n| pick(e, n))).collect(),
        other => {
            let msg = format!(
                "BSON field 'dropIndexes.index' is the wrong type '{}', expected types '[string, object, array]'",
                crate::types::type_name(other)
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
    };
    if targets.iter().any(|e| e.entry.is_none()) {
        return Err(refuse_id());
    }
    for e in targets.into_iter().filter_map(|e| e.entry.clone()) {
        drop_one(ctx.backend, &ns, &coll, &e)?;
    }
    let mut reply = Document::new().with("nIndexesWas", was);
    if matches!(&which, Value::String(s) if s == ALL) {
        reply.push("msg", "non-_id indexes dropped for collection");
    }
    // The reference router gives its shard's reply last, under `raw` (measured).
    let reply = reply.with("ok", 1.0);
    let raw = Document::new().with(super::raw_name(ctx.backend), reply.clone());
    Ok(reply.with("raw", raw))
}

/// The secondary indexes `listIndexes` shows: the ready ones, as stored.
pub fn listed<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
) -> Result<Vec<Document>, CommandError> {
    backend
        .indexes(c.id)?
        .into_iter()
        .filter(|e| e.state == IndexState::Ready)
        .map(|e| Ok(Document::from(RawDoc::from_bytes(&e.spec)?)))
        .collect()
}

/// Drops every index of a collection that is being dropped, with its entries.
pub fn drop_all<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
) -> Result<(), CommandError> {
    for e in backend.indexes(c.id)? {
        drop_one(backend, ns, c, &e)?;
    }
    Ok(())
}
