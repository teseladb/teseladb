//! The index entries of a document write (009 FR-003, T007): every secondary index of the
//! collection that is being built or is ready gets, with the document's own version and in the
//! same write, the entries the new version adds and loses the ones the old version had. The
//! definitions travel in the mutation ([`IndexRule`]); the leader that evaluates it computes the
//! entries with `teseladb-index`. The entries of a unique index (FR-004) go through the write's
//! [`UniqueGuard`] with the stored versions of their keys, which the leader reads
//! ([`unique_keys`]); a key another document holds is a `DuplicateKey`. An index with
//! `prepareUnique` refuses an entry whose values another document's entry has: the write asks for
//! a probe of those values ([`probe_error`]), the command reads the entries under their prefix and
//! the version of their guard ([`probe`]) and runs the write again with them in its rules.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_collation::{Collation, ParsedCollation, parse_collation};
use teseladb_index::delta::{IndexSet, Version};
use teseladb_index::keygen::KeygenError;
use teseladb_index::prepared::{PreparedError, PreparedGuard, Probe, ProbeRequest};
use teseladb_index::spec::index_def;
use teseladb_index::unique::{UniqueError, UniqueGuard, conflicting_values, duplicate_key};
use teseladb_keys::{KeyParts, PrimaryKey};
use teseladb_range::command::Put;
use teseladb_range::read::ReadConcern;

use super::Backend;
use crate::error::{self, CommandError, DUPLICATE_KEY, INTERNAL_ERROR, WRITE_CONFLICT};

/// Value type of an index entry (a document is [`super::mutation::DOCUMENT`]).
pub const INDEX_ENTRY: u8 = 2;

/// Value type of the mark that an index is multikey on a path (009 FR-001, 010 FR-001): one
/// entry per field and path through an array, which the planner reads.
pub const MULTIKEY_MARK: u8 = 4;

/// The index numbers the marks of index `n` go under (`MULTIKEY_MARKS + n`), outside every index
/// and the documents.
pub const MULTIKEY_MARKS: u32 = 0x8000_0000;

/// Field of the specification with the index's own collation.
const COLLATION: &str = "collation";
/// Field of the specification with the filter of a partial index (FR-008).
const PARTIAL: &str = "partialFilterExpression";
/// Fields of the specification a `DuplicateKey` names.
const NAME: &str = "name";
const KEY: &str = "key";
const UNIQUE: &str = "unique";
const PREPARE_UNIQUE: &str = "prepareUnique";
/// The start of the message of a write that asks for probes; the requests follow, each as
/// `guard/prefix/limit` in hexadecimal, separated by `;`.
const PROBE_MARK: &str = "prepared index probe: ";
/// Field of an encoded rule that says the index is delete-only.
const DELETE_ONLY: &str = "deleteOnly";
/// Value type of a guard of a prepared index.
pub const GUARD: u8 = 3;
/// Fields of an encoded probe.
const PROBE_GUARD: &str = "g";
const PROBE_VERSION: &str = "v";
const PROBE_FOUND: &str = "f";
const PROBE_LIMIT: &str = "l";

/// A secondary index the write keeps: its number, its specification and its buckets.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexRule {
    /// Number of the index in the catalog (above 0).
    pub id: u32,
    /// Specification (`key` and options).
    pub spec: Document,
    /// Hash buckets (D-008).
    pub buckets: u8,
    /// The index is delete-only (009 FR-006): the write removes the entries of the version it
    /// replaces and adds none.
    pub delete_only: bool,
}

/// One index of a [`Maintainer`].
struct Kept {
    /// The index alone, as a set.
    set: IndexSet,
    /// Its collation.
    collation: Option<Collation>,
    /// Its name and key pattern, for a `DuplicateKey`.
    name: String,
    pattern: Document,
    /// Unique (FR-004).
    unique: bool,
    /// `prepareUnique` on an index that is not unique yet.
    prepared: bool,
    /// The filter of a partial index: only the documents that match it have entries.
    partial: Option<teseladb_mql::Filter>,
    /// Delete-only (009 FR-006): entries go, none come.
    delete_only: bool,
}

/// The indexes of one collection, ready to give the entries of a version.
pub struct Maintainer {
    /// Namespace `db.coll`, for a `DuplicateKey`.
    ns: String,
    /// Each index.
    sets: Vec<Kept>,
    /// The probes of the write's prepared indexes.
    probes: Vec<Probe>,
    /// Whether the prepared indexes refuse duplicates (not in an index build's backfill).
    enforce: bool,
    /// The entries of prepared indexes the write's statements added and removed.
    prepared: std::cell::RefCell<PreparedGuard>,
    /// The probes the write asked for.
    requests: std::cell::RefCell<Vec<ProbeRequest>>,
}

/// The keys among `puts` that are entries of a unique index of `rules`: the leader reads them
/// before the write goes ahead.
pub fn unique_keys(rules: &[IndexRule], puts: &[Put]) -> Vec<Vec<u8>> {
    let unique = |id: u32| {
        rules.iter().any(|r| r.id == id && matches!(r.spec.get(UNIQUE), Some(Value::Boolean(true))))
    };
    puts.iter()
        .filter(|p| p.value_type == INDEX_ENTRY)
        .filter(|p| match KeyParts::of(&p.key) {
            Ok(KeyParts::Data { index, .. }) => unique(index),
            _ => false,
        })
        .map(|p| p.key.clone())
        .collect()
}

/// The collation of index specification `spec`, `None` when it compares binary.
pub fn collation_of(spec: RawDoc<'_>) -> Result<Option<Collation>, CommandError> {
    Ok(match spec.get(COLLATION) {
        Some(RawValue::Document(c)) => match parse_collation(c)? {
            ParsedCollation::Simple => None,
            ParsedCollation::Collation(c, _) => Some(c),
        },
        _ => None,
    })
}

/// The error RefDb answers for a document an index refuses.
pub fn keygen_error(e: &KeygenError, doc: RawDoc<'_>) -> CommandError {
    let n = e.code();
    let code = error::code(n).unwrap_or(INTERNAL_ERROR);
    let mut out = CommandError::new(code, e.message(doc));
    if let Some(info) = e.err_info() {
        out.extra.push("errInfo", info);
    }
    out
}

fn malformed() -> CommandError {
    CommandError::new(INTERNAL_ERROR, "malformed index rules")
}

impl Maintainer {
    /// The indexes `rules` of collection `collection` of tenant `tenant` of namespace `ns`;
    /// `text` indexes have no entries here (011 keeps them).
    pub fn new(
        ns: &str,
        (tenant, collection): (u32, u32),
        rules: &[IndexRule],
    ) -> Result<Maintainer, CommandError> {
        let mut sets = Vec::new();
        for r in rules {
            let bytes = r.spec.to_bytes()?;
            let spec = RawDoc::from_bytes(&bytes)?;
            let Some(def) = index_def(spec, r.buckets).map_err(|_| malformed())? else { continue };
            let collation = collation_of(spec)?;
            let unique = def.unique;
            // A stored specification has `prepareUnique: true` or none (`createIndexes`).
            let prepared = !unique && spec.get(PREPARE_UNIQUE) == Some(RawValue::Boolean(true));
            let set = IndexSet { tenant, collection, indexes: vec![(r.id, def)] };
            let name = match spec.get(NAME) {
                Some(RawValue::String(n)) => n.to_string(),
                _ => return Err(malformed()),
            };
            let pattern = match spec.get(KEY) {
                Some(RawValue::Document(k)) => Document::from(k),
                _ => return Err(malformed()),
            };
            let partial = match spec.get(PARTIAL) {
                Some(RawValue::Document(f)) => Some(
                    teseladb_mql::Filter::parse(f, teseladb_mql::FilterUse::Index)
                        .map_err(super::mutation::from_mql)?,
                ),
                _ => None,
            };
            let delete_only = r.delete_only;
            sets.push(Kept {
                set,
                collation,
                name,
                pattern,
                unique,
                prepared,
                partial,
                delete_only,
            });
        }
        Ok(Maintainer {
            ns: ns.to_string(),
            sets,
            probes: Vec::new(),
            enforce: true,
            prepared: std::cell::RefCell::default(),
            requests: std::cell::RefCell::default(),
        })
    }

    /// The maintainer with the write's `probes`; with `enforce` false (an index build's
    /// backfill) its prepared indexes take duplicates, as a `refrouter` 9.0.4 builds them.
    pub fn with_probes(self, probes: Vec<Probe>, enforce: bool) -> Maintainer {
        Maintainer { probes, enforce, ..self }
    }

    /// The error that asks for the probes the write needs, if it needs any.
    pub fn probe_error(&self) -> Option<CommandError> {
        let requests = self.requests.borrow();
        (!requests.is_empty()).then(|| probe_error(&requests))
    }

    /// The entries to write and the keys to delete when version `old` becomes `new` (`None`:
    /// no document); an index that refuses `new` fails the write, and so does a unique key that
    /// another document holds (`stored`, the versions the leader read) or that an earlier
    /// statement of the write took (`guard`). The entries end in the primary key `pk`, or in the
    /// document's `_id` without one.
    pub fn changes<'a, 's>(
        &self,
        old: Option<RawDoc<'a>>,
        new: Option<RawDoc<'a>>,
        pk: Option<&PrimaryKey>,
        guard: &mut UniqueGuard,
        stored: &dyn Fn(&[u8]) -> Option<&'s [u8]>,
    ) -> Result<(Vec<Put>, Vec<Vec<u8>>), CommandError> {
        let (mut puts, mut deletes) = (Vec::new(), Vec::new());
        if self.sets.is_empty() {
            return Ok((puts, deletes));
        }
        let id = |d: RawDoc<'_>| {
            pk.cloned().unwrap_or_else(|| PrimaryKey::id(d.get("_id").unwrap_or(RawValue::Null)))
        };
        let old_pk = old.map(id);
        let new_pk = new.map(id);
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        for kept in &self.sets {
            let collator = kept.collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator);
            // A version a partial index's filter leaves out has no entries there.
            let held = |doc: Option<RawDoc<'a>>| -> Result<Option<RawDoc<'a>>, CommandError> {
                let (Some(d), Some(f)) = (doc, &kept.partial) else { return Ok(doc) };
                let ctx = teseladb_mql::MatchContext {
                    collation: collator,
                    ext: &super::engines::NodeExtensions,
                    vars: None,
                };
                let matches = f.matches(d, &ctx).map_err(super::mutation::from_mql)?;
                Ok(matches.then_some(d))
            };
            let before = held(old)?.zip(old_pk.as_ref()).map(|(doc, pk)| Version { doc, pk });
            let after = held(new)?.zip(new_pk.as_ref()).map(|(doc, pk)| Version { doc, pk });
            let delta = kept.set.delta(before, after, collator).map_err(|e| {
                let doc = new.or(old).unwrap_or_else(|| empty());
                keygen_error(&e, doc)
            })?;
            let gone: Vec<Vec<u8>> = delta.remove.iter().map(|k| k.as_bytes().to_vec()).collect();
            if kept.delete_only {
                deletes.extend(gone);
                continue;
            }
            let pk = new_pk.as_ref().map_or(&[][..], PrimaryKey::as_bytes);
            if kept.unique {
                removed.extend(gone.iter().cloned());
                added.extend(delta.add.iter().map(|(k, _)| (k.as_bytes().to_vec(), pk.to_vec())));
            }
            if kept.prepared && self.enforce {
                let entries: Vec<(Vec<u8>, Vec<u8>)> = delta
                    .add
                    .iter()
                    .map(|(k, _)| {
                        let k = k.as_bytes();
                        (k[..k.len().saturating_sub(pk.len())].to_vec(), k.to_vec())
                    })
                    .collect();
                let raised = self
                    .prepared
                    .borrow_mut()
                    .admit(&gone, &entries, &self.probes, stored)
                    .map_err(|e| self.refused_prepared(kept, e, new))?;
                puts.extend(raised.into_iter().map(|(key, value)| Put {
                    key,
                    value_type: GUARD,
                    value,
                }));
            }
            deletes.extend(gone);
            puts.extend(delta.add.into_iter().map(|(k, value)| Put {
                key: k.as_bytes().to_vec(),
                value_type: INDEX_ENTRY,
                value,
            }));
            if let Some(v) = after {
                puts.extend(multikey_marks(&kept.set, &kept.pattern, v.doc));
            }
        }
        guard.admit(&removed, &added, stored).map_err(|e| self.refused(e, new))?;
        Ok((puts, deletes))
    }

    /// The error of an entry of the prepared index `kept` that version `new` cannot add; a
    /// probe the write needs is recorded for [`Maintainer::probe_error`].
    fn refused_prepared(
        &self,
        kept: &Kept,
        e: PreparedError,
        new: Option<RawDoc<'_>>,
    ) -> CommandError {
        match e {
            PreparedError::Probe(r) => {
                let mut requests = self.requests.borrow_mut();
                if !requests.iter().any(|q| q.guard == r.guard) {
                    requests.push(r);
                }
                probe_error(&requests)
            }
            PreparedError::Duplicate { prefix } => self.duplicate(kept, &prefix, new),
            PreparedError::Malformed(why) => {
                CommandError::new(INTERNAL_ERROR, format!("prepared index: {why}"))
            }
        }
    }

    /// The `DuplicateKey` of index `kept` for the entry of `new` whose key (or prefix) is `key`.
    fn duplicate(&self, kept: &Kept, key: &[u8], new: Option<RawDoc<'_>>) -> CommandError {
        let index = kept.set.indexes.first().map_or(0, |(i, _)| *i);
        let collator = kept.collation.as_ref().map(|c| c as &dyn teseladb_bson::Collator);
        let doc = match new {
            Some(d) => d,
            None => empty(),
        };
        let built = conflicting_values(&kept.set, index, doc, key, collator)
            .map_err(|e| keygen_error(&e, doc))
            .and_then(|values| {
                duplicate_key(&self.ns, &kept.name, &kept.pattern, &values).map_err(Into::into)
            });
        match built {
            Ok(d) => {
                let mut out = CommandError::new(DUPLICATE_KEY, d.message);
                out.extra.push("keyPattern", d.key_pattern);
                out.extra.push("keyValue", d.key_value);
                out
            }
            Err(e) => e,
        }
    }

    /// The error of a unique key the guard refused for version `new`.
    fn refused(&self, e: UniqueError, new: Option<RawDoc<'_>>) -> CommandError {
        let key = match e {
            UniqueError::Duplicate { key } => key,
            UniqueError::Malformed(k) => {
                return CommandError::new(INTERNAL_ERROR, format!("stored unique entry: {k:?}"));
            }
        };
        let index = match KeyParts::of(&key) {
            Ok(KeyParts::Data { index, .. }) => index,
            _ => return malformed(),
        };
        let Some(kept) = self.sets.iter().find(|k| k.set.indexes.iter().any(|(i, _)| *i == index))
        else {
            return malformed();
        };
        self.duplicate(kept, &key, new)
    }
}

/// The empty document, for an error without a document.
/// The marks of the paths of `doc` that go through an array on the fields of the index of
/// `set` (key pattern `pattern`).
fn multikey_marks(set: &IndexSet, pattern: &Document, doc: RawDoc<'_>) -> Vec<Put> {
    let Some((id, _)) = set.indexes.first() else { return Vec::new() };
    let mut out = Vec::new();
    for (field, _) in pattern.iter() {
        let parts: Vec<&str> = field.split('.').collect();
        let mut prefixes = Vec::new();
        array_prefixes(RawValue::Document(doc), &parts, 0, &mut prefixes);
        prefixes.sort_unstable();
        prefixes.dedup();
        for n in prefixes {
            out.push(Put {
                key: multikey_key(set.tenant, set.collection, *id, field, &parts[..n].join(".")),
                value_type: MULTIKEY_MARK,
                value: Vec::new(),
            });
        }
    }
    out
}

/// The key of the mark that the index `id` is multikey on `field` through the array at `path`.
pub fn multikey_key(tenant: u32, collection: u32, id: u32, field: &str, path: &str) -> Vec<u8> {
    let mut key = teseladb_keys::KeyBuilder::index_prefix(tenant, collection, MULTIKEY_MARKS + id)
        .as_bytes()
        .to_vec();
    key.extend_from_slice(field.as_bytes());
    key.push(0);
    key.extend_from_slice(path.as_bytes());
    key
}

/// The lengths of the prefixes of `parts` (from `at`) whose value in `v` is an array.
fn array_prefixes(v: RawValue<'_>, parts: &[&str], at: usize, out: &mut Vec<usize>) {
    match v {
        RawValue::Array(a) => {
            out.push(at);
            if at < parts.len() {
                for item in a.iter() {
                    if let RawValue::Document(_) = item {
                        array_prefixes(item, parts, at, out);
                    }
                }
            }
        }
        RawValue::Document(d) if at < parts.len() => {
            if let Some(next) = d.get(parts[at]) {
                array_prefixes(next, parts, at + 1, out);
            }
        }
        _ => {}
    }
}

fn empty() -> RawDoc<'static> {
    const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];
    RawDoc::from_bytes(&EMPTY).expect("the empty document")
}

/// The error that asks the command for the probes `requests`.
pub fn probe_error(requests: &[ProbeRequest]) -> CommandError {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let items: Vec<String> = requests
        .iter()
        .map(|r| format!("{}/{}/{}", hex(&r.guard), hex(&r.prefix), r.limit))
        .collect();
    CommandError::new(WRITE_CONFLICT, format!("{PROBE_MARK}{}", items.join(";")))
}

/// The probes a write's error asks for, if it asks for any.
pub fn probe_requests(e: &CommandError) -> Option<Vec<ProbeRequest>> {
    if e.code.code != WRITE_CONFLICT.code {
        return None;
    }
    let unhex = |s: &str| -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) {
            return None;
        }
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
    };
    e.errmsg
        .strip_prefix(PROBE_MARK)?
        .split(';')
        .map(|item| {
            let mut parts = item.split('/');
            let guard = unhex(parts.next()?)?;
            let prefix = unhex(parts.next()?)?;
            let limit = parts.next()?.parse().ok()?;
            parts.next().is_none().then_some(ProbeRequest { guard, prefix, limit })
        })
        .collect()
}

/// Reads what `request` asks for: the guard's version first, then the entries under the prefix,
/// so that an entry added after the guard was read raised it and the write probes again.
pub fn probe<B: Backend + ?Sized>(
    backend: &mut B,
    request: &ProbeRequest,
) -> Result<Probe, CommandError> {
    let after_guard = [request.guard.as_slice(), &[0]].concat();
    let version = match backend
        .scan(&request.guard, &after_guard, 1, ReadConcern::Local)?
        .into_iter()
        .find(|(k, _)| k == &request.guard)
    {
        Some((_, v)) => Some(
            teseladb_index::prepared::version_of(&v)
                .map_err(|why| CommandError::new(INTERNAL_ERROR, why))?,
        ),
        None => None,
    };
    let end = teseladb_keys::next_prefix(&request.prefix)
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "an index prefix without end"))?;
    let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
    let found = backend
        .scan(&request.prefix, &end, limit, ReadConcern::Local)?
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    Ok(Probe { guard: request.guard.clone(), version, found, limit: request.limit })
}

/// The keys of `probes` a write latches and reads: each guard and the entries it found.
pub fn probe_keys(probes: &[Probe]) -> Vec<Vec<u8>> {
    probes.iter().flat_map(|p| std::iter::once(p.guard.clone()).chain(p.found.clone())).collect()
}

/// The probes as they travel in a mutation.
pub fn probes_value(probes: &[Probe]) -> Value {
    let bin = |b: &[u8]| Value::Binary { subtype: 0, bytes: b.to_vec() };
    let items = probes
        .iter()
        .map(|p| {
            let mut d = Document::new().with(PROBE_GUARD, bin(&p.guard));
            if let Some(v) = p.version {
                d.push(PROBE_VERSION, Value::Int64(i64::from_be_bytes(v.to_be_bytes())));
            }
            let found: Vec<Value> = p.found.iter().map(|k| bin(k)).collect();
            Value::Document(d.with(PROBE_FOUND, found).with(PROBE_LIMIT, i64::from(p.limit)))
        })
        .collect::<Vec<_>>();
    Value::Array(items)
}

/// The probes of a mutation.
pub fn read_probes(v: Option<RawValue<'_>>) -> Result<Vec<Probe>, CommandError> {
    let Some(RawValue::Array(items)) = v else { return Ok(Vec::new()) };
    let bin = |v: Option<RawValue<'_>>| match v {
        Some(RawValue::Binary { bytes, .. }) => Ok(bytes.to_vec()),
        _ => Err(malformed()),
    };
    items
        .iter()
        .map(|item| {
            let RawValue::Document(d) = item else { return Err(malformed()) };
            let version = match d.get(PROBE_VERSION) {
                Some(RawValue::Int64(n)) => Some(u64::from_be_bytes(n.to_be_bytes())),
                None => None,
                Some(_) => return Err(malformed()),
            };
            let Some(RawValue::Array(found)) = d.get(PROBE_FOUND) else { return Err(malformed()) };
            let found = found.iter().map(|k| bin(Some(k))).collect::<Result<_, _>>()?;
            let limit = match d.get(PROBE_LIMIT) {
                Some(RawValue::Int64(n)) => u32::try_from(n).map_err(|_| malformed())?,
                _ => return Err(malformed()),
            };
            Ok(Probe { guard: bin(d.get(PROBE_GUARD))?, version, found, limit })
        })
        .collect()
}

/// The rules as they travel in a mutation.
pub fn rules_value(rules: &[IndexRule]) -> Value {
    let items = rules
        .iter()
        .map(|r| {
            Value::Document(
                Document::new()
                    .with("id", i64::from(r.id))
                    .with("spec", r.spec.clone())
                    .with("buckets", i32::from(r.buckets))
                    .with(DELETE_ONLY, r.delete_only),
            )
        })
        .collect::<Vec<_>>();
    Value::Array(items)
}

/// The rules of a mutation.
pub fn read_rules(v: Option<RawValue<'_>>) -> Result<Vec<IndexRule>, CommandError> {
    let Some(RawValue::Array(items)) = v else { return Ok(Vec::new()) };
    items
        .iter()
        .map(|item| {
            let RawValue::Document(d) = item else { return Err(malformed()) };
            let id = match d.get("id") {
                Some(RawValue::Int64(n)) => u32::try_from(n).map_err(|_| malformed())?,
                _ => return Err(malformed()),
            };
            let spec = match d.get("spec") {
                Some(RawValue::Document(s)) => Document::from(s),
                _ => return Err(malformed()),
            };
            let buckets = match d.get("buckets") {
                Some(RawValue::Int32(n)) => u8::try_from(n).map_err(|_| malformed())?,
                _ => return Err(malformed()),
            };
            let delete_only = match d.get(DELETE_ONLY) {
                Some(RawValue::Boolean(b)) => b,
                None => false,
                Some(_) => return Err(malformed()),
            };
            Ok(IndexRule { id, spec, buckets, delete_only })
        })
        .collect()
}
