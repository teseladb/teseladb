//! The writes of the CRUD commands as range mutations (FR-014, FR-007, FR-022): the command builds
//! a [`DocMutation`] over the keys of the documents it may touch, the range's leader decodes it
//! and evaluates it with `teseladb-mql` against the current versions of those keys at the
//! write's mark. Everything the leader needs travels in the mutation (filter, update, validator,
//! collation), encoded as one BSON document, so a mutation crosses nodes as bytes.
//!
//! The result the driver gets back is a BSON document too (see [`MutationResult`]): it is what
//! the range keeps in the idempotence record of a retryable write (005 FR-011).

use teseladb_bson::{BsonError, Document, Limits, MAX_DEPTH, MAX_SIZE, RawDoc, RawValue, Value};
use teseladb_changestream::describe::{Touch, describe};
use teseladb_changestream::meta::RetryInfo;
use teseladb_changestream::pipeline_diff::{describe_diff, pipeline_diff};
use teseladb_changestream::update_desc::UpdateDescription;
use teseladb_collation::{ParsedCollation, parse_collation};
use teseladb_hlc::{Hlc, to_timestamp};
use teseladb_mql::update::{Now, PathChange, Update, UpdateContext, UpdateOutcome};
use teseladb_mql::validation::{ValidationAction, ValidationLevel, Validator, Verdict};
use teseladb_mql::{Filter, FilterUse, MatchContext, MqlError};
use teseladb_range::command::Put;
use teseladb_range::host::{Mutation, MutationError};
use teseladb_range::mutate::Outcome;

use super::index_maintenance::{self, IndexRule, Maintainer};
use teseladb_range::capped::CappedLimits;

pub(crate) mod capped;
#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests;
pub(crate) mod timeseries;
use crate::error::{BSON_OBJECT_TOO_LARGE, CommandError, DUPLICATE_KEY, INTERNAL_ERROR, OVERFLOW};
use crate::msg::command_limits;
use teseladb_index::unique::UniqueGuard;

/// Limits of an encoded mutation or result: what fits in a range entry (005 FR-023), with room
/// for the levels the mutation adds around a filter or update at a command's depth.
pub const MUTATION_LIMITS: Limits =
    Limits { max_size: 64 << 20, max_depth: crate::msg::MAX_COMMAND_DEPTH + 4 };

/// Value type of a stored document.
pub const DOCUMENT: u8 = teseladb_changestream::DOCUMENT_VALUE;

/// Field names of the encoded mutation and of its result, each declared once.
mod f {
    pub const KIND: &str = "k";
    pub const NS: &str = "ns";
    pub const DOCS: &str = "docs";
    pub const ORDERED: &str = "ordered";
    pub const FILTER: &str = "filter";
    pub const UPDATE: &str = "u";
    pub const ARRAY_FILTERS: &str = "arrayFilters";
    pub const MULTI: &str = "multi";
    pub const UPSERT: &str = "upsert";
    pub const IMAGE: &str = "image";
    pub const VALIDATOR: &str = "validator";
    pub const LEVEL: &str = "validationLevel";
    pub const ACTION: &str = "validationAction";
    pub const BYPASS: &str = "bypass";
    pub const COLLATION: &str = "collation";
    pub const COLLECTION_ID: &str = "cid";
    pub const TENANT: &str = "tenant";
    pub const INDEXES: &str = "indexes";
    pub const VARS: &str = "let";
    pub const ATTEMPT: &str = "attempt";
    pub const UNLOGGED: &str = "unlogged";
    pub const SHARD_KEY: &str = "shardKey";
    pub const MOVES: &str = "moves";
    pub const REKEYS: &str = "rekeys";
    pub const CLUSTERED: &str = "clustered";
    pub const RETRY: &str = "retry";
    pub const MARK: &str = "mark";
    pub const PROBES: &str = "probes";
    pub const CAPPED: &str = "capped";
    pub const CAPPED_SIZE: &str = "size";
    pub const CAPPED_MAX: &str = "max";
    pub const CAPPED_OLDEST: &str = "oldest";
    pub const TIMESERIES: &str = "ts";
    pub const TS_TIME: &str = "time";
    pub const TS_META: &str = "meta";
    pub const TS_SPAN: &str = "span";
    pub const TS_ROUNDING: &str = "rounding";
    pub const TS_BUCKET: &str = "bucket";
    pub const N: &str = "n";
    pub const N_MODIFIED: &str = "nModified";
    pub const UPSERTED: &str = "upserted";
    pub const VALUE: &str = "value";
    pub const ERRORS: &str = "errors";
    pub const INDEX: &str = "index";
    pub const CODE: &str = "code";
    pub const ERRMSG: &str = "errmsg";
    pub const ERR_INFO: &str = "errInfo";
    pub const WARNINGS: &str = "warnings";
    pub const KEYS_INSERTED: &str = "keysInserted";
    pub const KEYS_DELETED: &str = "keysDeleted";
    /// The identifier of a log line of validation (`id` in RefDb's log).
    pub const LOG_ID: &str = "id";
    /// The document of a log line of validation.
    pub const LOG_DOCUMENT: &str = "document";
}

/// Kinds of mutation.
const INSERT: i32 = 0;
const UPDATE: i32 = 1;
const DELETE: i32 = 2;
const BACKFILL: i32 = 3;
const ERASE: i32 = 4;

/// Which document a `findAndModify` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Image {
    /// None (`update`, `delete`).
    None,
    /// The version before the change.
    Before,
    /// The version after it.
    After,
}

impl Image {
    fn code(self) -> i32 {
        match self {
            Image::None => 0,
            Image::Before => 1,
            Image::After => 2,
        }
    }

    fn from_code(c: i32) -> Image {
        match c {
            1 => Image::Before,
            2 => Image::After,
            _ => Image::None,
        }
    }
}

/// The collection's rules every write checks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    /// Namespace `db.coll`, for the messages.
    pub ns: String,
    /// Validator with its level and action (names as RefDb writes them).
    pub validator: Option<(Document, String, String)>,
    /// `bypassDocumentValidation`.
    pub bypass: bool,
    /// Collation of the command (or the collection's), as its document.
    pub collation: Option<Document>,
    /// Number of the collection, for the keys of its index entries.
    pub collection: u32,
    /// Its tenant, which its keys carry (007 FR-025).
    pub tenant: u32,
    /// The secondary indexes being built or ready, which every write keeps (009 FR-003).
    pub indexes: Vec<IndexRule>,
    /// The command's `let` variables, which the leader's filter and update read (FR-032).
    pub vars: Option<Document>,
    /// The number the writer drew for this send of a retryable write; the result carries it, so
    /// a result kept from an earlier send shows the statement was retried (`retriedStmtIds`).
    pub attempt: i64,
    /// What the command read of the values the write adds to an index with `prepareUnique`
    /// (009 FR-004).
    pub probes: Vec<teseladb_index::prepared::Probe>,
    /// The collection is capped (015 FR-003): the write's first key is the collection's state and
    /// the next ones its oldest documents.
    pub capped: Option<CappedRule>,
    /// The collection is a time series (015 FR-006): an insert goes into bucket `bucket`.
    pub timeseries: Option<TimeSeriesRule>,
    /// A write no change stream sees (014): the purge of a dropped collection, whose drop is the
    /// event. It keeps no history next to the documents.
    pub unlogged: bool,
    /// The collection's shard key (018 FR-001): its documents are keyed by it and their `_id`,
    /// which their index entries end in.
    pub shard_key: Option<Document>,
    /// A retryable write or a statement of a transaction: an update that changes the shard key
    /// moves its document to the key of the new values (018 FR-001), as a `refrouter` 9.0.4 moves
    /// it there and refuses it elsewhere (measured on 02-11-2026).
    pub moves: bool,
    /// A move of `shardCollection` keying the documents again by the new shard key (018 FR-001):
    /// each write keeps an internal meta ([`teseladb_changestream::meta::Moved::Internal`]), so
    /// the search indexes follow the documents' new keys and no change stream or oplog shows it.
    pub rekeys: bool,
    /// The retryable write outside a transaction that sends it (018 FR-009): its documents' metas
    /// keep the session for their oplog entries.
    pub retry: Option<teseladb_changestream::meta::RetryInfo>,
    /// The collection is clustered by `_id` (015 FR-009): a duplicate `_id` fails as there.
    pub clustered: bool,
}

/// The buckets of a time series and the one an insert goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeSeriesRule {
    /// How the collection lays out its buckets.
    pub spec: teseladb_mql::timeseries_write::BucketSpec,
    /// The `_id` of the bucket of the write.
    pub bucket: [u8; 12],
}

/// How a write of a capped collection lays out its keys: the state key, then `oldest` keys of
/// the collection's oldest documents in insertion order, then those of the write itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CappedRule {
    /// The collection's limits.
    pub limits: CappedLimits,
    /// How many of the oldest documents follow the state key.
    pub oldest: usize,
}

/// What the leader does to the documents of a write.
#[derive(Clone, Debug, PartialEq)]
pub enum DocMutation {
    /// Inserts `docs` (one key per document, in order); `ordered` stops at the first error.
    Insert {
        /// The documents, `_id` first.
        docs: Vec<Document>,
        /// Ordered insert.
        ordered: bool,
        /// Validator and collation.
        rules: Rules,
    },
    /// Updates the documents of its keys that still match `filter` (one unless `multi`); with
    /// `upsert` and no match, inserts that document at the last key.
    Update {
        /// Filter, checked again on the leader.
        filter: Document,
        /// The update (operators, replacement or pipeline).
        update: Value,
        /// `arrayFilters`.
        array_filters: Option<Vec<Value>>,
        /// Every matching document.
        multi: bool,
        /// The document to insert when nothing matches.
        upsert: Option<Document>,
        /// The version a `findAndModify` returns.
        image: Image,
        /// Validator and collation.
        rules: Rules,
    },
    /// Deletes the documents of its keys that still match `filter` (one unless `multi`).
    Delete {
        /// Filter.
        filter: Document,
        /// Every matching document.
        multi: bool,
        /// The version a `findAndModify` returns.
        image: Image,
        /// Collation.
        rules: Rules,
    },
    /// Writes the entries of the indexes of `rules` (those being built) for the current version
    /// of each document of its keys (009 FR-006): an index build's backfill. A document deleted
    /// meanwhile gets nothing, and one written meanwhile already has them.
    Backfill {
        /// The collection and the indexes being built.
        rules: Rules,
    },
    /// Deletes every key of the write that holds something: the entries of an index being
    /// dropped (009).
    Erase {
        /// The collection, for the messages.
        rules: Rules,
    },
}

/// One failed statement of a write.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteError {
    /// Index of the statement in the mutation.
    pub index: usize,
    /// The error.
    pub error: CommandError,
}

/// What a mutation gives back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MutationResult {
    /// Documents inserted, matched or deleted.
    pub n: i64,
    /// Documents changed by an update.
    pub n_modified: i64,
    /// `_id` of the document an upsert inserted.
    pub upserted: Option<Value>,
    /// The version a `findAndModify` returns.
    pub value: Option<Document>,
    /// Failed statements.
    pub errors: Vec<WriteError>,
    /// `errInfo` of documents a `warn` validator let through, for the log.
    pub warnings: Vec<Document>,
    /// The attempt of the send that evaluated the write ([`Rules::attempt`]).
    pub attempt: i64,
    /// Set by the writer, not encoded: the range answered with the result of an earlier send.
    pub retried: bool,
    /// The mark of a retryable write that wrote (the next statement's `prevOpTime`, 018 FR-009).
    pub mark: Option<Hlc>,
    /// Index keys the write added and removed, the `_id` of each document it inserted or
    /// deleted counted as one (the profiler's `keysInserted` and `keysDeleted`).
    pub keys: (i64, i64),
}

fn rules_doc(r: &Rules, d: &mut Document, send: Send<'_>) {
    let Send { attempt, retry } = send;
    d.push(f::NS, r.ns.as_str());
    if let Some((spec, level, action)) = &r.validator {
        d.push(f::VALIDATOR, spec.clone());
        d.push(f::LEVEL, level.as_str());
        d.push(f::ACTION, action.as_str());
    }
    d.push(f::BYPASS, r.bypass);
    if let Some(c) = &r.collation {
        d.push(f::COLLATION, c.clone());
    }
    d.push(f::COLLECTION_ID, i64::from(r.collection));
    if r.tenant != 0 {
        d.push(f::TENANT, i64::from(r.tenant));
    }
    if !r.indexes.is_empty() {
        d.push(f::INDEXES, index_maintenance::rules_value(&r.indexes));
    }
    if let Some(v) = &r.vars {
        d.push(f::VARS, v.clone());
    }
    let attempt = if attempt != 0 { attempt } else { r.attempt };
    if attempt != 0 {
        d.push(f::ATTEMPT, attempt);
    }
    if !r.probes.is_empty() {
        d.push(f::PROBES, index_maintenance::probes_value(&r.probes));
    }
    if r.unlogged {
        d.push(f::UNLOGGED, true);
    }
    if let Some(k) = &r.shard_key {
        d.push(f::SHARD_KEY, k.clone());
    }
    if r.moves {
        d.push(f::MOVES, true);
    }
    if r.rekeys {
        d.push(f::REKEYS, true);
    }
    if r.clustered {
        d.push(f::CLUSTERED, true);
    }
    if let Some(info) = retry.or(r.retry.as_ref()) {
        d.push(f::RETRY, Value::Binary { subtype: 0, bytes: info.to_bytes() });
    }
    if let Some(c) = &r.capped {
        let saturated = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        let mut cd = Document::new().with(f::CAPPED_SIZE, saturated(c.limits.size));
        if let Some(max) = c.limits.max {
            cd.push(f::CAPPED_MAX, saturated(max));
        }
        cd.push(f::CAPPED_OLDEST, saturated(c.oldest as u64));
        d.push(f::CAPPED, cd);
    }
    if let Some(t) = &r.timeseries {
        let mut td = Document::new().with(f::TS_TIME, t.spec.time_field.as_str());
        if let Some(m) = &t.spec.meta_field {
            td.push(f::TS_META, m.as_str());
        }
        td.push(f::TS_SPAN, t.spec.max_span_seconds);
        td.push(f::TS_ROUNDING, t.spec.rounding_seconds);
        td.push(f::TS_BUCKET, Value::ObjectId(t.bucket));
        d.push(f::TIMESERIES, td);
    }
}

/// Where an update of `old` into `new` moves its document when it changes the values of shard
/// key `key` (`None` when it keeps them): the document's key is its shard key and its `_id`. As a
/// `refrouter` 9.0.4 (measured on 02-11-2026), a multi-update cannot change them, and a single one
/// only in a transaction or a retryable write (`home`, the tenant and the collection).
fn moved_to(
    key: &Document,
    (old, new): (RawDoc<'_>, &Document),
    multi: bool,
    home: Option<(u32, u32)>,
) -> Result<Option<Vec<u8>>, CommandError> {
    let before = super::sharding::shard_values(key, &Document::from(old))?;
    if before == super::sharding::shard_values(key, new)? {
        return Ok(None);
    }
    if multi {
        let msg = "Multi-update operations are not allowed when updating the shard key field.";
        return Err(CommandError::new(crate::error::INVALID_OPTIONS, msg));
    }
    let Some((tenant, collection)) = home else {
        let msg = "Must run update to shard key field in a multi-statement transaction or with retryWrites: true.";
        return Err(CommandError::new(crate::error::ILLEGAL_OPERATION, msg));
    };
    super::sharding::document_key_in((tenant, collection), key, new).map(Some)
}

/// The index entries and deletions of a document that moves from key `from` to key `to` with
/// version `new`: it leaves the old key and its entries, and takes the new ones; an entry it
/// keeps (that of `_id_`, with the new key in its value) is written again, not deleted.
fn moved_entries(
    checker: &Checker,
    (from, to): (&[u8], &[u8]),
    (old, new): (RawDoc<'_>, &[u8]),
    lookup: &Lookup<'_>,
) -> Result<(Vec<Put>, Vec<Vec<u8>>), CommandError> {
    let (mut puts, mut deletes) = checker.entries_at(Some(from), Some(old), None, lookup)?;
    let (added, gone) = checker.entries_at(Some(to), None, Some(new), lookup)?;
    puts.extend(added);
    deletes.extend(gone);
    deletes.push(from.to_vec());
    deletes.retain(|k| k.as_slice() != to && !puts.iter().any(|p| &p.key == k));
    checker.moved.borrow_mut().push((from.to_vec(), to.to_vec()));
    Ok((puts, deletes))
}

/// Bytes of a document's key before its primary key in a collection with a shard key: space,
/// tenant, collection and index.
const DOC_PREFIX: usize = 13;

/// The time series rule a mutation carries.
fn read_timeseries(d: RawDoc<'_>) -> Result<Option<TimeSeriesRule>, CommandError> {
    let Some(v) = d.get(f::TIMESERIES) else { return Ok(None) };
    let RawValue::Document(t) = v else { return Err(bad_rules()) };
    let int = |k: &str| match t.get(k) {
        Some(RawValue::Int64(n)) => Ok(n),
        _ => Err(bad_rules()),
    };
    let (Some(RawValue::String(time)), Some(RawValue::ObjectId(bucket))) =
        (t.get(f::TS_TIME), t.get(f::TS_BUCKET))
    else {
        return Err(bad_rules());
    };
    let meta_field = match t.get(f::TS_META) {
        Some(RawValue::String(m)) => Some(m.to_owned()),
        None => None,
        Some(_) => return Err(bad_rules()),
    };
    let spec = teseladb_mql::timeseries_write::BucketSpec {
        time_field: time.to_owned(),
        meta_field,
        max_span_seconds: int(f::TS_SPAN)?,
        rounding_seconds: int(f::TS_ROUNDING)?,
    };
    Ok(Some(TimeSeriesRule { spec, bucket }))
}

/// The capped rule a mutation carries.
fn read_capped(d: RawDoc<'_>) -> Result<Option<CappedRule>, CommandError> {
    let Some(v) = d.get(f::CAPPED) else { return Ok(None) };
    let RawValue::Document(c) = v else { return Err(bad_rules()) };
    let int = |k: &str| match c.get(k) {
        Some(RawValue::Int64(n)) => Ok(Some(n)),
        None => Ok(None),
        Some(_) => Err(bad_rules()),
    };
    let size = int(f::CAPPED_SIZE)?.ok_or_else(bad_rules)?;
    let limits =
        CappedLimits::new(size, int(f::CAPPED_MAX)?.unwrap_or(0)).map_err(|_| bad_rules())?;
    let oldest =
        usize::try_from(int(f::CAPPED_OLDEST)?.ok_or_else(bad_rules)?).map_err(|_| bad_rules())?;
    Ok(Some(CappedRule { limits, oldest }))
}

fn get_doc(d: RawDoc<'_>, k: &str) -> Option<Document> {
    match d.get(k) {
        Some(RawValue::Document(x)) => Some(Document::from(x)),
        _ => None,
    }
}

fn get_str(d: RawDoc<'_>, k: &str) -> String {
    match d.get(k) {
        Some(RawValue::String(s)) => s.to_owned(),
        _ => String::new(),
    }
}

fn get_bool(d: RawDoc<'_>, k: &str) -> bool {
    matches!(d.get(k), Some(RawValue::Boolean(true)))
}

fn get_i32(d: RawDoc<'_>, k: &str) -> i32 {
    match d.get(k) {
        Some(RawValue::Int32(n)) => n,
        _ => 0,
    }
}

fn read_rules(d: RawDoc<'_>) -> Result<Rules, CommandError> {
    let validator =
        get_doc(d, f::VALIDATOR).map(|v| (v, get_str(d, f::LEVEL), get_str(d, f::ACTION)));
    let collection = match d.get(f::COLLECTION_ID) {
        Some(RawValue::Int64(n)) => u32::try_from(n).map_err(|_| bad_rules())?,
        None => 0,
        Some(_) => return Err(bad_rules()),
    };
    let tenant = match d.get(f::TENANT) {
        Some(RawValue::Int64(n)) => u32::try_from(n).map_err(|_| bad_rules())?,
        None => 0,
        Some(_) => return Err(bad_rules()),
    };
    Ok(Rules {
        ns: get_str(d, f::NS),
        validator,
        bypass: get_bool(d, f::BYPASS),
        collation: get_doc(d, f::COLLATION),
        collection,
        tenant,
        indexes: index_maintenance::read_rules(d.get(f::INDEXES))?,
        vars: get_doc(d, f::VARS),
        attempt: match d.get(f::ATTEMPT) {
            Some(RawValue::Int64(n)) => n,
            None => 0,
            Some(_) => return Err(bad_rules()),
        },
        probes: index_maintenance::read_probes(d.get(f::PROBES))?,
        capped: read_capped(d)?,
        timeseries: read_timeseries(d)?,
        unlogged: get_bool(d, f::UNLOGGED),
        shard_key: get_doc(d, f::SHARD_KEY),
        moves: get_bool(d, f::MOVES),
        rekeys: get_bool(d, f::REKEYS),
        clustered: get_bool(d, f::CLUSTERED),
        retry: match d.get(f::RETRY) {
            Some(RawValue::Binary { bytes, .. }) => {
                let mut b = bytes;
                let info = teseladb_changestream::meta::RetryInfo::take_from(&mut b)
                    .map_err(|_| bad_rules())?;
                if !b.is_empty() {
                    return Err(bad_rules());
                }
                Some(info)
            }
            None => None,
            Some(_) => return Err(bad_rules()),
        },
    })
}

impl DocMutation {
    /// Its encoding.
    pub fn encode(&self) -> Result<Vec<u8>, CommandError> {
        self.encode_for(Send::default())
    }

    /// Its encoding with `attempt` in its rules in place of theirs (when not 0).
    pub fn encode_with_attempt(&self, attempt: i64) -> Result<Vec<u8>, CommandError> {
        self.encode_for(Send { attempt, retry: None })
    }

    /// Its encoding for send `send`: its attempt and retryable write in place of the rules'.
    pub fn encode_for(&self, send: Send<'_>) -> Result<Vec<u8>, CommandError> {
        let mut d = Document::new();
        match self {
            DocMutation::Insert { docs, ordered, rules } => {
                d.push(f::KIND, INSERT);
                d.push(f::DOCS, docs.iter().cloned().map(Value::Document).collect::<Vec<_>>());
                d.push(f::ORDERED, *ordered);
                rules_doc(rules, &mut d, send);
            }
            DocMutation::Update { filter, update, array_filters, multi, upsert, image, rules } => {
                d.push(f::KIND, UPDATE);
                d.push(f::FILTER, filter.clone());
                d.push(f::UPDATE, update.clone());
                if let Some(a) = array_filters {
                    d.push(f::ARRAY_FILTERS, a.clone());
                }
                d.push(f::MULTI, *multi);
                if let Some(u) = upsert {
                    d.push(f::UPSERT, u.clone());
                }
                d.push(f::IMAGE, image.code());
                rules_doc(rules, &mut d, send);
            }
            DocMutation::Delete { filter, multi, image, rules } => {
                d.push(f::KIND, DELETE);
                d.push(f::FILTER, filter.clone());
                d.push(f::MULTI, *multi);
                d.push(f::IMAGE, image.code());
                rules_doc(rules, &mut d, send);
            }
            DocMutation::Backfill { rules } => {
                d.push(f::KIND, BACKFILL);
                rules_doc(rules, &mut d, send);
            }
            DocMutation::Erase { rules } => {
                d.push(f::KIND, ERASE);
                rules_doc(rules, &mut d, send);
            }
        }
        Ok(d.to_bytes_with(MUTATION_LIMITS)?)
    }

    /// Reads what [`DocMutation::encode`] wrote.
    pub fn decode(b: &[u8]) -> Result<DocMutation, CommandError> {
        let bad = || CommandError::new(INTERNAL_ERROR, "malformed mutation");
        let d = RawDoc::from_bytes_with(b, MUTATION_LIMITS)?;
        let rules = read_rules(d)?;
        let filter = || get_doc(d, f::FILTER).ok_or_else(bad);
        let multi = get_bool(d, f::MULTI);
        let image = Image::from_code(get_i32(d, f::IMAGE));
        match (d.get(f::KIND), d.get(f::DOCS)) {
            (Some(RawValue::Int32(INSERT)), Some(RawValue::Array(a))) => {
                let docs = a
                    .iter()
                    .map(|v| match v {
                        RawValue::Document(x) => Ok(Document::from(x)),
                        _ => Err(bad()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(DocMutation::Insert { docs, ordered: get_bool(d, f::ORDERED), rules })
            }
            (Some(RawValue::Int32(UPDATE)), _) => {
                let update = d.get(f::UPDATE).map(Value::from).ok_or_else(bad)?;
                let array_filters = match d.get(f::ARRAY_FILTERS) {
                    Some(RawValue::Array(a)) => Some(a.iter().map(Value::from).collect()),
                    _ => None,
                };
                let upsert = get_doc(d, f::UPSERT);
                let filter = filter()?;
                Ok(DocMutation::Update {
                    filter,
                    update,
                    array_filters,
                    multi,
                    upsert,
                    image,
                    rules,
                })
            }
            (Some(RawValue::Int32(DELETE)), _) => {
                Ok(DocMutation::Delete { filter: filter()?, multi, image, rules })
            }
            (Some(RawValue::Int32(BACKFILL)), _) => Ok(DocMutation::Backfill { rules }),
            (Some(RawValue::Int32(ERASE)), _) => Ok(DocMutation::Erase { rules }),
            _ => Err(bad()),
        }
    }
}

impl MutationResult {
    /// Its encoding.
    pub fn encode(&self) -> Result<Vec<u8>, CommandError> {
        let mut d = Document::new().with(f::N, self.n).with(f::N_MODIFIED, self.n_modified);
        if let Some(u) = &self.upserted {
            d.push(f::UPSERTED, u.clone());
        }
        if let Some(v) = &self.value {
            d.push(f::VALUE, v.clone());
        }
        if self.attempt != 0 {
            d.push(f::ATTEMPT, self.attempt);
        }
        if let Some(m) = self.mark {
            let at = Document::new()
                .with("p", i64::try_from(m.physical).unwrap_or(i64::MAX))
                .with("l", i64::from(m.logical));
            d.push(f::MARK, at);
        }
        let errors: Vec<Value> = self
            .errors
            .iter()
            .map(|e| {
                let mut x = Document::new()
                    .with(f::INDEX, e.index as i64)
                    .with(f::CODE, e.error.code.code)
                    .with(f::ERRMSG, e.error.errmsg.as_str());
                for (k, v) in e.error.extra.iter() {
                    x.push(k, v.clone());
                }
                Value::Document(x)
            })
            .collect();
        d.push(f::ERRORS, errors);
        d.push(f::WARNINGS, self.warnings.iter().cloned().map(Value::Document).collect::<Vec<_>>());
        if self.keys != (0, 0) {
            d.push(f::KEYS_INSERTED, self.keys.0);
            d.push(f::KEYS_DELETED, self.keys.1);
        }
        Ok(d.to_bytes_with(MUTATION_LIMITS)?)
    }

    /// Reads what [`MutationResult::encode`] wrote (an unreadable result is an `InternalError`).
    pub fn decode(b: &[u8]) -> Result<MutationResult, CommandError> {
        let bad = || CommandError::new(INTERNAL_ERROR, "malformed mutation result");
        let d = RawDoc::from_bytes_with(b, MUTATION_LIMITS)?;
        let num = |k| match d.get(k) {
            Some(RawValue::Int64(n)) => n,
            _ => 0,
        };
        let mut r = MutationResult {
            n: num(f::N),
            n_modified: num(f::N_MODIFIED),
            upserted: d.get(f::UPSERTED).map(Value::from),
            value: get_doc(d, f::VALUE),
            attempt: num(f::ATTEMPT),
            keys: (num(f::KEYS_INSERTED), num(f::KEYS_DELETED)),
            mark: read_mark(d.get(f::MARK)),
            ..MutationResult::default()
        };
        if let Some(RawValue::Array(a)) = d.get(f::ERRORS) {
            for e in a.iter() {
                let RawValue::Document(e) = e else { return Err(bad()) };
                r.errors.push(read_error(e).ok_or_else(bad)?);
            }
        }
        if let Some(RawValue::Array(a)) = d.get(f::WARNINGS) {
            r.warnings = a
                .iter()
                .filter_map(|w| match w {
                    RawValue::Document(x) => Some(Document::from(x)),
                    _ => None,
                })
                .collect();
        }
        Ok(r)
    }
}

/// The mark [`MutationResult::encode`] wrote.
fn read_mark(v: Option<RawValue<'_>>) -> Option<Hlc> {
    let Some(RawValue::Document(d)) = v else { return None };
    let (Some(RawValue::Int64(p)), Some(RawValue::Int64(l))) = (d.get("p"), d.get("l")) else {
        return None;
    };
    Some(Hlc { physical: u64::try_from(p).ok()?, logical: u32::try_from(l).ok()? })
}

/// What one send of a write carries beyond its mutation: the attempt of a retryable write
/// ([`Rules::attempt`]) and its session ([`Rules::retry`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Send<'a> {
    /// The attempt (0 keeps the rules').
    pub attempt: i64,
    /// The retryable write (`None` keeps the rules').
    pub retry: Option<&'a teseladb_changestream::meta::RetryInfo>,
}

fn read_error(e: RawDoc<'_>) -> Option<WriteError> {
    let Some(RawValue::Int64(index)) = e.get(f::INDEX) else { return None };
    let Some(RawValue::Int32(code)) = e.get(f::CODE) else { return None };
    let code = crate::error::code(code).unwrap_or(INTERNAL_ERROR);
    let mut error = CommandError::new(code, get_str(e, f::ERRMSG));
    for (k, v) in Document::from(e).iter() {
        if ![f::INDEX, f::CODE, f::ERRMSG].contains(&k) {
            error.extra.push(k, v.clone());
        }
    }
    Some(WriteError { index: usize::try_from(index).ok()?, error })
}

/// The command an update with `image` runs in, as RefDb's executor names it.
pub fn executor_of(image: Image) -> &'static str {
    match image {
        Image::None => super::crud::UPDATE,
        Image::Before | Image::After => super::crud::FIND_AND_MODIFY,
    }
}

/// An error raised while an update of command `op` runs, as a `refrouter` 9.0.4 words it
/// ("Plan executor error during update :: caused by :: …", measured); errors of parsing the
/// statement come without it.
pub fn executor_error(op: &str) -> impl Fn(CommandError) -> CommandError + '_ {
    move |mut e| {
        e.errmsg = format!("Plan executor error during {op} :: caused by :: {}", e.errmsg);
        e
    }
}

/// The error of an MQL failure, with its `errInfo`.
pub fn from_mql(e: MqlError) -> CommandError {
    let code = crate::error::code(e.code.code)
        .unwrap_or(crate::error::Code { code: e.code.code, name: e.code.name });
    let mut err = CommandError::new(code, e.message);
    if let Some(info) = e.info {
        err.extra.push(f::ERR_INFO, info);
    }
    err
}

/// How a write stores a document, for the words of its error when the document is too large or
/// too deep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    /// An inserted document.
    Insert,
    /// The new version of an updated or replaced document.
    Update,
    /// The document an upsert inserts.
    Upsert,
}

/// The BSON of a document a write stores, refused as a `refrouter` 9.0.4 refuses one past 16 MiB or
/// past [`MAX_DEPTH`] levels (measured, 003 T009). An update's error still gets the executor's
/// prefix from its caller.
pub fn stored_bytes(doc: &Document, how: Stored) -> Result<Vec<u8>, CommandError> {
    // Written within the mutation's size first, to name the document's whole size.
    let wide = Limits { max_size: MUTATION_LIMITS.max_size, max_depth: MAX_DEPTH };
    let too_large = |size: usize| {
        let text = match how {
            Stored::Insert => {
                format!("object to insert too large. size in bytes: {size}, max size: {MAX_SIZE}")
            }
            Stored::Update => format!("Resulting document after update is larger than {MAX_SIZE}"),
            Stored::Upsert => format!("Document to upsert is larger than {MAX_SIZE}"),
        };
        CommandError::new(BSON_OBJECT_TOO_LARGE, text)
    };
    let bytes = doc.to_bytes_with(wide).map_err(|e| match e {
        BsonError::TooDeep { .. } => {
            let text = match how {
                Stored::Insert => format!(
                    "cannot insert document :: caused by :: object exceeds {MAX_DEPTH} levels of \
                     nesting"
                ),
                Stored::Update | Stored::Upsert => {
                    format!("Document exceeds maximum nesting depth of {MAX_DEPTH}")
                }
            };
            CommandError::new(OVERFLOW, text)
        }
        BsonError::TooLarge { size, .. } => too_large(size),
        other => CommandError::from(other),
    })?;
    if bytes.len() > MAX_SIZE {
        return Err(too_large(bytes.len()));
    }
    Ok(bytes)
}

/// The validation level that keeps every document valid (9.0).
const CONSTRAINT_LEVEL: &str = "constraint";

/// What evaluating against the rules needs, parsed once per mutation.
/// The fields of a log line of validation in the result's `warnings`.
pub const LOG_ID_FIELD: &str = f::LOG_ID;
/// The document of a log line of validation.
pub const LOG_DOCUMENT_FIELD: &str = f::LOG_DOCUMENT;
/// The `errInfo` of a log line of validation.
pub const LOG_ERR_INFO_FIELD: &str = f::ERR_INFO;

/// The log line of a document `warn` lets through (RefDb's identifier, measured).
pub const LOG_WOULD_FAIL: i32 = 20_294;
/// The log line of a document `errorAndLog` refuses (RefDb's identifier, measured).
pub const LOG_FAILED: i32 = 7_488_700;

struct Checker {
    validator: Option<Validator>,
    /// A client's write to a collection named as a resharding's copy: no document it would
    /// store is written ([`super::reshard::not_a_copy`]). A resharding's own writes to its copy
    /// are [`Rules::unlogged`].
    not_a_copy: Option<String>,
    /// The log lines of `warn` and `errorAndLog` this write makes: they travel in the result's
    /// `warnings` to the node that runs the command, which writes them.
    log: std::cell::RefCell<Vec<Document>>,
    bypass: bool,
    /// The collection's level is `constraint`, which does not let writes bypass validation.
    constraint: bool,
    collation: Option<teseladb_collation::Collation>,
    /// The collection's secondary indexes.
    indexes: Maintainer,
    /// The unique keys the write's statements have put or removed so far.
    unique: std::cell::RefCell<UniqueGuard>,
    /// The command's `let` variables.
    vars: Option<Document>,
    /// The collection, when it is capped: its index entries end in the document's `seq`.
    capped: Option<(u32, u32)>,
    /// The collection has a shard key: its index entries end in its documents' keys after
    /// their prefix.
    sharded: bool,
    /// Its shard key, which an update changes only when the write [`Rules::moves`].
    shard_key: Option<Document>,
    /// The tenant and the collection of a write that moves documents ([`Rules::moves`]).
    moves: Option<(u32, u32)>,
    /// The documents the write moved, from their old key to their new one.
    moved: std::cell::RefCell<Vec<(Vec<u8>, Vec<u8>)>>,
    /// The description of each document the write updated with operators, by its key (005
    /// FR-008, 014 FR-009).
    changes: std::cell::RefCell<Vec<(Vec<u8>, UpdateDescription)>>,
}

impl Checker {
    /// The checker of a write with rules `r`; `enforce` is false in an index build's backfill,
    /// which takes the duplicates of an index with `prepareUnique`.
    fn new(r: &Rules, enforce: bool) -> Result<Checker, CommandError> {
        let validator = match &r.validator {
            Some((spec, level, action)) => {
                let bytes = spec.to_bytes()?;
                let raw = RawDoc::from_bytes(&bytes)?;
                let level = ValidationLevel::parse(level).map_err(from_mql)?;
                let action = ValidationAction::parse(action).map_err(from_mql)?;
                Some(Validator::parse(raw, level, action).map_err(from_mql)?)
            }
            None => None,
        };
        let collation = match &r.collation {
            Some(c) => {
                let bytes = c.to_bytes()?;
                let raw = RawDoc::from_bytes(&bytes)?;
                match parse_collation(raw)? {
                    ParsedCollation::Simple => None,
                    ParsedCollation::Collation(c, _) => Some(c),
                }
            }
            None => None,
        };
        let constraint = matches!(&r.validator, Some((_, level, _)) if level == CONSTRAINT_LEVEL);
        Ok(Checker {
            validator,
            not_a_copy: (!r.unlogged && super::reshard::copy_name(&r.ns)).then(|| r.ns.clone()),
            log: std::cell::RefCell::default(),
            bypass: r.bypass,
            constraint,
            collation,
            indexes: Maintainer::new(&r.ns, (r.tenant, r.collection), &r.indexes)?
                .with_probes(r.probes.clone(), enforce),
            unique: std::cell::RefCell::default(),
            vars: r.vars.clone(),
            capped: r.capped.map(|_| (r.tenant, r.collection)),
            sharded: r.shard_key.is_some(),
            shard_key: r.shard_key.clone(),
            moves: r.moves.then_some((r.tenant, r.collection)),
            moved: std::cell::RefCell::default(),
            changes: std::cell::RefCell::default(),
        })
    }

    fn ctx(&self) -> MatchContext<'_> {
        MatchContext {
            collation: self.collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator),
            ext: &super::engines::NodeExtensions,
            vars: self.vars.as_ref(),
        }
    }

    /// Checks a new version against the validator, keeping the log line of a `warn` or an
    /// `errorAndLog`; refuses it in a collection named as a resharding's copy.
    fn check(&self, doc: &[u8], old: Option<RawDoc<'_>>) -> Result<(), CommandError> {
        if let Some(ns) = &self.not_a_copy {
            return Err(super::reshard::not_a_copy(ns));
        }
        let Some(v) = &self.validator else { return Ok(()) };
        if self.bypass && self.constraint {
            let msg = "bypassDocumentValidation is not permitted with 'constraint' validationLevel";
            return Err(CommandError::new(crate::error::BAD_VALUE, msg));
        }
        let raw = RawDoc::from_bytes(doc)?;
        let line = |id: i32, info: Document| {
            Document::new()
                .with(f::LOG_ID, id)
                .with(f::LOG_DOCUMENT, Document::from(raw))
                .with(f::ERR_INFO, info)
        };
        match v.check(raw, old, self.bypass, &self.ctx()) {
            Ok(Verdict::Warned(info)) => {
                self.log.borrow_mut().push(line(LOG_WOULD_FAIL, info));
                Ok(())
            }
            Ok(Verdict::Valid | Verdict::Skipped | Verdict::Bypassed) => Ok(()),
            Err(e) => {
                if let (ValidationAction::ErrorAndLog, Some(info)) = (v.action(), &e.info) {
                    self.log.borrow_mut().push(line(LOG_FAILED, info.clone()));
                }
                Err(from_mql(e))
            }
        }
    }

    /// The index entries to write and delete when version `old` of the document at `key` becomes
    /// the bytes `new`; `lookup` gives the stored versions the leader read, for the unique keys.
    /// The entries end in the document's `_id`, in a capped collection in the `seq` of its key,
    /// and in a collection with a shard key in its key after the prefix.
    fn entries_at<'a>(
        &self,
        key: Option<&[u8]>,
        old: Option<RawDoc<'a>>,
        new: Option<&'a [u8]>,
        lookup: &Lookup<'_>,
    ) -> Result<(Vec<Put>, Vec<Vec<u8>>), CommandError> {
        let new = new.map(RawDoc::from_bytes).transpose()?;
        let stored = |k: &[u8]| lookup(k).map(|(_, v)| v);
        let pk = match (self.capped, key) {
            (Some((t, c)), Some(k)) => Some(
                teseladb_range::capped::seq_of(t, c, k)
                    .map(teseladb_keys::PrimaryKey::seq)
                    .ok_or_else(|| {
                        CommandError::new(INTERNAL_ERROR, "not a capped document key")
                    })?,
            ),
            (None, Some(k)) if self.sharded => Some(teseladb_keys::PrimaryKey::from_bytes(
                k.get(DOC_PREFIX..).unwrap_or_default().to_vec(),
            )),
            _ => None,
        };
        self.indexes.changes(old, new, pk.as_ref(), &mut self.unique.borrow_mut(), &stored)
    }

    /// The log lines this write made.
    fn take_log(&self) -> Vec<Document> {
        self.log.take()
    }

    /// The change description of this write: what it changed in each document it updated with
    /// operators.
    fn take_changes(&self) -> Result<Vec<u8>, CommandError> {
        Ok(UpdateDescription::encode_all(&self.changes.take())?)
    }
}

/// The change an operator made to `path`, as the description takes it.
fn touch((change, path): (PathChange, String)) -> Touch {
    match change {
        PathChange::Assign => Touch::Assign(path),
        PathChange::Unset => Touch::Unset(path),
        PathChange::Rename(to) => Touch::Rename(path, to),
        PathChange::Append => Touch::Append(path),
        PathChange::Rewrite => Touch::Rewrite(path),
    }
}

fn bad_rules() -> CommandError {
    CommandError::new(INTERNAL_ERROR, "malformed collection rules")
}

/// The 11000 of a document whose `_id` is taken. In a clustered collection the documents are
/// the index: the message names no index and ends in the value found, an empty document, which
/// `foundValue` repeats (measured on a `refrouter` 9.0.4 on 05-11-2026).
fn duplicate(rules: &Rules, id: &Value) -> CommandError {
    let (ns, shown) = (&rules.ns, crate::commands::display(id));
    let errmsg = match rules.clustered {
        true => format!(
            "E11000 duplicate key error collection: {ns} dup key: {{ _id: {shown} }} found value: {{}}"
        ),
        false => format!(
            "E11000 duplicate key error collection: {ns} index: _id_ dup key: {{ _id: {shown} }}"
        ),
    };
    let mut e = CommandError::new(DUPLICATE_KEY, errmsg);
    e.extra.push("keyPattern", Document::new().with("_id", 1));
    e.extra.push("keyValue", Document::new().with("_id", id.clone()));
    if rules.clustered {
        e.extra.push("foundValue", Document::new());
    }
    e
}

/// The `_id` of a document.
fn id_of(doc: &Document) -> Value {
    doc.get("_id").cloned().unwrap_or(Value::Null)
}

fn put(key: &[u8], bytes: Vec<u8>) -> Put {
    Put { key: key.to_vec(), value_type: DOCUMENT, value: bytes }
}

/// A [`DocMutation`] bound to the keys of its write, ready for the range.
pub struct Bound {
    /// The mutation.
    pub mutation: DocMutation,
    /// Keys of the write, in the mutation's order (for an insert one per document, possibly
    /// repeated; for an update or delete the candidates, then the upsert's key).
    pub keys: Vec<Vec<u8>>,
}

impl Bound {
    /// Its encoding: the keys, then the mutation.
    pub fn encode(&self) -> Result<Vec<u8>, CommandError> {
        self.encode_with_attempt(0)
    }

    /// Its encoding with `attempt` for this send of a retryable write ([`Rules::attempt`]).
    pub fn encode_with_attempt(&self, attempt: i64) -> Result<Vec<u8>, CommandError> {
        self.encode_for(Send { attempt, retry: None })
    }

    /// Its encoding for send `send` ([`DocMutation::encode_for`]).
    pub fn encode_for(&self, send: Send<'_>) -> Result<Vec<u8>, CommandError> {
        let keys: Vec<Value> =
            self.keys.iter().map(|k| Value::Binary { subtype: 0, bytes: k.clone() }).collect();
        let m = self.mutation.encode_for(send)?;
        let d =
            Document::new().with("keys", keys).with("m", Value::Binary { subtype: 0, bytes: m });
        Ok(d.to_bytes_with(MUTATION_LIMITS)?)
    }

    /// Reads what [`Bound::encode`] wrote.
    pub fn decode(b: &[u8]) -> Result<Bound, CommandError> {
        let bad = || CommandError::new(INTERNAL_ERROR, "malformed mutation");
        let d = RawDoc::from_bytes_with(b, MUTATION_LIMITS)?;
        let (Some(RawValue::Array(keys)), Some(RawValue::Binary { bytes, .. })) =
            (d.get("keys"), d.get("m"))
        else {
            return Err(bad());
        };
        let keys = keys
            .iter()
            .map(|k| match k {
                RawValue::Binary { bytes, .. } => Ok(bytes.to_vec()),
                _ => Err(bad()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Bound { mutation: DocMutation::decode(bytes)?, keys })
    }

    /// The rules of its mutation.
    pub fn rules(&self) -> &Rules {
        match &self.mutation {
            DocMutation::Insert { rules, .. }
            | DocMutation::Update { rules, .. }
            | DocMutation::Delete { rules, .. }
            | DocMutation::Backfill { rules }
            | DocMutation::Erase { rules } => rules,
        }
    }

    /// Whether the write keeps secondary indexes, whose entries may be in another range.
    pub fn keeps_indexes(&self) -> bool {
        !self.rules().indexes.is_empty()
    }

    /// The probes in its rules.
    pub fn probes(&self) -> &[teseladb_index::prepared::Probe] {
        &self.rules().probes
    }

    /// The bound mutation of retryable write `retry` (018 FR-009).
    pub fn with_retry(&self, retry: teseladb_changestream::meta::RetryInfo) -> Bound {
        let mut mutation = self.mutation.clone();
        match &mut mutation {
            DocMutation::Insert { rules, .. }
            | DocMutation::Update { rules, .. }
            | DocMutation::Delete { rules, .. }
            | DocMutation::Backfill { rules }
            | DocMutation::Erase { rules } => rules.retry = Some(retry),
        }
        Bound { mutation, keys: self.keys.clone() }
    }

    /// `needsRetryImage` of a `findAndModify` (018 FR-009): 1 the document before, 2 after.
    fn retry_image(&self) -> u8 {
        match &self.mutation {
            DocMutation::Update { image, .. } | DocMutation::Delete { image, .. } => match image {
                Image::Before => 1,
                Image::After => 2,
                Image::None => 0,
            },
            _ => 0,
        }
    }

    /// The bound mutation with `probes` in its rules (009 FR-004).
    pub fn with_probes(&self, probes: Vec<teseladb_index::prepared::Probe>) -> Bound {
        let mut mutation = self.mutation.clone();
        match &mut mutation {
            DocMutation::Insert { rules, .. }
            | DocMutation::Update { rules, .. }
            | DocMutation::Delete { rules, .. }
            | DocMutation::Backfill { rules }
            | DocMutation::Erase { rules } => rules.probes = probes,
        }
        Bound { mutation, keys: self.keys.clone() }
    }

    /// The distinct keys the range latches, in first-seen order: the write's, then those of its
    /// probes.
    pub fn latch_keys(&self) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = Vec::new();
        let probed = index_maintenance::probe_keys(&self.rules().probes);
        for k in self.keys.iter().chain(&probed) {
            if !out.contains(k) {
                out.push(k.clone());
            }
        }
        out
    }
}

impl Mutation for Bound {
    fn eval(&self, current: &[Option<(u8, &[u8])>], ts: Hlc) -> Result<Outcome, MutationError> {
        self.eval_with(&self.latch_keys(), current, ts)
    }

    /// The entries of a unique index the write puts (009 FR-004).
    fn reads(&self, outcome: &Outcome) -> Vec<Vec<u8>> {
        index_maintenance::unique_keys(&self.rules().indexes, &outcome.puts)
    }

    fn eval_with(
        &self,
        latch: &[Vec<u8>],
        current: &[Option<(u8, &[u8])>],
        ts: Hlc,
    ) -> Result<Outcome, MutationError> {
        let lookup = |k: &[u8]| {
            latch.iter().position(|x| x == k).and_then(|i| current.get(i).copied().flatten())
        };
        let enforce = !matches!(self.mutation, DocMutation::Backfill { .. });
        let checker = Checker::new(self.rules(), enforce);
        let r = checker.as_ref().map_err(Clone::clone).and_then(|checker| match &self.mutation {
            m @ (DocMutation::Insert { .. }
            | DocMutation::Update { .. }
            | DocMutation::Delete { .. })
                if self.rules().capped.is_some() =>
            {
                capped::eval(&self.keys, m, ts, checker, &lookup)
            }
            DocMutation::Insert { docs, rules: Rules { timeseries: Some(t), .. }, .. } => {
                timeseries::insert(&self.keys, docs, (&t.spec, t.bucket), ts, checker, &lookup)
            }
            DocMutation::Insert { docs, ordered, rules } => {
                insert(&self.keys, docs, *ordered, rules, ts, checker, &lookup)
            }
            DocMutation::Update { .. } | DocMutation::Delete { .. } => {
                modify(&self.keys, &self.mutation, ts, checker, &lookup)
            }
            DocMutation::Backfill { .. } => backfill(&self.keys, checker, &lookup),
            DocMutation::Erase { .. } => {
                let gone: Vec<Vec<u8>> =
                    self.keys.iter().filter(|k| lookup(k).is_some()).cloned().collect();
                let r = MutationResult { n: gone.len() as i64, ..MutationResult::default() };
                Ok((Vec::new(), gone, r))
            }
        });
        // A refused statement still answers: its error travels in the result with its `errInfo`.
        // A value of an index with `prepareUnique` to probe first: nothing is written and the
        // command runs the write again with the probes.
        if let Ok(checker) = &checker
            && let Some(e) = checker.indexes.probe_error()
        {
            return Err(e.to_mutation());
        }
        // A capped write whose scan of the oldest documents fell short: the command scans again.
        if let Err(e) = &r
            && (capped::is_refusal(e) || timeseries::is_refusal(e))
        {
            return Err(e.to_mutation());
        }
        // A refused statement writes nothing, so it describes nothing either.
        let applied = r.is_ok();
        let (puts, deletes, mut result) = r.unwrap_or_else(|error| {
            let errors = vec![WriteError { index: 0, error }];
            (Vec::new(), Vec::new(), MutationResult { errors, ..MutationResult::default() })
        });
        let mut change = Vec::new();
        let mut moved = Vec::new();
        if let Ok(checker) = &checker {
            moved = checker.moved.take();
            result.warnings.extend(checker.take_log());
            let changes = checker.take_changes().map_err(|e| e.to_mutation())?;
            if applied {
                change = changes;
            }
        }
        let attempt = self.rules().attempt;
        result.attempt = attempt;
        if applied && self.rules().retry.is_some() {
            result.mark = Some(ts);
        }
        if applied {
            result.keys = key_counts((&puts, &deletes), &lookup, self.rules().capped.is_some());
        }
        let mut puts = puts;
        // An update that writes a document it found, without a description, replaced it.
        let replaced: Vec<Vec<u8>> = match &self.mutation {
            DocMutation::Update { .. } => puts
                .iter()
                .filter(|p| p.value_type == DOCUMENT && lookup(&p.key).is_some())
                .map(|p| p.key.clone())
                .collect(),
            _ => Vec::new(),
        };
        // An erase removes what a dropped index or collection left: it changes no document.
        let erase = matches!(self.mutation, DocMutation::Erase { .. });
        if !self.rules().unlogged && !erase {
            let retry = self.rules().retry.map(|r| RetryInfo { image: self.retry_image(), ..r });
            let rekeys = self.rules().rekeys;
            let docs =
                Written { change: &change, replaced: &replaced, moved: &moved, retry, rekeys };
            puts.extend(history_puts(&self.keys, (&puts, &deletes), &docs)?);
        }
        match result.encode() {
            Ok(result) => Ok(Outcome { puts, deletes, change, result }),
            // A result past the limits of an entry writes nothing and answers with that error.
            Err(error) => {
                let errors = vec![WriteError { index: 0, error }];
                let refused = MutationResult { errors, attempt, ..MutationResult::default() };
                let result = refused.encode().map_err(|e| MutationError(e.errmsg))?;
                Ok(Outcome { puts: Vec::new(), deletes: Vec::new(), change: Vec::new(), result })
            }
        }
    }
}

type Evaluated = (Vec<Put>, Vec<Vec<u8>>, MutationResult);

/// The index keys a write adds and removes (`keysInserted`, `keysDeleted` of a `refnode` 9.0.4,
/// measured 01-11-2026): its entries in the collection's indexes, and the key of `_id` of each
/// document it inserts or deletes, which is the document's own key here (a capped collection
/// keeps an index of `_id` of its own, counted among the others).
pub fn key_counts(
    (puts, deletes): (&[Put], &[Vec<u8>]),
    lookup: &Lookup<'_>,
    capped: bool,
) -> (i64, i64) {
    let index = |k: &[u8]| {
        crate::ops::space_of(k).is_some_and(|s| {
            s != teseladb_keys::space::DOCUMENTS && s < index_maintenance::MULTIKEY_MARKS
        })
    };
    let document =
        |k: &[u8]| !capped && crate::ops::space_of(k) == Some(teseladb_keys::space::DOCUMENTS);
    let added = puts
        .iter()
        .filter(|p| index(&p.key) || (document(&p.key) && lookup(&p.key).is_none()))
        .count();
    let removed = deletes.iter().filter(|k| index(k) || document(k)).count();
    (i64::try_from(added).unwrap_or(i64::MAX), i64::try_from(removed).unwrap_or(i64::MAX))
}

/// The documents a write changes, in the write's order: its document keys that it puts as
/// documents or deletes, then the documents it puts under other keys (a capped collection's).
pub fn written_docs(keys: &[Vec<u8>], puts: &[Put], deletes: &[Vec<u8>]) -> Vec<Vec<u8>> {
    use teseladb_changestream::changes::is_document_key;
    let put_doc = |k: &[u8]| puts.iter().any(|p| p.value_type == DOCUMENT && p.key == k);
    let mut docs: Vec<Vec<u8>> = Vec::new();
    for k in keys.iter().filter(|k| is_document_key(k)) {
        if (put_doc(k) || deletes.contains(k)) && !docs.contains(k) {
            docs.push(k.clone());
        }
    }
    let doc_puts = puts.iter().filter(|p| p.value_type == DOCUMENT && is_document_key(&p.key));
    for p in doc_puts {
        if !docs.contains(&p.key) {
            docs.push(p.key.clone());
        }
    }
    docs
}

/// What a write did to its documents, for their events: the encoded descriptions of its
/// updates by operators, the documents it replaced, and those it moved with their shard key.
struct Written<'a> {
    change: &'a [u8],
    replaced: &'a [Vec<u8>],
    moved: &'a [(Vec<u8>, Vec<u8>)],
    retry: Option<RetryInfo>,
    rekeys: bool,
}

/// The versions next to each document a write changes with what its event needs (014 FR-005,
/// FR-009): its position in the write and the description of an update by operators; a document
/// that moved (018 FR-001) leaves its old key without an event and keeps the update's at the new.
fn history_puts(
    keys: &[Vec<u8>],
    (puts, deletes): (&[Put], &[Vec<u8>]),
    w: &Written<'_>,
) -> Result<Vec<Put>, MutationError> {
    use teseladb_changestream::meta::{DocWrite, Moved, history_with};
    let mut updates = UpdateDescription::decode_all(w.change)
        .map_err(|e| MutationError(format!("change description: {e:?}")))?;
    let writes: Vec<DocWrite> = written_docs(keys, puts, deletes)
        .into_iter()
        .map(|key| {
            let update = updates.iter().position(|(u, _)| *u == key).map(|i| updates.remove(i).1);
            let put = puts.iter().any(|p| p.key == key);
            let from = w.moved.iter().find(|(_, to)| *to == key).map(|(from, _)| from.clone());
            let replace = update.is_none() && (from.is_some() || w.replaced.contains(&key));
            let moved = match from {
                _ if w.rekeys => Some(Moved::Internal),
                Some(f) => Some(Moved::From(f)),
                None => w.moved.iter().any(|(f, _)| *f == key).then_some(Moved::Away),
            };
            DocWrite { delete: !put, replace, update, doc: None, key, moved }
        })
        .collect();
    history_with(&writes, None, w.retry.as_ref())
        .into_iter()
        .map(|(key, meta)| {
            let value = meta.encode().map_err(|e| MutationError(e.to_string()))?;
            Ok(Put { key, value_type: teseladb_range::HISTORY_VALUE, value })
        })
        .collect()
}
type Lookup<'a> = dyn Fn(&[u8]) -> Option<(u8, &'a [u8])> + 'a;

/// `doc` with each top-level `Timestamp(0, 0)` other than `_id` set to the write's time, one
/// increment apart, as a `refrouter` 9.0.4 does on inserts and replacements (measured); `None` when
/// it has none.
fn fill_timestamps(doc: &Document, ts: Hlc) -> Option<Document> {
    let empty = Value::Timestamp { time: 0, increment: 0 };
    let fields: Vec<String> = doc
        .iter()
        .filter(|(k, v)| *k != "_id" && **v == empty)
        .map(|(k, _)| k.to_owned())
        .collect();
    if fields.is_empty() {
        return None;
    }
    let (time, first) = teseladb_hlc::to_timestamp(ts);
    let mut out = doc.clone();
    for (n, k) in (0u32..).zip(fields) {
        out.insert(k, Value::Timestamp { time, increment: first.saturating_add(n) });
    }
    Some(out)
}

fn insert(
    keys: &[Vec<u8>],
    docs: &[Document],
    ordered: bool,
    rules: &Rules,
    ts: Hlc,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    let mut r = MutationResult::default();
    let (mut puts, mut deletes): (Vec<Put>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
    for (i, (doc, key)) in docs.iter().zip(keys).enumerate() {
        let filled = fill_timestamps(doc, ts);
        let doc = filled.as_ref().unwrap_or(doc);
        let taken = lookup(key).is_some() || puts.iter().any(|p| &p.key == key);
        let outcome = if taken {
            Err(duplicate(rules, &id_of(doc)))
        } else {
            stored_bytes(doc, Stored::Insert)
                .and_then(|b| checker.check(&b, None).map(|()| b))
                .and_then(|b| checker.entries_at(Some(key), None, Some(&b), lookup).map(|e| (b, e)))
        };
        match outcome {
            Ok((bytes, (entries, gone))) => {
                puts.push(put(key, bytes));
                puts.extend(entries);
                deletes.extend(gone);
                r.n += 1;
            }
            Err(error) => {
                r.errors.push(WriteError { index: i, error });
                if ordered {
                    break;
                }
            }
        }
    }
    Ok((puts, deletes, r))
}

/// The entries of the indexes being built for the current version of each document of `keys`;
/// `n` counts the documents. A document an index refuses stops it with nothing written, its
/// error at its position among `keys` (the build says where its scan stopped).
fn backfill(
    keys: &[Vec<u8>],
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    let (mut puts, mut deletes) = (Vec::new(), Vec::new());
    let mut r = MutationResult::default();
    for (index, key) in keys.iter().enumerate() {
        let Some((_, bytes)) = lookup(key) else { continue };
        match checker.entries_at(Some(key), None, Some(bytes), lookup) {
            Ok((entries, gone)) => {
                puts.extend(entries);
                deletes.extend(gone);
                r.n += 1;
            }
            Err(error) => {
                let r = MutationResult { errors: vec![WriteError { index, error }], ..r };
                return Ok((Vec::new(), Vec::new(), r));
            }
        }
    }
    Ok((puts, deletes, r))
}

/// The indexes of a build checking the documents of a collection one by one, as its backfill
/// does, without writing: what an index refuses, and the duplicates of a unique index among the
/// documents checked so far.
pub(crate) struct DocCheck(Checker);

impl DocCheck {
    pub(crate) fn new(rules: &Rules) -> Result<DocCheck, CommandError> {
        Checker::new(rules, false).map(DocCheck)
    }

    /// Why the indexes refuse document `bytes` stored at `key`, if they do.
    pub(crate) fn check(&self, key: &[u8], bytes: &[u8]) -> Option<CommandError> {
        let none = |_: &[u8]| None;
        self.0.entries_at(Some(key), None, Some(bytes), &none).err()
    }
}

/// The documents of `rows` (keys and stored bytes, as a scan reads them) the indexes of `rules`
/// refuse, by their position in `rows`, with why. A duplicate in a unique index is no refusal: a
/// build finds those once its scan is over.
pub(crate) fn refusals(
    rules: &Rules,
    rows: &[(Vec<u8>, Vec<u8>)],
) -> Result<Vec<(usize, CommandError)>, CommandError> {
    let check = DocCheck::new(rules)?;
    Ok(rows
        .iter()
        .enumerate()
        .filter_map(|(at, (key, bytes))| check.check(key, bytes).map(|e| (at, e)))
        .filter(|(_, e)| e.code != DUPLICATE_KEY)
        .collect())
}

fn now_of(ts: Hlc) -> Now {
    Now { millis: i64::try_from(ts.physical).unwrap_or(i64::MAX), timestamp: to_timestamp(ts) }
}

fn modify(
    keys: &[Vec<u8>],
    m: &DocMutation,
    ts: Hlc,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    let (filter, multi, image, rules) = match m {
        DocMutation::Update { filter, multi, image, rules, .. }
        | DocMutation::Delete { filter, multi, image, rules } => (filter, *multi, *image, rules),
        DocMutation::Insert { .. } | DocMutation::Backfill { .. } | DocMutation::Erase { .. } => {
            return Err(CommandError::new(INTERNAL_ERROR, "not a modify"));
        }
    };
    let fbytes = filter.to_bytes_with(command_limits())?;
    let fraw = RawDoc::from_bytes_with(&fbytes, command_limits())?;
    let parsed = Filter::parse(fraw, FilterUse::Query).map_err(from_mql)?;
    let update = match m {
        DocMutation::Update { update, array_filters, .. } => {
            let holder = Document::new()
                .with("u", update.clone())
                .with("af", array_filters.clone().map_or(Value::Null, Value::Array));
            let hb = holder.to_bytes_with(command_limits())?;
            let h = RawDoc::from_bytes_with(&hb, command_limits())?;
            let af = match h.get("af") {
                Some(RawValue::Array(a)) => Some(a),
                _ => None,
            };
            let u = h.get("u").ok_or_else(bad_rules)?;
            Some(Update::parse(u, af).map_err(from_mql)?)
        }
        DocMutation::Insert { .. }
        | DocMutation::Delete { .. }
        | DocMutation::Backfill { .. }
        | DocMutation::Erase { .. } => None,
    };
    let ctx = checker.ctx();
    let mut r = MutationResult::default();
    let (mut puts, mut deletes) = (Vec::new(), Vec::new());
    let upsert = match m {
        DocMutation::Update { upsert: Some(u), .. } => Some(u),
        DocMutation::Insert { .. }
        | DocMutation::Update { .. }
        | DocMutation::Delete { .. }
        | DocMutation::Backfill { .. }
        | DocMutation::Erase { .. } => None,
    };
    let candidates = if upsert.is_some() { &keys[..keys.len().saturating_sub(1)] } else { keys };
    for key in candidates {
        let Some((_, bytes)) = lookup(key) else { continue };
        // A stored document always parses: one that does not is a fault, not a miss.
        let doc = RawDoc::from_bytes(bytes)?;
        let position = parsed.match_position(doc, &ctx).map_err(from_mql);
        let position = match (&update, position) {
            (Some(_), Err(e)) => return Err(executor_error(executor_of(image))(e)),
            (_, p) => p?,
        };
        let Some(position) = position else { continue };
        r.n += 1;
        match &update {
            None => {
                deletes.push(key.clone());
                let (entries, gone) = checker.entries_at(Some(key), Some(doc), None, lookup)?;
                puts.extend(entries);
                deletes.extend(gone);
                if image != Image::None {
                    r.value = Some(Document::from(doc));
                }
            }
            Some(u) => {
                let uctx = UpdateContext { matcher: ctx, position, now: now_of(ts), insert: false };
                let during = executor_error(executor_of(image));
                let mut out = u.apply(doc, &uctx).map_err(from_mql).map_err(&during)?;
                if let Some(filled) =
                    u.is_replacement().then(|| fill_timestamps(&out.doc, ts)).flatten()
                {
                    out = UpdateOutcome { doc: filled, modified: true };
                }
                if out.modified {
                    let target = match &checker.shard_key {
                        Some(k) => {
                            moved_to(k, (doc, &out.doc), multi, checker.moves).map_err(&during)?
                        }
                        None => None,
                    };
                    let at = target.clone().unwrap_or_else(|| key.clone());
                    if let Some(changes) = u.path_changes(false) {
                        let touches: Vec<Touch> = changes.into_iter().map(touch).collect();
                        let d = describe(&Document::from(doc), &out.doc, &touches, position);
                        checker.changes.borrow_mut().push((at.clone(), d));
                    } else if u.is_pipeline() {
                        // A pipeline has no operators: its description compares the documents,
                        // and one whose diff is as large as the document replaced it (014 FR-009).
                        if let Some(diff) = pipeline_diff(&Document::from(doc), &out.doc) {
                            checker.changes.borrow_mut().push((at.clone(), describe_diff(&diff)));
                        }
                    }
                    let new = stored_bytes(&out.doc, Stored::Update).map_err(&during)?;
                    checker.check(&new, Some(doc)).map_err(&during)?;
                    let written = match target {
                        None => checker.entries_at(Some(key), Some(doc), Some(&new), lookup),
                        Some(_) => moved_entries(checker, (key, &at), (doc, &new), lookup),
                    };
                    let (entries, gone) = written.map_err(&during)?;
                    puts.push(put(&at, new));
                    puts.extend(entries);
                    deletes.extend(gone);
                    r.n_modified += 1;
                }
                r.value = match image {
                    Image::None => None,
                    Image::Before => Some(Document::from(doc)),
                    Image::After => Some(out.doc),
                };
            }
        }
        if !multi {
            break;
        }
    }
    if let (0, Some(doc), Some(key)) = (r.n, upsert, keys.last()) {
        if lookup(key).is_some() {
            return Err(duplicate(rules, &id_of(doc)));
        }
        let during = executor_error(executor_of(image));
        let replacement = update.as_ref().is_some_and(|u| u.is_replacement());
        let filled = replacement.then(|| fill_timestamps(doc, ts)).flatten();
        let doc = filled.as_ref().unwrap_or(doc);
        let new = stored_bytes(doc, Stored::Upsert).map_err(&during)?;
        checker.check(&new, None).map_err(&during)?;
        let (entries, gone) =
            checker.entries_at(Some(key), None, Some(&new), lookup).map_err(&during)?;
        puts.push(put(key, new));
        puts.extend(entries);
        deletes.extend(gone);
        r.upserted = Some(id_of(doc));
        if image == Image::After {
            r.value = Some(doc.clone());
        }
    }
    Ok((puts, deletes, r))
}
