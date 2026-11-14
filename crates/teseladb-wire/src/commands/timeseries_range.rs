//! Time series with measurements outside the years a bucket `_id` holds (015 FR-007, audit
//! A015-3-1): a bucket's `_id` starts with its window's start in seconds as 32 bits, which keeps
//! the buckets in time order only for windows from 1970 to 2106. A measurement before or after
//! them wraps around and lands anywhere in that order, so once a collection has one, a read
//! bounded in time scans every bucket, as the reference server does once a collection needs its
//! extended range (measured on a reference replica set member 9.0.4 on 02-11-2026: a bucket of `new
//! Date(-1)` gets `_id` time `0xffffffc4` and the reads by time still find it). The catalog keeps
//! that among the options under [`EXTENDED_RANGE`], which `listCollections` and `collMod` never
//! show and whose catalog change gives no change event.

use teseladb_bson::{Document, RawDoc};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry};
use teseladb_mql::timeseries_write::BucketSpec;

use super::Backend;
use crate::error::{CommandError, NAMESPACE_NOT_FOUND};

/// Where the catalog marks a time series that has measurements outside the bucket `_id` years.
pub const EXTENDED_RANGE: &str = "$timeseriesExtendedRange";

/// Whether time series `coll` has measurements outside the years a bucket `_id` holds.
pub fn is_extended(coll: &CollectionEntry) -> bool {
    RawDoc::from_bytes(&coll.options).is_ok_and(|o| o.get(EXTENDED_RANGE).is_some())
}

/// Whether a catalog change's description is the mark of [`mark`].
pub fn is_extended_change(description: &[u8]) -> bool {
    RawDoc::from_bytes(description).is_ok_and(|d| d.get(EXTENDED_RANGE).is_some())
}

/// Whether time `t_ms` falls in a window whose start a bucket `_id` cannot hold in order.
pub fn outside(spec: &BucketSpec, t_ms: i64) -> bool {
    !(0..=i64::from(u32::MAX)).contains(&spec.window(t_ms))
}

/// Marks `coll` in the catalog before its first measurement outside the bucket `_id` years is
/// written, so that no read finds that bucket without the mark; nothing when `times` has none or
/// the mark is there.
pub fn mark<B: Backend + ?Sized>(
    backend: &mut B,
    (coll, spec): (&CollectionEntry, &BucketSpec),
    times: impl IntoIterator<Item = i64>,
) -> Result<(), CommandError> {
    if is_extended(coll) || !times.into_iter().any(|t| outside(spec, t)) {
        return Ok(());
    }
    let (ns, now) = backend
        .collections()?
        .into_iter()
        .find(|(_, c)| c.id == coll.id)
        .ok_or_else(|| CommandError::new(NAMESPACE_NOT_FOUND, "no collection"))?;
    let options = Document::from(RawDoc::from_bytes(&now.options)?).with(EXTENDED_RANGE, true);
    backend.catalog_write(CatalogCommand::Modify {
        ns,
        options: options.to_bytes()?,
        description: Document::new().with(EXTENDED_RANGE, true).to_bytes()?,
    })?;
    Ok(())
}
