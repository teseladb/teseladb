//! TTL indexes (FR-010): an index on one field with `expireAfterSeconds`, and the documents its
//! monitor deletes: those whose field holds a date, or an array whose earliest date, plus the
//! index's seconds, is at or before the cluster time of the pass. Any other value never expires.

use teseladb_bson::{RawDoc, RawValue};

/// The option of a TTL index.
const EXPIRE_AFTER_SECONDS: &str = "expireAfterSeconds";
/// The key pattern of an index.
const KEY: &str = "key";
/// Milliseconds in a second.
const MS_PER_SECOND: f64 = 1000.0;

/// A TTL index: its field and its seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Ttl {
    /// The indexed field.
    pub field: String,
    /// `expireAfterSeconds`.
    pub seconds: f64,
}

impl Ttl {
    /// The TTL of specification `spec`, if it is a TTL index: one ordered field and a number of
    /// seconds.
    pub fn of(spec: RawDoc<'_>) -> Option<Ttl> {
        let seconds = match spec.get(EXPIRE_AFTER_SECONDS)? {
            RawValue::Int32(n) => f64::from(n),
            RawValue::Int64(n) => n as f64,
            RawValue::Double(n) => n,
            _ => return None,
        };
        let RawValue::Document(key) = spec.get(KEY)? else { return None };
        let mut fields = key.iter();
        let (field, kind) = fields.next()?;
        let ordered = matches!(kind, RawValue::Int32(_) | RawValue::Int64(_) | RawValue::Double(_));
        (ordered && fields.next().is_none()).then(|| Ttl { field: field.to_string(), seconds })
    }

    /// The latest date that has expired at cluster time `now_ms`.
    pub fn cutoff(&self, now_ms: i64) -> i64 {
        // Faulty variant (009 T028): the seconds go the wrong way, and documents that have not
        // expired yet go.
        #[cfg(teseladb_bug = "ttl_deletes_live")]
        return now_ms.saturating_add((self.seconds * MS_PER_SECOND) as i64);
        #[cfg(not(teseladb_bug = "ttl_deletes_live"))]
        now_ms.saturating_sub((self.seconds * MS_PER_SECOND) as i64)
    }
}

/// Whether value `v` of a TTL index's field has expired at `cutoff` ([`Ttl::cutoff`]).
pub fn expired(v: RawValue<'_>, cutoff: i64) -> bool {
    match v {
        RawValue::DateTime(ms) => ms <= cutoff,
        RawValue::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                RawValue::DateTime(ms) => Some(ms),
                _ => None,
            })
            .min()
            .is_some_and(|ms| ms <= cutoff),
        _ => false,
    }
}
