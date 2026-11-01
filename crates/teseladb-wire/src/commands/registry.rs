//! The cluster's record of sessions (FR-016, data model §2): `config.system.sessions`, a
//! collection of the data range like any other, with RefDb's documents
//! `{_id: {id: <UUID>, uid: <SHA-256 of the user>}, lastUse: <date>}`. Each node writes the
//! sessions it used since its last refresh (`logicalSessionRefreshMillis`) and removes the
//! records unused for the session timeout, the work of RefDb's TTL index `lsidTTLIndex`.

use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};

use super::crud::{Modify, Writer, modify};
use super::mutation::Image;
use super::names::ns;
use super::{Backend, Ctx, Shared, lock, new_uuid};
use crate::error::{CommandError, INTERNAL_ERROR};

/// Database of the record.
pub const SESSIONS_DB: &str = "config";
/// Collection of the record.
pub const SESSIONS_COLL: &str = "system.sessions";
/// Milliseconds between two refreshes of a node (`logicalSessionRefreshMillis`, 9.0.4).
pub const REFRESH_INTERVAL_MS: u64 = 300_000;

/// What one refresh did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    /// Sessions whose record was written.
    pub written: usize,
    /// Records removed because their session expired.
    pub removed: i64,
}

/// The record's `_id` of session `lsid` without authentication.
pub fn record_id(lsid: u128) -> Document {
    Document::new()
        .with("id", Value::Binary { subtype: 4, bytes: lsid.to_be_bytes().to_vec() })
        .with("uid", Value::Binary { subtype: 0, bytes: sha256(b"").to_vec() })
}

/// The record's collection, created on first use (users cannot create `system.` collections).
fn collection<B: Backend + ?Sized>(
    backend: &mut B,
    space: &Namespace,
) -> Result<CollectionEntry, CommandError> {
    if let Some(c) = backend.collection(space)? {
        return Ok(c);
    }
    let uuid = new_uuid(backend);
    let options = Document::new().to_bytes()?;
    let cmd = CatalogCommand::Create { ns: space.clone(), uuid, options, if_absent: true };
    backend
        .catalog_write(cmd)?
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the sessions' record was not created"))
}

fn statement(filter: Document, update: Option<Value>, multi: bool) -> Modify {
    Modify {
        filter,
        update,
        array_filters: None,
        multi,
        upsert: !multi,
        collation: None,
        sort: None,
        image: Image::None,
        vars: None,
        hint: None,
        supplied: None,
    }
}

/// Writes the record of each session of `shared` used at or after `since_ms` and removes the
/// records unused for the session timeout at cluster time `now_ms`.
pub fn refresh<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    since_ms: u64,
    now_ms: u64,
) -> Result<Refreshed, CommandError> {
    let r = refresh_records(backend, shared, since_ms, now_ms)?;
    // `logicalSessionRecordCache` of `serverStatus` (006 T046).
    let done = backend.now_ms();
    let ms = |t: u64| i64::try_from(t).unwrap_or(i64::MAX);
    let jobs = &mut lock(shared).status.sessions;
    jobs.refreshes += 1;
    jobs.refresh_at = ms(now_ms);
    jobs.refresh_done = ms(done);
    jobs.refresh_ms = ms(done.saturating_sub(now_ms));
    jobs.refreshed = i64::try_from(r.written).unwrap_or(i64::MAX);
    jobs.ended = r.removed;
    Ok(r)
}

fn refresh_records<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    since_ms: u64,
    now_ms: u64,
) -> Result<Refreshed, CommandError> {
    let (used, timeout, ended) = {
        let mut g = lock(shared);
        let ended = g.sessions.take_ending();
        (g.sessions.used_since(since_ms), g.sessions.timeout_ms(), ended)
    };
    // The sessions `endSessions` named end now, with their cursors, as at a refresh of RefDb's
    // session cache.
    super::session::kill_local(shared, &super::session::KillScope::Sessions(ended.clone()));
    let space = ns(SESSIONS_DB, SESSIONS_COLL);
    let exists = backend.collection(&space)?.is_some();
    if used.is_empty() && !exists {
        return Ok(Refreshed::default());
    }
    collection(backend, &space)?;
    let cmd = Document::new().with("update", SESSIONS_COLL);
    let mut ctx = Ctx { backend, shared, db: SESSIONS_DB, cmd: &cmd };
    let mut w = Writer::new(&ctx, "update")?;
    let mut out = Refreshed::default();
    for (i, (lsid, last_use)) in used.iter().enumerate() {
        let filter = Document::new().with("_id", record_id(*lsid));
        let date = Value::DateTime(i64::try_from(*last_use).unwrap_or(i64::MAX));
        let set = Document::new().with("$max", Document::new().with("lastUse", date));
        let m = statement(filter, Some(Value::Document(set)), false);
        modify(&mut ctx, &mut w, &space, &m, true, i as u32)?;
        out.written += 1;
    }
    if !ended.is_empty() {
        // The records of the ended sessions go too.
        let ids: Vec<Value> = ended.iter().map(|l| Value::Document(record_id(*l))).collect();
        let filter = Document::new().with("_id", Document::new().with("$in", ids));
        let m = statement(filter, None, true);
        out.removed += modify(&mut ctx, &mut w, &space, &m, true, used.len() as u32)?.n;
    }
    let limit = i64::try_from(now_ms.saturating_sub(timeout)).unwrap_or(i64::MAX);
    let old = Document::new().with("$lte", Value::DateTime(limit));
    let m = statement(Document::new().with("lastUse", old), None, true);
    out.removed += modify(&mut ctx, &mut w, &space, &m, true, used.len() as u32 + 1)?.n;
    Ok(out)
}
