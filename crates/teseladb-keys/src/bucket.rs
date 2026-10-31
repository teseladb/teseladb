//! Buckets (FR-012): a document's key starts with `crc32c(enc(_id)) mod n`, so that increasing
//! `_id`s (ObjectIds, dates) spread over `n` ranges instead of all writing to the last one. The
//! encoding has no type bits, so `1`, `1.0` and `1L` fall in the same bucket.

use teseladb_bson::RawValue;

use crate::encode::{Direction, encode_value};
use crate::typebits::TypeBits;

/// Number of buckets of a collection's documents.
pub const DOCUMENT_BUCKETS: u8 = 16;

/// The bucket of `id` among `n` (at least 1).
pub fn bucket(id: RawValue<'_>, n: u8) -> u8 {
    let mut buf = Vec::new();
    encode_value(&mut buf, id, Direction::Ascending, &mut TypeBits::default());
    bucket_of_bytes(&buf, n)
}

/// The bucket of encoded bytes among `n`.
pub(crate) fn bucket_of_bytes(encoded: &[u8], n: u8) -> u8 {
    assert!(n > 0, "at least one bucket");
    (teseladb_hash::crc32c(encoded) % u32::from(n)) as u8
}
