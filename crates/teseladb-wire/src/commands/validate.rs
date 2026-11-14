//! `validate` (010 FR-017, T034): the documents of a collection against the entries of each of
//! its indexes, with the verification of 009 (`teseladb_index::verify::verify_range`), read in
//! one pass of the collection's keys. The reply has the fields and the order of a `refrouter` 9.0.4
//! (measured 01-11-2026, `lc12`), and the shard's own reply under `raw`; what an inconsistent
//! index adds (`missingIndexEntries`, `extraIndexEntries`, `errors`, `advice`) was measured on a
//! `refnode` 9.0.4 with the fail points `skipIndexNewRecords` and
//! `skipUnindexingDocumentWhenDeleted`. TeselaDB has no record ids, so the entries carry none
//! (DV-075).

use std::collections::{BTreeMap, BTreeSet};

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::spec::index_def;
use teseladb_index::verify::{VerifyReport, verify_range};
use teseladb_keys::{Direction, Key, KeyBuilder, PrimaryKey, TypeBits};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::indexes::IndexState;
use teseladb_range::read::ReadConcern;

use super::index_maintenance::{MULTIKEY_MARKS, collation_of, keygen_error};
use super::kinds::{kind_of, not_a_view};
use super::names::{self, full};
use super::query::documents_of;
use super::{Backend, Ctx};
use crate::error::{CommandError, INTERNAL_ERROR, INVALID_NAMESPACE, INVALID_OPTIONS};
use crate::types::type_name;
use teseladb_meta::collection_types::CollectionKind;

/// The command's name.
pub const VALIDATE: &str = "validate";

/// Keys read from the store at a time.
const CHUNK: usize = 1024;

/// What a client may ask of `validate`.
#[derive(Clone, Copy, Debug, Default)]
struct Options {
    full: bool,
    background: bool,
    metadata: bool,
    repair: bool,
    conformance: bool,
}

impl Options {
    /// The options of `cmd`, refused as a `refrouter` 9.0.4 refuses them (measured).
    fn of(cmd: &Document) -> Result<Options, CommandError> {
        // Any value but `false`, `0` and `null` asks for the option (measured: `full: "a"`).
        let on = |k: &str| {
            cmd.get(k).is_some_and(|v| crate::types::truthy(v).unwrap_or(*v != Value::Null))
        };
        let o = Options {
            full: on("full"),
            background: on("background"),
            metadata: on("metadata"),
            repair: on("repair"),
            conformance: on("checkBSONConformance"),
        };
        let refuse = |msg: &str| Err(CommandError::new(INVALID_OPTIONS, msg));
        if o.repair {
            return refuse(
                "Running the validate command with { repair: true } can only be performed in \
                 standalone mode.",
            );
        }
        if o.metadata && (o.full || o.background || o.conformance) {
            return refuse(
                "Running the validate command with { metadata: true } is not supported with any \
                 other options",
            );
        }
        if o.background && o.full {
            return refuse(
                "Running the validate command with both { background: true } and { full: true } \
                 is not supported.",
            );
        }
        Ok(o)
    }
}

/// One index as `validate` checks it.
struct Checked {
    id: u32,
    name: String,
    spec: Document,
    multikey: bool,
    /// Entries held, for an index the verification does not read (a `text` index, kept by 011).
    held_only: Option<u64>,
}

/// `validate`.
pub fn validate<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let coll = match ctx.first() {
        Value::String(s) => s.clone(),
        other => {
            let msg = format!("Collection name has invalid type {}", type_name(other));
            return Err(CommandError::new(INVALID_NAMESPACE, msg));
        }
    };
    if coll.is_empty() {
        let msg = format!("Invalid namespace specified '{}'", ctx.db);
        return Err(CommandError::new(INVALID_NAMESPACE, msg));
    }
    let options = Options::of(ctx.cmd)?;
    let ns = names::ns(ctx.db, &coll);
    names::check(&ns)?;
    let Some(c) = ctx.backend.collection(&ns)? else {
        let msg = format!("Collection '{}' does not exist to validate.", full(&ns));
        return Err(super::catalog::missing_namespace(ctx.backend, &ns, msg)?);
    };
    not_a_view(&ns, &c)?;
    let reply = shard_reply(ctx.backend, &ns, &c, options)?;
    let mut out = reply.clone();
    let shard = super::shard_name(ctx.backend.node());
    out.push("raw", Document::new().with(shard, reply));
    Ok(out)
}

/// The shard's reply.
fn shard_reply<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
    options: Options,
) -> Result<Document, CommandError> {
    let kind = kind_of(c)?;
    let checked = indexes(backend, ns, c, &kind)?;
    let (report, ids) = match options.metadata {
        true => (VerifyReport::default(), Ids::new()),
        false => verify(backend, c, &kind, &checked)?,
    };
    let mut d =
        Document::new().with("valid", true).with("repaired", false).with("repairMode", "None");
    if options.background {
        let (time, increment) = teseladb_hlc::to_timestamp(backend.cluster_mark());
        d.push("readTimestamp", Value::Timestamp { time, increment });
    }
    d.push("ns", full(ns));
    d.push("uuid", Value::Binary { subtype: 4, bytes: c.uuid.to_vec() });
    d.push("fastCountType", "legacySizeStorer");
    let found = Found::of(&report, &ids, &checked);
    d.push("warnings", strings(&found.warnings));
    d.push("errors", strings(&found.errors));
    d.push("extraIndexEntries", Value::Array(found.extra));
    d.push("missingIndexEntries", Value::Array(found.missing));
    if !options.metadata {
        d.push("nInvalidDocuments", 0);
        d.push("nNonCompliantDocuments", 0);
        d.push("nrecords", count(report.records));
    }
    d.push("corruptRecords", Value::Array(Vec::new()));
    d.push("nIndexes", count(checked.len() as u64));
    let mut keys = Document::new();
    let mut details = Document::new();
    for i in sorted(&checked) {
        let held = match (options.metadata, i.held_only) {
            (true, _) => 0,
            (false, Some(n)) => n,
            (false, None) => report.indexes.get(&i.id).map_or(report.records, |r| r.entries),
        };
        keys.push(&i.name, count(held));
        let errors = found.index_errors.get(&i.name);
        let spec = if options.metadata { Document::new() } else { i.spec.clone() };
        let mut detail = Document::new()
            .with("valid", errors.is_none())
            .with("spec", spec)
            .with("isMultikey", i.multikey);
        if let Some(e) = errors {
            detail.push("errors", strings(e));
        }
        details.push(&i.name, detail);
    }
    d.push("keysPerIndex", keys);
    d.push("indexDetails", details);
    if !found.index_errors.is_empty() {
        d.insert("valid", false);
        d.push(
            "advice",
            "A corrupt namespace has been detected. See \
             http://dochub.refdb.org/core/data-recovery for recovery steps.",
        );
    }
    d.push("ok", 1.0);
    Ok(d)
}

/// The indexes in the order `validate` lists them: by name.
fn sorted(checked: &[Checked]) -> Vec<&Checked> {
    let mut out: Vec<&Checked> = checked.iter().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn count(n: u64) -> Value {
    Value::Int32(i32::try_from(n).unwrap_or(i32::MAX))
}

fn strings(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
}

/// What the verification found, as `validate` writes it.
#[derive(Default)]
struct Found {
    warnings: Vec<String>,
    errors: Vec<String>,
    extra: Vec<Value>,
    missing: Vec<Value>,
    index_errors: BTreeMap<String, Vec<String>>,
}

impl Found {
    fn of(report: &VerifyReport, ids: &Ids, checked: &[Checked]) -> Found {
        let mut f = Found::default();
        let by_id: BTreeMap<u32, &Checked> = checked.iter().map(|c| (c.id, c)).collect();
        for (id, r) in &report.indexes {
            let Some(index) = by_id.get(id) else { continue };
            for m in &r.missing {
                let key = entry_key(index, m.key.as_bytes(), &m.value);
                f.missing.push(Value::Document(
                    Document::new()
                        .with("indexName", index.name.as_str())
                        .with("indexKey", key)
                        .with("idKey", id_key(ids, &m.pk)),
                ));
            }
            for (k, v) in &r.extra {
                f.extra.push(Value::Document(
                    Document::new()
                        .with("indexName", index.name.as_str())
                        .with("indexKey", entry_key(index, k.as_bytes(), v)),
                ));
            }
            let mut errors = Vec::new();
            if !r.missing_multikey_paths.is_empty() {
                errors.push(format!(
                    "Index '{}' has one or more missing multikey metadata index keys",
                    index.name
                ));
            }
            if !r.missing.is_empty() || !r.extra.is_empty() {
                errors.push(format!("Index with name '{}' has inconsistencies.", index.name));
            }
            if !errors.is_empty() {
                f.index_errors.insert(index.name.clone(), errors);
            }
        }
        let missing: usize = report.indexes.values().map(|r| r.missing.len()).sum();
        let extra: usize = report.indexes.values().map(|r| r.extra.len()).sum();
        if missing > 0 {
            f.warnings.push(format!("Detected {missing} missing index entries."));
        }
        if extra > 0 {
            f.warnings.push(format!("Detected {extra} extra index entries."));
        }
        f.errors = f.index_errors.keys().map(|n| format!("Found errors in {n}")).collect();
        f
    }
}

/// The `_id` of each document by its primary key, as `idKey` shows it.
type Ids = BTreeMap<Vec<u8>, Value>;

/// `idKey` of the document of primary key `pk`.
fn id_key(ids: &Ids, pk: &PrimaryKey) -> Document {
    match ids.get(pk.as_bytes()) {
        Some(id) => Document::new().with("_id", id.clone()),
        None => Document::new(),
    }
}

/// The key of entry `key` with stored value `value` of `index`, each field with its value.
fn entry_key(index: &Checked, key: &[u8], value: &[u8]) -> Document {
    let pattern: Vec<(String, Direction)> = index
        .spec
        .get("key")
        .and_then(|k| match k {
            Value::Document(d) => Some(d),
            _ => None,
        })
        .map(|d| {
            d.iter()
                .map(|(k, v)| {
                    let down = crate::types::number(v).is_some_and(|n| n < 0.0);
                    (k.to_owned(), if down { Direction::Descending } else { Direction::Ascending })
                })
                .collect()
        })
        .unwrap_or_default();
    let dirs: Vec<Direction> = pattern.iter().map(|(_, d)| *d).collect();
    let unique = matches!(index.spec.get("unique"), Some(Value::Boolean(true)));
    let head = KeyBuilder::index_prefix(0, 0, index.id).as_bytes().len();
    let decoded = key.get(head..).and_then(|rest| {
        let cut = teseladb_keys::values_len(rest, &dirs).ok()?;
        let bits = match unique {
            true => teseladb_keys::space::split_unique_value(value).ok()?.1,
            false => TypeBits::from_bytes(value.to_vec()),
        };
        teseladb_keys::decode(&rest[..cut], &bits, &dirs).ok()
    });
    match decoded {
        Some(values) => pattern.iter().zip(values).fold(Document::new(), |d, ((k, _), v)| {
            d.with(k, if v == Value::Undefined { Value::Null } else { v })
        }),
        None => Document::new(),
    }
}

/// The ready indexes of `c`: `_id_` first unless the collection has none, then the others in
/// the catalog's order.
fn indexes<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
    kind: &CollectionKind,
) -> Result<Vec<Checked>, CommandError> {
    let mut out = Vec::new();
    let id_index = super::catalog::has_id_index(ns)
        && !matches!(kind, CollectionKind::Clustered { .. } | CollectionKind::TimeSeries(_));
    if id_index {
        let id = match kind {
            CollectionKind::Capped { .. } => teseladb_range::capped::ID_INDEX,
            _ => teseladb_keys::DOCUMENTS,
        };
        let spec = super::catalog::id_index(c)?;
        out.push(Checked { id, name: "_id_".to_owned(), spec, multikey: false, held_only: None });
    }
    for e in backend.indexes(c.id)? {
        if e.state != IndexState::Ready {
            continue;
        }
        let spec = Document::from(RawDoc::from_bytes(&e.spec)?);
        let multikey = has_marks(backend, c, e.id)?;
        let text = index_def(RawDoc::from_bytes(&e.spec)?, e.buckets)
            .map_err(crate::error::internal)?
            .is_none();
        let held_only = match text {
            true => Some(held(backend, c, e.id)?.len() as u64),
            false => None,
        };
        out.push(Checked { id: e.id, name: e.name, spec, multikey, held_only });
    }
    Ok(out)
}

/// Whether index `id` of `c` has multikey marks.
fn has_marks<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    id: u32,
) -> Result<bool, CommandError> {
    Ok(!marks(backend, c, id)?.is_empty())
}

/// The multikey paths recorded for index `id` of `c`.
fn marks<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    id: u32,
) -> Result<BTreeSet<String>, CommandError> {
    let start = KeyBuilder::index_prefix(c.tenant, c.id, MULTIKEY_MARKS + id);
    let end = KeyBuilder::index_prefix(c.tenant, c.id, MULTIKEY_MARKS + id + 1);
    let head = start.as_bytes().len();
    let rows = backend.scan(start.as_bytes(), end.as_bytes(), usize::MAX, ReadConcern::Local)?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, _)| {
            let rest = key.get(head..)?;
            let cut = rest.iter().position(|b| *b == 0)?;
            Some(String::from_utf8_lossy(&rest[cut + 1..]).into_owned())
        })
        .collect())
}

/// Every row of `[start, end)`, in key order.
fn rows<B: Backend + ?Sized>(
    backend: &mut B,
    start: &[u8],
    end: &[u8],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
    let mut out = Vec::new();
    let mut from = start.to_vec();
    loop {
        let chunk = backend.scan(&from, end, CHUNK, ReadConcern::Local)?;
        let short = chunk.len() < CHUNK;
        if let Some((last, _)) = chunk.last() {
            from = [last.as_slice(), &[0]].concat();
        }
        out.extend(chunk);
        if short {
            return Ok(out);
        }
    }
}

/// The entries index `id` of `c` holds.
fn held<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    id: u32,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
    let start = KeyBuilder::index_prefix(c.tenant, c.id, id);
    let end = KeyBuilder::index_prefix(c.tenant, c.id, id + 1);
    rows(backend, start.as_bytes(), end.as_bytes())
}

/// The verification of the indexes of `c` but the one its documents are.
fn verify<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    kind: &CollectionKind,
    checked: &[Checked],
) -> Result<(VerifyReport, Ids), CommandError> {
    let (start, end) = documents_of(c);
    // Each document is checked as its BSON: a table of offsets that does not hold leaves the
    // value as it is, and the BSON check refuses it.
    let docs: Vec<_> = rows(backend, &start, &end)?
        .into_iter()
        .map(|(key, doc)| (key, teseladb_keys::stored::strip(doc)))
        .collect();
    let capped = matches!(kind, CollectionKind::Capped { .. });
    let pks: Vec<PrimaryKey> =
        docs.iter().map(|(key, doc)| primary_key(c, capped, key, doc)).collect::<Result<_, _>>()?;
    let mut report = VerifyReport { records: docs.len() as u64, ..VerifyReport::default() };
    let mut ids = Ids::new();
    for ((_, doc), pk) in docs.iter().zip(&pks) {
        if let Some(id) = RawDoc::from_bytes(doc)?.get("_id") {
            ids.insert(pk.as_bytes().to_vec(), Value::from(id));
        }
    }
    for index in checked.iter().filter(|i| i.held_only.is_none()) {
        if index.id == teseladb_keys::DOCUMENTS {
            continue;
        }
        // The `_id_` index of a capped collection is written unique, which its shown
        // specification does not say: its entries are checked as they are written.
        let spec_bytes = match index.id == teseladb_range::capped::ID_INDEX {
            true => super::capped::id_index_rule(c).spec.to_bytes()?,
            false => index.spec.to_bytes()?,
        };
        let spec = RawDoc::from_bytes(&spec_bytes)?;
        let buckets =
            backend.indexes(c.id)?.into_iter().find(|e| e.id == index.id).map_or(0, |e| e.buckets);
        let Some(def) = index_def(spec, buckets).map_err(crate::error::internal)? else {
            continue;
        };
        let wildcard = index.spec.get("key").is_some_and(|k| match k {
            Value::Document(d) => d.iter().any(|(f, _)| f.ends_with("$**")),
            _ => false,
        });
        let mut paths = BTreeMap::new();
        if wildcard {
            paths.insert(index.id, marks(backend, c, index.id)?);
        }
        let collation = collation_of(spec)?;
        let collator = collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator);
        let set = IndexSet { tenant: c.tenant, collection: c.id, indexes: vec![(index.id, def)] };
        let versions = docs
            .iter()
            .zip(&pks)
            .map(|((_, doc), pk)| Ok(Version { doc: RawDoc::from_bytes(doc)?, pk }))
            .collect::<Result<Vec<_>, CommandError>>()?;
        let entries = held(backend, c, index.id)?
            .into_iter()
            .map(|(k, v)| (Key::from_stored(k), v))
            .collect::<Vec<_>>();
        let one = verify_range(&set, versions, entries, &paths, collator).map_err(|e| {
            let doc = docs.first().and_then(|(_, d)| RawDoc::from_bytes(d).ok());
            match doc {
                Some(d) => keygen_error(&e, d),
                None => CommandError::new(INTERNAL_ERROR, "an index refuses a document"),
            }
        })?;
        report.indexes.extend(one.indexes);
    }
    Ok((report, ids))
}

/// The primary key of the stored document `doc` of key `key`: its `_id`, or its insertion
/// number in a capped collection.
fn primary_key(
    c: &CollectionEntry,
    capped: bool,
    key: &[u8],
    doc: &[u8],
) -> Result<PrimaryKey, CommandError> {
    if capped {
        return teseladb_range::capped::seq_of(c.tenant, c.id, key)
            .map(PrimaryKey::seq)
            .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "not a capped document key"));
    }
    let doc = RawDoc::from_bytes(doc)?;
    Ok(PrimaryKey::id(doc.get("_id").unwrap_or(teseladb_bson::RawValue::Null)))
}
