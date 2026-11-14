//! The catalog changes change streams read (014 FR-014): each creation, drop, rename and
//! modification of a collection, each drop of a database and each index that becomes ready, is
//! dropped or changes, with the mark of the Group 0 entry that made it and its position in that
//! entry. A stream merges them with the documents' changes by mark. The log keeps what fits in
//! [`MAX_BYTES`]; a stream that would read below what it dropped has lost its history.

use std::collections::VecDeque;

use teseladb_hlc::Hlc;
use teseladb_types::{DecodeError, Decoder, Encoder};

use crate::catalog::{
    Catalog, CatalogCommand, CollectionEntry, Namespace, get_ns, get_uuid, put_ns,
};
use crate::indexes::{IndexCommand, IndexState, Indexes};

/// Bytes of changes the log keeps (a catalog change takes some hundred bytes: tens of thousands
/// of them, far more than a day of DDL).
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

/// A change of the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DdlChange {
    /// A collection or a view was created.
    Create {
        /// Namespace.
        ns: Namespace,
        /// The collection created.
        collection: CollectionEntry,
    },
    /// A collection was dropped.
    Drop {
        /// Namespace.
        ns: Namespace,
        /// The collection dropped.
        collection: CollectionEntry,
    },
    /// A collection was renamed.
    Rename {
        /// Source.
        from: Namespace,
        /// Target.
        to: Namespace,
        /// The collection, as it was before.
        collection: CollectionEntry,
        /// The collection at `to` the rename dropped (`dropTarget`).
        target: Option<CollectionEntry>,
    },
    /// A collection's options changed (`collMod`).
    Modify {
        /// Namespace.
        ns: Namespace,
        /// The collection, as it was before.
        collection: CollectionEntry,
        /// What `collMod` asked for, as a BSON document.
        description: Vec<u8>,
    },
    /// A database was dropped (after the drops of its collections).
    DropDatabase {
        /// Database.
        db: String,
    },
    /// An index became ready.
    CreateIndex {
        /// Namespace.
        ns: Namespace,
        /// Its collection.
        collection: CollectionEntry,
        /// Its specification.
        spec: Vec<u8>,
    },
    /// A ready index was dropped.
    DropIndex {
        /// Namespace.
        ns: Namespace,
        /// Its collection.
        collection: CollectionEntry,
        /// Its specification.
        spec: Vec<u8>,
    },
    /// A ready index's options changed (`collMod` of an index).
    ModifyIndex {
        /// Namespace.
        ns: Namespace,
        /// Its collection.
        collection: CollectionEntry,
        /// The specification before.
        before: Vec<u8>,
        /// The specification after.
        after: Vec<u8>,
    },
}

/// A change with its place among the changes of the cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DdlRecord {
    /// The mark of the Group 0 entry that made it.
    pub mark: Hlc,
    /// Its position among the changes of that entry.
    pub op: u32,
    /// What changed.
    pub change: DdlChange,
}

/// The catalog changes kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DdlLog {
    /// The changes, oldest first.
    pub records: VecDeque<DdlRecord>,
    /// The mark of the newest change dropped: below or at it the log is not complete.
    pub floor: Hlc,
    /// The mark of the last Group 0 entry applied (`None` until one is, after a snapshot of a
    /// version before it). Group 0's leader gives its entries growing marks, so no change at or
    /// below it appears later on this copy.
    pub applied_mark: Option<Hlc>,
    bytes: u64,
}

impl Default for DdlLog {
    fn default() -> DdlLog {
        let floor = Hlc { physical: 0, logical: 0 };
        DdlLog { records: VecDeque::new(), floor, applied_mark: None, bytes: 0 }
    }
}

const CREATE: u8 = 1;
const DROP: u8 = 2;
const RENAME: u8 = 3;
const MODIFY: u8 = 4;
const DROP_DATABASE: u8 = 5;
const CREATE_INDEX: u8 = 6;
const DROP_INDEX: u8 = 7;
const MODIFY_INDEX: u8 = 8;

fn put_collection(e: &mut Encoder, c: &CollectionEntry) {
    e.u32(c.id).raw(&c.uuid).bytes(&c.options).u64(c.created_index).u32(c.tenant);
}

fn get_collection(d: &mut Decoder<'_>) -> Result<CollectionEntry, DecodeError> {
    Ok(CollectionEntry {
        id: d.u32()?,
        uuid: get_uuid(d)?,
        options: d.bytes()?.to_vec(),
        created_index: d.u64()?,
        tenant: d.u32()?,
    })
}

fn put_mark(e: &mut Encoder, m: Hlc) {
    e.u64(m.physical).u32(m.logical);
}

fn get_mark(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

fn encode_record(e: &mut Encoder, r: &DdlRecord) {
    put_mark(e, r.mark);
    e.u32(r.op);
    match &r.change {
        DdlChange::Create { ns, collection } => {
            e.u8(CREATE);
            put_ns(e, ns);
            put_collection(e, collection);
        }
        DdlChange::Drop { ns, collection } => {
            e.u8(DROP);
            put_ns(e, ns);
            put_collection(e, collection);
        }
        DdlChange::Rename { from, to, collection, target } => {
            e.u8(RENAME);
            put_ns(e, from);
            put_ns(e, to);
            put_collection(e, collection);
            e.bool(target.is_some());
            if let Some(t) = target {
                put_collection(e, t);
            }
        }
        DdlChange::Modify { ns, collection, description } => {
            e.u8(MODIFY);
            put_ns(e, ns);
            put_collection(e, collection);
            e.bytes(description);
        }
        DdlChange::DropDatabase { db } => {
            e.u8(DROP_DATABASE).str(db);
        }
        DdlChange::CreateIndex { ns, collection, spec }
        | DdlChange::DropIndex { ns, collection, spec } => {
            let kind = if matches!(r.change, DdlChange::CreateIndex { .. }) {
                CREATE_INDEX
            } else {
                DROP_INDEX
            };
            e.u8(kind);
            put_ns(e, ns);
            put_collection(e, collection);
            e.bytes(spec);
        }
        DdlChange::ModifyIndex { ns, collection, before, after } => {
            e.u8(MODIFY_INDEX);
            put_ns(e, ns);
            put_collection(e, collection);
            e.bytes(before).bytes(after);
        }
    }
}

fn decode_record(d: &mut Decoder<'_>) -> Result<DdlRecord, DecodeError> {
    let mark = get_mark(d)?;
    let op = d.u32()?;
    let at = d.position();
    let change = match d.u8()? {
        CREATE => DdlChange::Create { ns: get_ns(d)?, collection: get_collection(d)? },
        DROP => DdlChange::Drop { ns: get_ns(d)?, collection: get_collection(d)? },
        RENAME => DdlChange::Rename {
            from: get_ns(d)?,
            to: get_ns(d)?,
            collection: get_collection(d)?,
            target: if d.bool()? { Some(get_collection(d)?) } else { None },
        },
        MODIFY => DdlChange::Modify {
            ns: get_ns(d)?,
            collection: get_collection(d)?,
            description: d.bytes()?.to_vec(),
        },
        DROP_DATABASE => DdlChange::DropDatabase { db: d.str()?.to_owned() },
        CREATE_INDEX => DdlChange::CreateIndex {
            ns: get_ns(d)?,
            collection: get_collection(d)?,
            spec: d.bytes()?.to_vec(),
        },
        DROP_INDEX => DdlChange::DropIndex {
            ns: get_ns(d)?,
            collection: get_collection(d)?,
            spec: d.bytes()?.to_vec(),
        },
        MODIFY_INDEX => DdlChange::ModifyIndex {
            ns: get_ns(d)?,
            collection: get_collection(d)?,
            before: d.bytes()?.to_vec(),
            after: d.bytes()?.to_vec(),
        },
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    };
    Ok(DdlRecord { mark, op, change })
}

impl DdlRecord {
    /// The bytes the log keeps for it: its encoding.
    pub fn size(&self) -> u64 {
        let mut e = Encoder::new();
        encode_record(&mut e, self);
        e.len() as u64
    }
}

impl DdlLog {
    /// The bytes of the changes kept.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Records the changes of the entry at `mark`, in order; the oldest ones go while the bytes
    /// kept pass [`MAX_BYTES`].
    pub fn record(&mut self, mark: Hlc, changes: Vec<DdlChange>) {
        for (op, change) in changes.into_iter().enumerate() {
            let r = DdlRecord { mark, op: u32::try_from(op).unwrap_or(u32::MAX), change };
            self.bytes += r.size();
            self.records.push_back(r);
        }
        while self.bytes > MAX_BYTES {
            let Some(old) = self.records.pop_front() else { break };
            self.bytes -= old.size();
            self.floor = self.floor.max(old.mark);
        }
    }

    /// Records `changes` of the entry at `mark` whose command applied; `created` is the collection
    /// it created or found, which a creation takes.
    pub fn record_applied(
        &mut self,
        mark: Hlc,
        mut changes: Vec<DdlChange>,
        created: Option<&CollectionEntry>,
    ) {
        if changes.is_empty() {
            return;
        }
        if let (Some(DdlChange::Create { collection, .. }), Some(c)) =
            (changes.first_mut(), created)
        {
            *collection = c.clone();
        }
        self.record(mark, changes);
    }

    /// The changes above mark `from`.
    pub fn after(&self, from: Hlc) -> Vec<DdlRecord> {
        let start = self.records.partition_point(|r| r.mark <= from);
        self.records.range(start..).cloned().collect()
    }

    /// Writes it into `e`.
    pub fn encode(&self, e: &mut Encoder) {
        put_mark(e, self.floor);
        e.bool(self.applied_mark.is_some());
        put_mark(e, self.applied_mark.unwrap_or(self.floor));
        e.var(self.records.len() as u64);
        for r in &self.records {
            encode_record(e, r);
        }
    }

    /// Reads what [`DdlLog::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<DdlLog, DecodeError> {
        let floor = get_mark(d)?;
        let known = d.bool()?;
        let applied = get_mark(d)?;
        let n = d.var()?;
        let applied_mark = known.then_some(applied);
        let mut log = DdlLog { floor, applied_mark, ..DdlLog::default() };
        for _ in 0..n {
            let r = decode_record(d)?;
            log.bytes += r.size();
            log.records.push_back(r);
        }
        Ok(log)
    }
}

/// The changes catalog command `c` makes, read before it applies (014 FR-014). A creation
/// carries the collection it finds now (none yet): [`DdlLog::record_applied`] puts the new one.
pub fn catalog_changes(cat: &Catalog, c: &CatalogCommand) -> Vec<DdlChange> {
    let entry = |ns: &Namespace| cat.get(ns).cloned();
    match c {
        CatalogCommand::Create { ns, .. } if entry(ns).is_none() => {
            let collection = CollectionEntry {
                id: 0,
                uuid: [0; 16],
                options: Vec::new(),
                created_index: 0,
                tenant: 0,
            };
            vec![DdlChange::Create { ns: ns.clone(), collection }]
        }
        CatalogCommand::Drop { ns } => entry(ns)
            .map(|collection| DdlChange::Drop { ns: ns.clone(), collection })
            .into_iter()
            .collect(),
        CatalogCommand::Rename { from, to, drop_target, .. } => {
            // A dropped target gives no `drop`: the `rename` names it (measured on 01-11).
            let Some(collection) = entry(from) else { return Vec::new() };
            let target = entry(to).filter(|_| *drop_target);
            vec![DdlChange::Rename { from: from.clone(), to: to.clone(), collection, target }]
        }
        CatalogCommand::Modify { ns, description, .. } => entry(ns)
            .map(|collection| DdlChange::Modify {
                ns: ns.clone(),
                collection,
                description: description.clone(),
            })
            .into_iter()
            .collect(),
        CatalogCommand::DropDatabase { db } => {
            // Its views go without a `drop` (measured on 01-11).
            let is_view = |c: &CollectionEntry| {
                teseladb_bson::RawDoc::from_bytes(&c.options)
                    .is_ok_and(|o| o.get(crate::collection_types::VIEW_ON).is_some())
            };
            let mut v: Vec<DdlChange> = cat
                .in_db(db)
                .into_iter()
                .filter(|(_, c)| !is_view(c))
                .map(|(ns, c)| DdlChange::Drop { ns: ns.clone(), collection: c.clone() })
                .collect();
            if !v.is_empty() || cat.dbs.contains_key(db) {
                v.push(DdlChange::DropDatabase { db: db.clone() });
            }
            v
        }
        CatalogCommand::Create { .. }
        | CatalogCommand::Purged { .. }
        | CatalogCommand::AddFieldNames { .. }
        | CatalogCommand::CreateDatabase { .. } => Vec::new(),
    }
}

/// The changes index command `c` makes to the indexes of a live collection, read before it
/// applies: an index that becomes ready, a ready one dropped or with new options.
pub fn index_changes(cat: &Catalog, indexes: &Indexes, c: &IndexCommand) -> Vec<DdlChange> {
    let of = |id: &u32| {
        let e = indexes.indexes.get(id)?;
        let (ns, coll) = cat.collections.iter().find(|(_, c)| c.id == e.collection)?;
        Some((e, ns.clone(), coll.clone()))
    };
    let change = match c {
        IndexCommand::SetState { id, state } => {
            of(id).and_then(|(e, ns, collection)| match (e.state, *state) {
                (IndexState::Ready, IndexState::Ready) => None,
                (_, IndexState::Ready) => {
                    Some(DdlChange::CreateIndex { ns, collection, spec: e.spec.clone() })
                }
                (IndexState::Ready, _) => {
                    Some(DdlChange::DropIndex { ns, collection, spec: e.spec.clone() })
                }
                _ => None,
            })
        }
        IndexCommand::Drop { id } => {
            of(id).filter(|(e, ..)| e.state == IndexState::Ready).map(|(e, ns, collection)| {
                DdlChange::DropIndex { ns, collection, spec: e.spec.clone() }
            })
        }
        IndexCommand::SetSpec { id, spec } => of(id)
            .filter(|(e, ..)| e.state == IndexState::Ready)
            .map(|(e, ns, collection)| DdlChange::ModifyIndex {
                ns,
                collection,
                before: e.spec.clone(),
                after: spec.clone(),
            }),
        _ => None,
    };
    change.into_iter().collect()
}
