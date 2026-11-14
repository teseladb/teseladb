//! The TTL monitor's pass (009 FR-010, T017): for every ready TTL index, the documents whose field
//! expired at the cluster time of the pass go, in a delete of many documents with the filter
//! `{<field>: {$lte: <cutoff>}}`, which selects a date, or an array with a date, at or before the
//! cutoff (`teseladb_index::ttl`). The node runs a pass every `ttlMonitorSleepSecs` (60 s).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::ttl::Ttl;
use teseladb_meta::catalog::Namespace;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::IndexState;

use super::crud::{Modify, Writer, modify};
use super::mutation::Image;
use super::{Backend, Ctx, Shared};
use crate::error::CommandError;

/// How often the monitor runs, as the reference server's `ttlMonitorSleepSecs` by default.
pub const TTL_MONITOR_SECS: u64 = 60;

/// The filter of the documents index `ttl` lets go at cluster time `now_ms` (the simulator's
/// `indexes` workload deletes with it too).
pub fn expired_filter(ttl: &Ttl, now_ms: i64) -> Document {
    let cutoff = Value::DateTime(ttl.cutoff(now_ms));
    Document::new().with(ttl.field.as_str(), Document::new().with("$lte", cutoff))
}

/// One pass at cluster time `now_ms`: the documents it deleted.
pub fn pass<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    now_ms: i64,
) -> Result<i64, CommandError> {
    // The results that retryable statements across ranges left go after their retry window
    // (009 FR-003).
    super::cross_range::purge_results(backend, u64::try_from(now_ms).unwrap_or(0))?;
    let mut deleted = 0;
    for (ns, coll) in backend.collections()? {
        // A clustered collection with `expireAfterSeconds` expires by its `_id` (015 FR-009).
        if let CollectionKind::Clustered { expire_after_seconds: Some(seconds), .. } =
            super::kinds::kind_of(&coll)?
        {
            let ttl = Ttl { field: "_id".to_owned(), seconds: seconds as f64 };
            deleted += expire(backend, shared, &ns, expired_filter(&ttl, now_ms))?;
        }
        // A time series with `expireAfterSeconds` drops the buckets whose newest measurement
        // expired (015 FR-006): their `control.max` of the time field.
        if let CollectionKind::TimeSeries(ts) = super::kinds::kind_of(&coll)?
            && let Some(seconds) = ts.expire_after_seconds
        {
            let field = format!("control.max.{}", ts.time_field);
            let ttl = Ttl { field, seconds: seconds as f64 };
            deleted += expire(backend, shared, &ns, expired_filter(&ttl, now_ms))?;
        }
        for e in backend.indexes(coll.id)? {
            if e.state != IndexState::Ready {
                continue;
            }
            let Some(ttl) = Ttl::of(RawDoc::from_bytes(&e.spec)?) else { continue };
            deleted += expire(backend, shared, &ns, expired_filter(&ttl, now_ms))?;
        }
    }
    Ok(deleted)
}

/// Deletes the documents of `ns` that `filter` selects: how many went.
fn expire<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &Shared,
    ns: &Namespace,
    filter: Document,
) -> Result<i64, CommandError> {
    let m = Modify {
        filter,
        update: None,
        array_filters: None,
        multi: true,
        upsert: false,
        collation: None,
        sort: None,
        image: Image::None,
        vars: None,
        hint: None,
        supplied: None,
    };
    // A time series' buckets go whole, as they are stored.
    let cmd = Document::new().with("delete", ns.coll.as_str()).with("rawData", true);
    let mut ctx = Ctx { backend, shared, db: &ns.db, cmd: &cmd };
    Ok(modify(&mut ctx, &mut Writer::internal(), ns, &m, true, 0)?.n)
}
