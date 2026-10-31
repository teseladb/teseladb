//! HLC marks as BSON timestamps (FR-024): `t` is the seconds of the physical part and `i` the
//! milliseconds within the second times 2²² plus the logical counter. The conversion is injective
//! and its inverse exact; a timestamp it never makes (`i` from 1 000 × 2²² up, as RefDb writes
//! them) maps to the greatest mark not above it.

use teseladb_keys::Hlc;

use crate::clock::LOGICAL_BITS;

const LOGICAL_MASK: u32 = (1 << LOGICAL_BITS) - 1;

/// The `(t, i)` of a mark.
pub fn to_timestamp(h: Hlc) -> (u32, u32) {
    let seconds = u32::try_from(h.physical / 1000).unwrap_or(u32::MAX);
    let ms = (h.physical % 1000) as u32;
    (seconds, (ms << LOGICAL_BITS) | (h.logical & LOGICAL_MASK))
}

/// The mark of `(t, i)`: exact for what [`to_timestamp`] makes, the greatest mark not above it
/// otherwise.
pub fn from_timestamp((t, i): (u32, u32)) -> Hlc {
    let ms = i >> LOGICAL_BITS;
    if ms >= 1000 {
        return Hlc { physical: u64::from(t) * 1000 + 999, logical: LOGICAL_MASK };
    }
    Hlc { physical: u64::from(t) * 1000 + u64::from(ms), logical: i & LOGICAL_MASK }
}
