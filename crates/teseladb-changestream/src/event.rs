//! The event document a cursor returns (014 FR-002, contract `events.md`), with its fields in the
//! order a reference router 9.0.4 gives them (measured on 31-10 with `watch()` from the reference
//! shell): `txnNumber` and `lsid` first inside a transaction; then `_id`, `operationType`,
//! `clusterTime`, `collectionUUID` (only with `showExpandedEvents`) and `wallTime`; an insert or a
//! replacement goes on with `fullDocument`, `ns` and `documentKey`; an update with `ns`,
//! `documentKey`, `updateDescription` and then `fullDocument` or `fullDocumentBeforeChange` when
//! asked for; a deletion with `ns` and `documentKey`. A catalog event (measured on 01-11) puts `to`
//! first for a `rename`, then `_id`, `operationType`, `clusterTime`, `collectionUUID`, `wallTime`
//! and `ns`, and with `showExpandedEvents` `operationDescription`, `stateBeforeChange` and
//! `nsType`; an `invalidate` has no `ns`.

use teseladb_bson::{BsonError, Document, RawDoc, Value};
use teseladb_hlc::{Hlc, to_timestamp};

use crate::changes::Change;
use crate::meta::lsid_document;
use crate::token::{EventKind, ResumeToken};

/// BSON binary subtype of a UUID.
const UUID_SUBTYPE: u8 = 4;

/// The namespace of an event: a database, and a collection unless the event is about the database.
#[derive(Clone, Debug, PartialEq)]
pub struct Namespace {
    /// Database.
    pub db: String,
    /// Collection.
    pub coll: Option<String>,
}

/// An event ready to be written as the document a cursor returns.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeEvent {
    /// Its resume token.
    pub token: ResumeToken,
    /// Commit mark (`clusterTime`).
    pub cluster_time: Hlc,
    /// Wall clock of the commit in milliseconds (`wallTime`).
    pub wall_ms: i64,
    /// Namespace.
    pub ns: Namespace,
    /// Identifier of the collection, written with `showExpandedEvents`.
    pub collection_uuid: Option<[u8; 16]>,
    /// `documentKey`, for the events of a document.
    pub document_key: Option<Document>,
    /// `updateDescription` of an update.
    pub update_description: Option<Document>,
    /// `fullDocument`, when the event carries it (`Value::Null` when asked for and missing).
    pub full_document: Option<Value>,
    /// `fullDocumentBeforeChange`, when asked for (`Value::Null` when missing).
    pub before: Option<Value>,
    /// `lsid` and `txnNumber` of the transaction that wrote it.
    pub txn: Option<(Document, i64)>,
    /// `to` of a `rename`.
    pub to: Option<Namespace>,
    /// `operationDescription` of a DDL event, written with `showExpandedEvents`.
    pub operation_description: Option<Document>,
    /// `stateBeforeChange` of a `modify`, written with `showExpandedEvents`.
    pub state_before: Option<Document>,
    /// `nsType` of a `create` (`collection`, `view` or `timeseries`), written with
    /// `showExpandedEvents`.
    pub ns_type: Option<&'static str>,
}

impl ChangeEvent {
    /// The event of `change` in namespace `ns`, with its `documentKey`; `expanded` is
    /// `showExpandedEvents`. An insert and a replacement carry their document; the caller adds
    /// what the options ask for (`fullDocument` of an update, `fullDocumentBeforeChange`,
    /// `collectionUUID`).
    pub fn of(
        change: &Change,
        ns: Namespace,
        document_key: Option<Document>,
        expanded: bool,
    ) -> Result<ChangeEvent, BsonError> {
        let full_document = match (change.kind, &change.doc) {
            (EventKind::Insert | EventKind::Replace, Some(bytes)) => {
                Some(Value::Document(Document::from(RawDoc::from_bytes(bytes)?)))
            }
            _ => None,
        };
        let meta = change.meta.as_ref();
        let ts = change.event.ts;
        Ok(ChangeEvent {
            token: ResumeToken::new(change.event.clone(), change.kind),
            cluster_time: ts,
            wall_ms: i64::try_from(ts.physical).unwrap_or(i64::MAX),
            ns,
            collection_uuid: None,
            document_key,
            update_description: meta
                .and_then(|m| m.update.as_ref())
                .map(|u| u.to_document(expanded)),
            full_document,
            before: None,
            txn: meta
                .and_then(|m| m.txn.as_ref())
                .map(|t| (lsid_document(t.lsid, &t.uid), t.txn_number)),
            to: None,
            operation_description: None,
            state_before: None,
            ns_type: None,
        })
    }

    /// An event of the catalog (014 FR-014) or an `invalidate`, with token `token` (its mark is
    /// the `clusterTime`) in namespace `ns`; the caller adds the rest.
    pub fn catalog(token: ResumeToken, ns: Namespace) -> ChangeEvent {
        let ts = token.event.ts;
        ChangeEvent {
            token,
            cluster_time: ts,
            wall_ms: i64::try_from(ts.physical).unwrap_or(i64::MAX),
            ns,
            collection_uuid: None,
            document_key: None,
            update_description: None,
            full_document: None,
            before: None,
            txn: None,
            to: None,
            operation_description: None,
            state_before: None,
            ns_type: None,
        }
    }

    /// The event's type.
    pub fn kind(&self) -> EventKind {
        self.token.kind
    }

    /// The document the cursor returns; `expanded` is `showExpandedEvents`.
    pub fn to_document(&self, expanded: bool) -> Document {
        let mut d = Document::new();
        if let Some(to) = &self.to {
            d.push("to", namespace(to));
        }
        if let Some((lsid, number)) = &self.txn {
            d.push("txnNumber", *number);
            d.push("lsid", lsid.clone());
        }
        let mut id = Document::new();
        id.push("_data", self.token.encode());
        d.push("_id", id);
        d.push("operationType", operation_type(self.kind()));
        let (time, increment) = to_timestamp(self.cluster_time);
        d.push("clusterTime", Value::Timestamp { time, increment });
        if expanded && let Some(uuid) = self.collection_uuid {
            d.push("collectionUUID", Value::Binary { subtype: UUID_SUBTYPE, bytes: uuid.to_vec() });
        }
        d.push("wallTime", Value::DateTime(self.wall_ms));
        let full_first = matches!(self.kind(), EventKind::Insert | EventKind::Replace);
        if full_first && let Some(full) = &self.full_document {
            d.push("fullDocument", full.clone());
        }
        if self.kind() != EventKind::Invalidate {
            d.push("ns", namespace(&self.ns));
        }
        if let Some(key) = &self.document_key {
            d.push("documentKey", key.clone());
        }
        if let Some(desc) = &self.update_description {
            d.push("updateDescription", desc.clone());
        }
        // An update gives the pre-image before the post-image (measured on 01-11).
        if let Some(before) = &self.before {
            d.push("fullDocumentBeforeChange", before.clone());
        }
        if !full_first && let Some(full) = &self.full_document {
            d.push("fullDocument", full.clone());
        }
        if expanded {
            if let Some(desc) = &self.operation_description {
                d.push("operationDescription", desc.clone());
            }
            if let Some(state) = &self.state_before {
                d.push("stateBeforeChange", state.clone());
            }
            if let Some(t) = self.ns_type {
                d.push("nsType", t);
            }
        }
        d
    }
}

/// `ns` as an event writes it: `{db, coll}`, or `{db}` for a database.
fn namespace(ns: &Namespace) -> Document {
    let mut d = Document::new();
    d.push("db", ns.db.clone());
    if let Some(coll) = &ns.coll {
        d.push("coll", coll.clone());
    }
    d
}

/// The `operationType` of an event type.
pub fn operation_type(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Insert => "insert",
        EventKind::Update => "update",
        EventKind::Replace => "replace",
        EventKind::Delete => "delete",
        EventKind::Drop => "drop",
        EventKind::Rename => "rename",
        EventKind::DropDatabase => "dropDatabase",
        EventKind::Invalidate => "invalidate",
        EventKind::Create => "create",
        EventKind::CreateIndexes => "createIndexes",
        EventKind::DropIndexes => "dropIndexes",
        EventKind::Modify => "modify",
        EventKind::ShardCollection => "shardCollection",
        EventKind::RefineCollectionShardKey => "refineCollectionShardKey",
        EventKind::ReshardCollection => "reshardCollection",
    }
}
