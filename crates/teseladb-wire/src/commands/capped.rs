//! The command side of capped collections (015 FR-003, FR-005, T007, T009): a capped
//! collection's writes go one document at a time with the collection's state and the oldest
//! documents scanned before them ([`write`]); its `_id` is kept unique by an `_id_` index of its
//! own ([`id_index_rule`], [`id_key`]), because its documents are keyed by insertion order;
//! `convertToCapped` copies a collection into a new capped one that takes its name
//! ([`convert_to_capped`]). `cloneCollectionAsCapped` is not a command of a `refrouter` 9.0.4
//! (measured: 59), and TeselaDB answers it the same.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::spec::index_def;
use teseladb_keys::PrimaryKey;
use teseladb_keys::space::split_unique_value;
use teseladb_meta::catalog::CollectionEntry;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_range::capped::{self as cap, CappedError, CappedLimits, ID_INDEX};
use teseladb_range::read::ReadConcern;

use super::catalog::write_adding_database;
use super::crud::{Writer, index_rules, rules_of};
use super::index_maintenance::IndexRule;
use super::mutation::{Bound, CappedRule, DocMutation, MutationResult, capped::refused};
use super::names::{self, full};
use super::query::target;
use super::query::{documents_of, options_collation};
use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, CLUSTERED_CAPPED_TEST, CommandError, ILLEGAL_OPERATION, INTERNAL_ERROR,
    INVALID_OPTIONS, NAMESPACE_NOT_FOUND, OPERATION_NOT_SUPPORTED_IN_TRANSACTION, WRITE_CONFLICT,
};
use teseladb_meta::catalog::{CatalogCommand, Namespace};

/// `convertToCapped`.
pub const CONVERT_TO_CAPPED: &str = "convertToCapped";

/// How many of the oldest documents a write scans the first time it needs them.
const FIRST_SCAN: usize = 16;
/// How many times a write scans the oldest documents again before it gives up.
const MAX_ROUNDS: usize = 64;

/// The capped rule of `coll`'s writes, `None` when it is not capped.
pub fn rule_of(coll: &CollectionEntry) -> Result<Option<CappedRule>, CommandError> {
    Ok(match super::kinds::kind_of(coll)? {
        CollectionKind::Capped { size, max } => {
            Some(CappedRule { limits: CappedLimits { size, max }, oldest: 0 })
        }
        CollectionKind::Normal
        | CollectionKind::View { .. }
        | CollectionKind::TimeSeries(_)
        | CollectionKind::Clustered { .. } => None,
    })
}

/// The `_id_` index of capped collection `coll`, unique and with the collection's collation, as
/// RefDb builds it.
pub fn id_index_rule(coll: &CollectionEntry) -> IndexRule {
    let mut spec = Document::new()
        .with("v", 2)
        .with("key", Document::new().with("_id", 1))
        .with("name", "_id_")
        .with("unique", true);
    if let Some(c) = options_collation(coll) {
        spec.push("collation", c);
    }
    IndexRule { id: ID_INDEX, spec, buckets: 0, delete_only: false }
}

/// The key of the document of capped collection `coll` (or of one with a shard key) whose `_id`
/// is `id`, read from its `_id_` index; `None` when there is none.
pub fn id_key<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    id: &Value,
) -> Result<Option<Vec<u8>>, CommandError> {
    Ok(id_keys(backend, coll, std::slice::from_ref(id))?.into_iter().next().flatten())
}

/// [`id_key`] of each of `ids`, read together.
pub fn id_keys<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    ids: &[Value],
) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
    let rule = id_index_rule(coll);
    let spec_bytes = rule.spec.to_bytes()?;
    let def = index_def(RawDoc::from_bytes(&spec_bytes)?, 0)
        .ok()
        .flatten()
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the _id_ index has no definition"))?;
    let set = IndexSet { tenant: coll.tenant, collection: coll.id, indexes: vec![(ID_INDEX, def)] };
    let collation = super::index_maintenance::collation_of(RawDoc::from_bytes(&spec_bytes)?)?;
    let collator = collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator);
    let pk = PrimaryKey::seq(0);
    // The `_id_` entry of each id, where it has one.
    let mut entries: Vec<Option<Vec<u8>>> = Vec::with_capacity(ids.len());
    for id in ids {
        let holder = Document::new().with("_id", id.clone()).to_bytes()?;
        let doc = RawDoc::from_bytes(&holder)?;
        let keys = set
            .keys(Version { doc, pk: &pk }, collator)
            .map_err(|e| super::index_maintenance::keygen_error(&e, doc))?;
        entries.push(keys.into_keys().next().map(|k| k.as_bytes().to_vec()));
    }
    let wanted: Vec<Vec<u8>> = entries.iter().flatten().cloned().collect();
    let mut found = match wanted.is_empty() {
        true => Vec::new(),
        false => backend.read_versions(&wanted)?.1,
    }
    .into_iter();
    let mut out = Vec::with_capacity(ids.len());
    for entry in &entries {
        let value = match entry {
            Some(_) => found.next().flatten(),
            None => None,
        };
        out.push(value.map(|(_, v)| doc_key_of_entry(coll, &v)).transpose()?);
    }
    Ok(out)
}

/// The key of the document an `_id_` entry of `coll` holds `value` for.
fn doc_key_of_entry(coll: &CollectionEntry, value: &[u8]) -> Result<Vec<u8>, CommandError> {
    let (pk, _) =
        split_unique_value(value).map_err(|e| CommandError::new(INTERNAL_ERROR, e.to_string()))?;
    // A collection with a shard key: the entry ends in its document's key (018 FR-001).
    if super::sharding::is_sharded(coll) {
        let key = teseladb_keys::KeyBuilder::document_of_sharded_pk(coll.tenant, coll.id, pk);
        return Ok(key.as_bytes().to_vec());
    }
    let seq = <[u8; 8]>::try_from(pk)
        .map_err(|_| CommandError::new(INTERNAL_ERROR, "a capped _id_ entry without its seq"))?;
    Ok(cap::doc_key(coll.tenant, coll.id, u64::from_be_bytes(seq)))
}

/// The error of capped write `bound` in a transaction, as a `refrouter` 9.0.4's shard answers it
/// (measured): an insert or an update is `OperationNotSupportedInTransaction`, a deletion
/// `IllegalOperation`.
pub fn in_transaction(bound: &Bound) -> CommandError {
    let ns = &bound.rules().ns;
    match bound.mutation {
        DocMutation::Delete { .. } => CommandError::new(
            ILLEGAL_OPERATION,
            format!("Cannot remove from a capped collection in a multi-document transaction: {ns}"),
        ),
        DocMutation::Insert { .. }
        | DocMutation::Update { .. }
        | DocMutation::Backfill { .. }
        | DocMutation::Erase { .. } => CommandError::new(
            OPERATION_NOT_SUPPORTED_IN_TRANSACTION,
            format!(
                "Collection '{ns}' is a capped collection. Writes in transactions are not allowed on capped collections."
            ),
        ),
    }
}

/// The keys of the `limit` oldest documents of capped collection `coll`.
fn oldest_keys<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    limit: usize,
) -> Result<Vec<Vec<u8>>, CommandError> {
    let (start, end) = documents_of(coll);
    Ok(backend.scan(&start, &end, limit, ReadConcern::Local)?.into_iter().map(|(k, _)| k).collect())
}

/// `bound` laid out for its range: the collection's state, the `oldest` keys and its own keys.
fn laid_out(bound: &Bound, collection: u32, oldest: Vec<Vec<u8>>) -> Result<Bound, CommandError> {
    let mut mutation = bound.mutation.clone();
    let rules = match &mut mutation {
        DocMutation::Insert { rules, .. }
        | DocMutation::Update { rules, .. }
        | DocMutation::Delete { rules, .. } => rules,
        DocMutation::Backfill { .. } | DocMutation::Erase { .. } => {
            return Err(CommandError::new(INTERNAL_ERROR, "not a capped write"));
        }
    };
    let tenant = rules.tenant;
    let rule = rules.capped.as_mut().ok_or_else(|| {
        CommandError::new(INTERNAL_ERROR, "a capped write of a collection that is not capped")
    })?;
    rule.oldest = oldest.len();
    let keys = std::iter::once(cap::state_key(tenant, collection))
        .chain(oldest)
        .chain(bound.keys.iter().cloned())
        .collect();
    Ok(Bound { mutation, keys })
}

/// Runs `bound`, a write of capped collection `coll` with only its own keys: it goes first
/// without the oldest documents, and scans them (more each time) when the range asks for them.
pub fn write<B: Backend + ?Sized>(
    backend: &mut B,
    w: &mut Writer,
    coll: &CollectionEntry,
    bound: &Bound,
    stmt: u32,
) -> Result<MutationResult, CommandError> {
    let mut want = 0usize;
    for _ in 0..MAX_ROUNDS {
        let oldest = if want == 0 { Vec::new() } else { oldest_keys(backend, coll, want)? };
        let b = laid_out(bound, coll.id, oldest)?;
        match w.write(backend, &b, stmt) {
            Err(e) => match refused(&e) {
                Some(CappedError::NeedOlder) => want = want.saturating_mul(2).max(FIRST_SCAN),
                Some(CappedError::Stale | CappedError::BadSize) => want = want.max(FIRST_SCAN),
                None => return Err(e),
            },
            written => return written,
        }
    }
    Err(CommandError::new(WRITE_CONFLICT, "the capped collection kept changing during the write"))
}

/// Deletes what a dropped capped collection keeps besides its documents and catalog indexes: its
/// `_id_` entries and its state.
pub fn purge<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
) -> Result<(), CommandError> {
    if rule_of(c)?.is_none() {
        return Ok(());
    }
    let start =
        teseladb_keys::KeyBuilder::index_prefix(c.tenant, c.id, ID_INDEX).as_bytes().to_vec();
    let end = teseladb_keys::KeyBuilder::collection_end(c.tenant, c.id).as_bytes().to_vec();
    let mut w = Writer::internal();
    let rules = super::mutation::Rules {
        ns: full(ns),
        collection: c.id,
        tenant: c.tenant,
        unlogged: true,
        ..Default::default()
    };
    loop {
        let rows =
            backend.scan(&start, &end, super::crud::MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
        if rows.is_empty() {
            return Ok(());
        }
        let keys = rows.into_iter().map(|(k, _)| k).collect();
        let mutation = DocMutation::Erase { rules: rules.clone() };
        w.write(backend, &Bound { mutation, keys }, 0)?;
    }
}

/// The `size` of `convertToCapped`, as a `refrouter` 9.0.4 reads it (measured): a number, truncated,
/// from 1 to 1 PiB.
fn convert_size(v: Option<&Value>) -> Result<u64, CommandError> {
    let n = match v {
        Some(Value::Int32(n)) => *n as f64,
        Some(Value::Int64(n)) => *n as f64,
        Some(Value::Double(n)) => n.trunc(),
        _ => 0.0,
    };
    if n.is_nan() || n < 1.0 {
        let msg = "Capped collection size must be greater than zero";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    if n > cap::MAX_CAPPED_SIZE as f64 {
        return Err(CommandError::new(BAD_VALUE, "size cannot exceed 1 PB"));
    }
    Ok(n as u64)
}

/// The name of the collection `convertToCapped` fills before it takes the original's name:
/// `tmp` and five random letters or digits, as RefDb names it.
fn temporary<B: Backend + ?Sized>(backend: &mut B, ns: &Namespace) -> Namespace {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut r = backend.random();
    let mut tag = String::with_capacity(5);
    for _ in 0..5 {
        tag.push(char::from(ALPHABET[(r % ALPHABET.len() as u64) as usize]));
        r /= ALPHABET.len() as u64;
    }
    Namespace { db: ns.db.clone(), coll: format!("tmp{tag}.convertToCapped.{}", ns.coll) }
}

/// The options of the capped copy: `capped` and `size` first, then the original's options
/// (validator, collation...) except a former `max`, which `convertToCapped` drops (measured).
fn converted_options(coll: &CollectionEntry, size: u64) -> Result<Vec<u8>, CommandError> {
    let original = Document::from(RawDoc::from_bytes(&coll.options)?);
    let size =
        i64::try_from(size).map_err(|_| CommandError::new(BAD_VALUE, "size cannot exceed 1 PB"))?;
    let mut options = Document::new().with("capped", true).with("size", size);
    for (k, v) in original.iter() {
        if !["capped", "size", "max"].contains(&k) {
            options.push(k, v.clone());
        }
    }
    Ok(options.to_bytes()?)
}

/// `convertToCapped`: the collection's documents, in its key order, go into a new capped
/// collection (the oldest evicted by `size`), which then takes the collection's name; the
/// secondary indexes do not follow and the collection gets a new UUID (measured on a `refrouter`
/// 9.0.4).
pub fn convert_to_capped<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let ns = target(ctx)?;
    let size = convert_size(ctx.cmd.get("size"))?;
    let Some(coll) = ctx.backend.collection(&ns)? else {
        let msg = format!("source collection {} does not exist", full(&ns));
        return Err(CommandError::new(NAMESPACE_NOT_FOUND, msg));
    };
    super::kinds::not_a_view(&ns, &coll)?;
    let tmp = temporary(ctx.backend, &ns);
    match super::kinds::kind_of(&coll)? {
        CollectionKind::TimeSeries(_) => {
            let msg = "Cannot convert a timeseries collection to a capped collection";
            return Err(CommandError::new(ILLEGAL_OPERATION, msg));
        }
        // A capped and clustered collection is a test feature of RefDb (DV-057).
        CollectionKind::Clustered { .. } => {
            let msg = "Clustered capped collection only available with 'enableTestCommands' server parameter";
            return Err(CommandError::new(CLUSTERED_CAPPED_TEST, msg));
        }
        CollectionKind::Normal | CollectionKind::Capped { .. } | CollectionKind::View { .. } => {}
    }
    if names::check_writable(&ns).is_err() {
        let msg = format!("Invalid namespace: {}", full(&tmp));
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    let uuid = super::new_uuid(ctx.backend);
    let options = converted_options(&coll, size)?;
    let create = CatalogCommand::Create { ns: tmp.clone(), uuid, options, if_absent: false };
    let copy = write_adding_database(ctx.backend, &tmp.db, create)?
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the capped copy was not created"))?;
    let rules = rules_of(&tmp, &copy, true, None, index_rules(ctx.backend, &copy)?)?;
    let mut w = Writer::internal();
    let (mut next, end) = documents_of(&coll);
    loop {
        let rows = ctx.backend.scan(&next, &end, super::query::SCAN_CHUNK, ReadConcern::Local)?;
        let short = rows.len() < super::query::SCAN_CHUNK;
        for (key, bytes) in rows {
            let doc = Document::from(RawDoc::from_bytes(&bytes)?);
            let mutation =
                DocMutation::Insert { docs: vec![doc], ordered: true, rules: rules.clone() };
            let r = write(ctx.backend, &mut w, &copy, &Bound { mutation, keys: Vec::new() }, 0)?;
            if let Some(e) = r.errors.into_iter().next() {
                return Err(e.error);
            }
            next = [key.as_slice(), &[0]].concat();
        }
        if short {
            break;
        }
    }
    let rename =
        CatalogCommand::Rename { from: tmp, to: ns.clone(), drop_target: true, uuid: [0; 16] };
    write_adding_database(ctx.backend, &ns.db, rename)?;
    super::catalog::purge(ctx.backend, &ns, &coll)?;
    Ok(Document::new().with("ok", 1.0))
}
