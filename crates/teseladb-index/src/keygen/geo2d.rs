//! Keys of a `2d` index (FR-011): the geohash of each legacy pair of the field, as the reference
//! server writes it (binary subtype 128, 8 bytes, big-endian). A field without an array or a
//! document gives no keys; an array of pairs gives one per pair. Rejections and their codes were
//! measured on the reference server 7.0.45 (`tests/geo2d.rs`).

use teseladb_bson::{RawDoc, RawValue};

use crate::geo::geojson::{STREAM_DIGITS, number};
use crate::geo::planar::Grid;
use crate::shell::general;

/// Binary subtype of a `2d` key.
pub const GEO2D_SUBTYPE: u8 = 128;
/// Not a pair, nor an array of pairs.
pub const LOCATION_NOT_PAIR: i32 = 16804;
/// A coordinate that is not a number.
pub const NOT_LEGACY_PAIR: i32 = 13026;
/// A coordinate outside the grid.
pub const OUT_OF_GRID: i32 = 13027;
/// A pair with one value.
pub const ONE_ELEMENT: i32 = 13068;

/// Why a document has no `2d` keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geo2dError {
    /// The reference server's code.
    pub code: i32,
    /// The reference server's reason.
    pub reason: String,
    /// Whether the reference server appends ` :: caused by :: <document>` to the reason.
    pub shows_document: bool,
}

fn error(code: i32, reason: &str) -> Geo2dError {
    Geo2dError { code, reason: reason.to_string(), shows_document: code != LOCATION_NOT_PAIR }
}

fn values(v: RawValue<'_>) -> Option<RawDoc<'_>> {
    match v {
        RawValue::Array(a) => Some(a.as_doc()),
        RawValue::Document(d) => Some(d),
        _ => None,
    }
}

/// The geohash of one pair.
fn pair_key(pair: RawDoc<'_>, grid: &Grid) -> Result<u64, Geo2dError> {
    let mut it = pair.iter().map(|(_, v)| v);
    let first = it.next().and_then(number);
    let Some(x) = first else {
        return Err(error(
            LOCATION_NOT_PAIR,
            "location object expected, location array not in correct format",
        ));
    };
    let Some(second) = it.next() else {
        return Err(error(ONE_ELEMENT, "geo field only has 1 element"));
    };
    let Some(y) = number(second) else {
        return Err(error(
            NOT_LEGACY_PAIR,
            "geo values must be 'legacy coordinate pairs' for 2d indexes",
        ));
    };
    if !grid.contains(x) || !grid.contains(y) {
        // Measured on the reference server 9.0.4: six significant digits (`[ 0.1, 0.333333 ]`).
        let reason = format!(
            "point not in interval of [ {}, {} ]",
            general(grid.min, STREAM_DIGITS),
            general(grid.max, STREAM_DIGITS)
        );
        return Err(error(OUT_OF_GRID, &reason));
    }
    Ok(grid.hash(x, y))
}

/// The `2d` keys of a field's value: 8-byte geohashes, sorted, without repeats.
pub fn geo2d_keys(v: RawValue<'_>, grid: &Grid) -> Result<Vec<[u8; 8]>, Geo2dError> {
    let Some(doc) = values(v) else { return Ok(Vec::new()) };
    let Some((_, first)) = doc.iter().next() else { return Ok(Vec::new()) };
    let pairs: Vec<RawDoc<'_>> = if values(first).is_some() {
        doc.iter()
            .map(|(_, p)| {
                values(p).ok_or_else(|| {
                    error(
                        LOCATION_NOT_PAIR,
                        "location object expected, location array not in correct format",
                    )
                })
            })
            .collect::<Result<_, _>>()?
    } else {
        vec![doc]
    };
    let mut keys = pairs
        .into_iter()
        .map(|p| pair_key(p, grid).map(u64::to_be_bytes))
        .collect::<Result<Vec<_>, _>>()?;
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}
