//! The catalog events of a change stream (014 FR-002, FR-014): each change Group 0 recorded
//! becomes the event a `refrouter` 9.0.4 gives (measured on 01-11): `create` with the options and
//! the `_id` index (a view with `viewOn` and `pipeline`, no `collectionUUID` and `nsType: "view"`;
//! a time series gives none), `createIndexes` and `dropIndexes` with the index, `modify` with what
//! `collMod` asked for and the options before, `rename` with `to` (and `dropTarget`, the
//! collection it dropped), `drop` and `dropDatabase`.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_changestream::ddl::{ddl_key, index_change, shown};
use teseladb_changestream::event::{ChangeEvent, Namespace as EventNs};
use teseladb_changestream::stage::Scope;
use teseladb_changestream::token::{EventKind, ResumeToken};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::ddl_log::{DdlChange, DdlRecord};

use super::kinds;
use crate::error::CommandError;

/// BSON binary subtype of a UUID.
const UUID_SUBTYPE: u8 = 4;
/// `nsType` of a collection.
const NS_COLLECTION: &str = "collection";
/// `nsType` of a view.
const NS_VIEW: &str = "view";

/// What a catalog event needs of its stream: what it watches and `showExpandedEvents`.
pub struct Watch<'a> {
    /// Scope.
    pub scope: Scope,
    /// Database (empty for the cluster).
    pub db: &'a str,
    /// Collection, for a collection's stream.
    pub coll: Option<&'a str>,
    /// `showExpandedEvents`.
    pub expanded: bool,
}

fn uuid(u: [u8; 16]) -> Value {
    Value::Binary { subtype: UUID_SUBTYPE, bytes: u.to_vec() }
}

fn event_ns(ns: &Namespace) -> EventNs {
    EventNs { db: ns.db.clone(), coll: Some(ns.coll.clone()) }
}

fn doc(bytes: &[u8]) -> Result<Document, CommandError> {
    Ok(Document::from(RawDoc::from_bytes(bytes)?))
}

/// `stateBeforeChange.collectionOptions`: the collection's UUID, then its options.
fn options_before(c: &CollectionEntry) -> Result<Document, CommandError> {
    let mut d = Document::new().with("uuid", uuid(c.uuid));
    for (k, v) in super::catalog::options_of(c)?.iter() {
        d.push(k, v.clone());
    }
    Ok(d)
}

/// Whether namespace `ns` of a collection is one a stream of `w` reads.
fn in_watch(w: &Watch<'_>, ns: &Namespace) -> bool {
    if ns.coll.starts_with(super::change_stream::SYSTEM_PREFIX) {
        return false;
    }
    match w.scope {
        Scope::Collection => ns.db == w.db && Some(ns.coll.as_str()) == w.coll,
        Scope::Database => ns.db == w.db,
        Scope::Cluster => !super::change_stream::INTERNAL_DBS.contains(&ns.db.as_str()),
    }
}

/// The type of a collection's event, and whether a time series gives none.
/// The description `shardCollection` writes with its catalog change: what the
/// `shardCollection` event shows (measured on a `refrouter` 9.0.4 on 02-11).
pub fn shard_description(
    key: &Document,
    unique: bool,
    capped: bool,
) -> Result<Vec<u8>, CommandError> {
    let d = Document::new()
        .with(SHARD_KEY_FIELD, key.clone())
        .with("unique", unique)
        .with("presplitHashedZones", false)
        .with("capped", capped);
    Ok(d.to_bytes()?)
}

/// The field of a `shardCollection`'s description, which no `collMod` writes.
const SHARD_KEY_FIELD: &str = "shardKey";

/// Whether a catalog change's description is that of a `shardCollection`.
fn is_shard(description: &[u8]) -> bool {
    RawDoc::from_bytes(description).is_ok_and(|d| d.get(SHARD_KEY_FIELD).is_some())
}

fn kind_of(c: &CollectionEntry) -> Result<Option<CollectionKind>, CommandError> {
    match kinds::kind_of(c)? {
        CollectionKind::TimeSeries(_) => Ok(None),
        k => Ok(Some(k)),
    }
}

/// The event of record `r` a stream of `w` shows, if it shows one.
pub fn event_of(w: &Watch<'_>, r: &DdlRecord) -> Result<Option<ChangeEvent>, CommandError> {
    let key = ddl_key(r.mark, r.op);
    let token = |kind| ResumeToken::new(key.clone(), kind);
    // A resharding's own catalog changes (018 FR-002): its critical section gives no event and
    // its rename is the `reshardCollection` or `refineCollectionShardKey` of the collection.
    match &r.change {
        DdlChange::Modify { description, .. } if super::reshard::is_cutover_change(description) => {
            return Ok(None);
        }
        DdlChange::Rename { from, to, target, .. } => {
            if let Some((refine, uuid_of, desc)) =
                super::reshard::cutover_event(from, target.as_ref())?
            {
                let kind = if refine {
                    EventKind::RefineCollectionShardKey
                } else {
                    EventKind::ReshardCollection
                };
                if !in_watch(w, to) || !shown(kind, w.expanded) {
                    return Ok(None);
                }
                let mut e = ChangeEvent::catalog(token(kind), event_ns(to));
                e.collection_uuid = Some(uuid_of);
                e.operation_description = Some(desc);
                return Ok(Some(e));
            }
        }
        _ => {}
    }
    let (kind, ns) = match &r.change {
        DdlChange::Create { ns, .. } => (EventKind::Create, ns),
        DdlChange::Drop { ns, .. } => (EventKind::Drop, ns),
        DdlChange::Rename { from, .. } => (EventKind::Rename, from),
        DdlChange::Modify { ns, description, .. } if is_shard(description) => {
            (EventKind::ShardCollection, ns)
        }
        DdlChange::Modify { ns, .. } | DdlChange::ModifyIndex { ns, .. } => (EventKind::Modify, ns),
        DdlChange::CreateIndex { ns, .. } => (EventKind::CreateIndexes, ns),
        DdlChange::DropIndex { ns, .. } => (EventKind::DropIndexes, ns),
        DdlChange::DropDatabase { db } => {
            let shows = match w.scope {
                Scope::Collection => false,
                Scope::Database => db == w.db,
                Scope::Cluster => !super::change_stream::INTERNAL_DBS.contains(&db.as_str()),
            };
            let e = ChangeEvent::catalog(
                token(EventKind::DropDatabase),
                EventNs { db: db.clone(), coll: None },
            );
            return Ok(shows.then_some(e));
        }
    };
    let watched = match &r.change {
        // The target a rename dropped is watched too (measured: its stream sees the `rename`).
        DdlChange::Rename { to, target: Some(_), .. } => in_watch(w, ns) || in_watch(w, to),
        _ => in_watch(w, ns),
    };
    if !watched || !shown(kind, w.expanded) {
        return Ok(None);
    }
    let mut e = ChangeEvent::catalog(token(kind), event_ns(ns));
    match &r.change {
        DdlChange::Create { collection, .. } => {
            let Some(k) = kind_of(collection)? else { return Ok(None) };
            if let CollectionKind::View { .. } = k {
                let o = super::catalog::options_of(collection)?;
                let mut desc = Document::new();
                for f in ["viewOn", "pipeline", "collation"] {
                    if let Some(v) = o.get(f) {
                        desc.push(f, v.clone());
                    }
                }
                e.operation_description = Some(desc);
                e.ns_type = Some(NS_VIEW);
            } else {
                let mut desc = super::catalog::options_of(collection)?;
                desc.push("idIndex", super::catalog::id_index(collection)?);
                e.collection_uuid = Some(collection.uuid);
                e.operation_description = Some(desc);
                e.ns_type = Some(NS_COLLECTION);
            }
        }
        DdlChange::Drop { collection, .. } => {
            let Some(k) = kind_of(collection)? else { return Ok(None) };
            if !matches!(k, CollectionKind::View { .. }) {
                e.collection_uuid = Some(collection.uuid);
            }
        }
        DdlChange::Rename { to, collection, target, .. } => {
            let mut desc = Document::new().with("to", ns_document(to));
            if let Some(t) = target {
                desc.push("dropTarget", uuid(t.uuid));
            }
            e.to = Some(event_ns(to));
            e.collection_uuid = Some(collection.uuid);
            e.operation_description = Some(desc);
        }
        DdlChange::Modify { collection, description, .. } if kind == EventKind::ShardCollection => {
            e.collection_uuid = Some(collection.uuid);
            e.operation_description = Some(doc(description)?);
        }
        DdlChange::Modify { collection, description, .. } => {
            e.collection_uuid = Some(collection.uuid);
            e.operation_description = Some(doc(description)?);
            let before = Document::new().with("collectionOptions", options_before(collection)?);
            e.state_before = Some(before);
        }
        DdlChange::ModifyIndex { collection, before, after, .. } => {
            let (index, options) = index_change(&doc(before)?, &doc(after)?);
            e.collection_uuid = Some(collection.uuid);
            e.operation_description = Some(Document::new().with("index", index));
            e.state_before = Some(
                Document::new()
                    .with("collectionOptions", options_before(collection)?)
                    .with("indexOptions", options),
            );
        }
        DdlChange::CreateIndex { collection, spec, .. }
        | DdlChange::DropIndex { collection, spec, .. } => {
            e.collection_uuid = Some(collection.uuid);
            let indexes = vec![Value::Document(doc(spec)?)];
            e.operation_description = Some(Document::new().with("indexes", indexes));
        }
        DdlChange::DropDatabase { .. } => {}
    }
    Ok(Some(e))
}

/// Whether the event of record `r` ends a stream of `w` (the `invalidate` after it).
pub fn ends(w: &Watch<'_>, r: &DdlRecord) -> bool {
    use teseladb_changestream::ddl::invalidates;
    let watched = (w.db, w.coll);
    match &r.change {
        DdlChange::Drop { ns, .. } => {
            invalidates(w.scope, watched, EventKind::Drop, (&ns.db, Some(&ns.coll)))
        }
        DdlChange::Rename { from, .. } if from.coll.starts_with(super::reshard::TEMP_PREFIX) => {
            false
        }
        DdlChange::Rename { from, to, target, .. } => {
            let on = |n: &Namespace| {
                invalidates(w.scope, watched, EventKind::Rename, (&n.db, Some(&n.coll)))
            };
            on(from) || (target.is_some() && on(to))
        }
        DdlChange::DropDatabase { db } => {
            invalidates(w.scope, watched, EventKind::DropDatabase, (db, None))
        }
        _ => false,
    }
}

fn ns_document(ns: &Namespace) -> Document {
    Document::new().with("db", ns.db.clone()).with("coll", ns.coll.clone())
}
