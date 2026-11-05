//! What the planner of 010 sees of a collection's indexes (FR-006, FR-009, FR-015): the catalog,
//! which indexes a read at a given time may use, `hint` resolution, and the key intervals that
//! cover a filter's bounds, with buckets, hashed values and geographic regions.
//!
//! A `2dsphere` field is scanned through the cells of a covering of the query region: every
//! indexed cell inside a covering cell (its range of descendants) and every coarser indexed cell
//! that contains one (its ancestors from the coarsest indexed level), merged where they overlap or
//! touch. Measured on refnode 7.0.45 with `explain` (`tests/planner_api.rs`).

use teseladb_keys::{
    FieldBounds, Hlc, Interval, KeyBuilder, KeyRange, PrimaryKey, bounds as key_bounds,
};
use teseladb_bson::{Collator, RawValue, compare};

use crate::geo::cells::CellId;
use crate::geo::cover::{CoverOptions, Region, covering};
use crate::keygen::{FieldKind, IndexDef, hashed_key};

/// RefDb's `BadValue`, the code of a `hint` that names no usable index.
pub const BAD_VALUE: i32 = 2;
/// The reason RefDb's planner gives for such a `hint`.
pub const HINT_NOT_FOUND: &str = "hint provided does not correspond to an existing index";

/// Covering limits of query regions on a `2dsphere` index (measured).
pub const QUERY_COVER: CoverOptions = CoverOptions { min_level: 2, max_level: 23, max_cells: 20 };

/// Where an index is in its life (FR-006).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexState {
    /// Being built: writes maintain it, reads do not use it.
    Building,
    /// Usable by reads at or after the time it became ready.
    Ready(Hlc),
    /// Being dropped.
    Dropping,
}

/// An index of the catalog as the planner needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogIndex {
    /// Number within the collection (never 0, which is the documents').
    pub id: u32,
    /// Name.
    pub name: String,
    /// Key pattern as written (`{a: 1, b: -1}`), in BSON.
    pub key_pattern: Vec<u8>,
    /// Fields and buckets.
    pub def: IndexDef,
    /// Unique.
    pub unique: bool,
    /// Sparse.
    pub sparse: bool,
    /// Partial (`partialFilterExpression`): the planner checks the filter implies it.
    pub partial: bool,
    /// Hidden from the planner (FR-009).
    pub hidden: bool,
    /// Some document went through an array on one of its fields.
    pub multikey: bool,
    /// The paths of the arrays a wildcard field walked in some document
    /// ([`crate::keygen::KeyValues::multikey_paths`]); RefDb counts one index entry per path
    /// in `validate` and shows them in `explain`.
    pub multikey_paths: Vec<String>,
    /// State.
    pub state: IndexState,
}

impl CatalogIndex {
    /// Whether a read at `read_ts` may use the index: ready by then and not hidden.
    pub fn usable_at(&self, read_ts: Hlc) -> bool {
        !self.hidden && matches!(self.state, IndexState::Ready(at) if at <= read_ts)
    }
}

/// How a query names an index in `hint`.
#[derive(Clone, Copy, Debug)]
pub enum Hint<'a> {
    /// By name.
    Name(&'a str),
    /// By key pattern, in BSON.
    KeyPattern(&'a [u8]),
}

/// The indexes of one collection.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexCatalog {
    /// Tenant.
    pub tenant: u32,
    /// Collection.
    pub collection: u32,
    /// Indexes.
    pub indexes: Vec<CatalogIndex>,
}

impl IndexCatalog {
    /// The indexes a read at `read_ts` may choose.
    pub fn usable(&self, read_ts: Hlc) -> impl Iterator<Item = &CatalogIndex> {
        self.indexes.iter().filter(move |i| i.usable_at(read_ts))
    }

    /// The index `hint` names, if a read at `read_ts` may use it; otherwise RefDb's error code
    /// and reason (a hidden index or one not ready counts as missing).
    pub fn resolve_hint(
        &self,
        hint: Hint<'_>,
        read_ts: Hlc,
    ) -> Result<&CatalogIndex, (i32, &'static str)> {
        self.usable(read_ts)
            .find(|i| match hint {
                Hint::Name(n) => i.name == n,
                Hint::KeyPattern(k) => i.key_pattern == k,
            })
            .ok_or((BAD_VALUE, HINT_NOT_FOUND))
    }

    /// The key intervals of `index` that cover `filter`, as whole keys. Bounds on a hashed field
    /// are values (only points narrow it); bounds on a `2dsphere` field come from
    /// [`geo_intervals`].
    pub fn bounds(
        &self,
        index: &CatalogIndex,
        filter: &FieldBounds<'_>,
        collation: Option<&dyn Collator>,
    ) -> Vec<KeyRange> {
        let spec = index.def.key_spec();
        // One entry per key column: a wildcard field has two (path and value), neither hashed.
        let hashed_columns: Vec<bool> = index
            .def
            .fields
            .iter()
            .flat_map(|f| match f.kind {
                FieldKind::Wildcard(_) => vec![false, false],
                FieldKind::Hashed => vec![true],
                _ => vec![false],
            })
            .collect();
        let mut fields: Vec<Vec<Interval<'_>>> = Vec::new();
        for (hashed, intervals) in hashed_columns.iter().zip(&filter.fields) {
            let points = intervals.iter().all(|i| is_point(i, collation));
            fields.push(match (hashed, points) {
                (true, true) => {
                    intervals.iter().map(|i| point(RawValue::Int64(hashed_key(i.low)))).collect()
                }
                (true, false) => vec![everything()],
                (false, _) => intervals.clone(),
            });
        }
        let prefix = KeyBuilder::index_prefix(self.tenant, self.collection, index.id);
        let narrowed = FieldBounds { fields };
        if spec.buckets > 0
            && narrowed.fields.len() == spec.fields.len()
            && let Some(ranges) = exact_buckets(self, index, &narrowed, collation)
        {
            return ranges;
        }
        key_bounds(&spec, &narrowed, collation)
            .into_iter()
            .map(|r| r.under(prefix.as_bytes()))
            .collect()
    }
}

fn is_point(i: &Interval<'_>, collation: Option<&dyn Collator>) -> bool {
    i.low_inclusive && i.high_inclusive && compare(i.low, i.high, collation).is_eq()
}

fn point(v: RawValue<'_>) -> Interval<'_> {
    Interval { low: v, low_inclusive: true, high: v, high_inclusive: true }
}

fn everything() -> Interval<'static> {
    Interval {
        low: RawValue::MinKey,
        low_inclusive: true,
        high: RawValue::MaxKey,
        high_inclusive: true,
    }
}

/// With buckets and a point on every field, the bucket of each tuple is known: one interval per
/// tuple instead of one per bucket.
fn exact_buckets(
    catalog: &IndexCatalog,
    index: &CatalogIndex,
    filter: &FieldBounds<'_>,
    collation: Option<&dyn Collator>,
) -> Option<Vec<KeyRange>> {
    if !filter.fields.iter().all(|f| f.iter().all(|i| is_point(i, collation))) {
        return None;
    }
    let mut tuples: Vec<Vec<RawValue<'_>>> = vec![Vec::new()];
    for field in &filter.fields {
        tuples = tuples
            .iter()
            .flat_map(|t| field.iter().map(move |i| [&t[..], &[i.low]].concat()))
            .collect();
    }
    let spec = index.def.key_spec();
    // The entry of a tuple without its primary key is the prefix of all its entries.
    let empty_pk = PrimaryKey::seq(0);
    let pk_len = empty_pk.as_bytes().len();
    let mut out: Vec<KeyRange> = tuples
        .iter()
        .map(|t| {
            let (key, _) = KeyBuilder::index(
                catalog.tenant,
                catalog.collection,
                index.id,
                &spec,
                t,
                &empty_pk,
                collation,
            );
            let bytes = key.as_bytes();
            let start = bytes[..bytes.len() - pk_len].to_vec();
            KeyRange { end: next_prefix(&start), start }
        })
        .collect();
    out.sort_by(|a, b| a.start.cmp(&b.start));
    out.dedup();
    Some(out)
}

/// The first byte string after every string that starts with `p`.
fn next_prefix(p: &[u8]) -> Option<Vec<u8>> {
    let mut out = p.to_vec();
    while let Some(last) = out.pop() {
        if last < u8::MAX {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

/// The intervals of cell ids (as stored, signed) that a `2dsphere` field must scan for `region`,
/// in order, inclusive at both ends.
pub fn geo_intervals(region: &dyn Region) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for cell in covering(region, QUERY_COVER) {
        ranges.push((cell.range_min().0, cell.range_max().0));
        for level in QUERY_COVER.min_level..cell.level() {
            let up: CellId = cell.parent(level);
            ranges.push((up.0, up.0));
        }
    }
    // Stored keys are signed: order by the signed value.
    let mut signed: Vec<(i64, i64)> =
        ranges.into_iter().map(|(a, b)| (a as i64, b as i64)).collect();
    signed.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (lo, hi) in signed {
        match out.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// [`geo_intervals`] as the bounds of a `2dsphere` field for [`IndexCatalog::bounds`].
pub fn geo_field_bounds(region: &dyn Region) -> Vec<Interval<'static>> {
    geo_intervals(region)
        .into_iter()
        .map(|(lo, hi)| Interval {
            low: RawValue::Int64(lo),
            low_inclusive: true,
            high: RawValue::Int64(hi),
            high_inclusive: true,
        })
        .collect()
}
