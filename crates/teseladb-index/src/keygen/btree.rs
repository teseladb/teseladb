//! Keys of an index (FR-001, FR-002, FR-005, FR-011, FR-013, FR-014): the values of each field,
//! taken element by element where fields share an array ([`super::multikey`]), and the entries
//! in the key format of 003 with the document's primary key, without repeats and in key order.
//! Fields that go through different arrays are refused with `CannotIndexParallelArrays`. A hashed
//! field stores the hash of its value and refuses arrays; a `2dsphere` field stores the cells of
//! its geometry and a `2d` field the geohash of its pair, and a document without a value for one
//! of them has no entries (measured on refnode 7.0.45 and 9.0.4). A wildcard field gives one key
//! per (path, value) it reaches ([`super::wildcard`]).

use std::collections::BTreeMap;

use teseladb_keys::space::unique_value;
use teseladb_keys::{Key, KeyBuilder, PrimaryKey, TypeBits};
use teseladb_bson::{Collator, RawDoc, RawValue, Value};

use super::geo2d::{GEO2D_SUBTYPE, geo2d_keys};
use super::geo2dsphere::geo2dsphere_keys;
use super::hashed::hashed_key;
use super::multikey::{Found, field_values as walk_fields};
use super::wildcard::{compound_array, wildcard_values};
use super::{FieldKind, IndexDef, IndexField, KeygenError};
use crate::geo::geojson::GeoError;

/// One value of an index key.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyValue<'a> {
    /// A value of the document.
    Raw(RawValue<'a>),
    /// A hash or a cell id.
    Long(i64),
    /// A `2d` geohash.
    Geohash([u8; 8]),
    /// The path of a wildcard key.
    Path(String),
}

impl KeyValue<'_> {
    /// The value as BSON.
    pub fn raw(&self) -> RawValue<'_> {
        match self {
            KeyValue::Raw(v) => *v,
            KeyValue::Long(n) => RawValue::Int64(*n),
            KeyValue::Geohash(h) => RawValue::Binary { subtype: GEO2D_SUBTYPE, bytes: h },
            KeyValue::Path(p) => RawValue::String(p),
        }
    }
}

/// The key values of a document in an index: one tuple per entry, one value per field.
#[derive(Clone, Debug)]
pub struct KeyValues<'a> {
    /// The tuples, which may repeat.
    pub tuples: Vec<Vec<KeyValue<'a>>>,
    /// Some field went through an array.
    pub multikey: bool,
    /// The paths of the arrays a wildcard field walked (RefDb keeps one entry per path).
    pub multikey_paths: Vec<String>,
}

/// Whether a geographic field's value stands for no location.
fn no_location(v: &RawValue<'_>) -> bool {
    match v {
        RawValue::Null | RawValue::Undefined => true,
        RawValue::Array(a) => a.as_doc().is_empty(),
        RawValue::Document(d) => d.is_empty(),
        _ => false,
    }
}

/// The cells of every measurement's geometry in bucket column `v` (015 FR-008); a value that
/// stands for no location has none, and one that is no geometry fails as RefDb's 511.
fn bucket_geo_keys(field: &IndexField, v: RawValue<'_>) -> Result<Vec<i64>, KeygenError> {
    let RawValue::Binary { subtype: teseladb_bson::types::BINARY_COLUMN, bytes } = v else {
        return Ok(Vec::new());
    };
    let geo = |element: Value, reason| KeygenError::Geo {
        path: field.path.clone(),
        element,
        reason,
        bucket: true,
    };
    let cells = teseladb_bson::column::decode(bytes, usize::MAX)
        .map_err(|e| geo(Value::Null, GeoError(e.0.to_string())))?;
    let mut keys = Vec::new();
    for cell in cells.into_iter().flatten() {
        let mut holder = Vec::with_capacity(cell.bytes.len() + 8);
        holder.extend_from_slice(&((cell.bytes.len() + 8) as i32).to_le_bytes());
        holder.push(cell.kind);
        holder.extend_from_slice(b"g\0");
        holder.extend_from_slice(&cell.bytes);
        holder.push(0);
        let doc =
            RawDoc::from_bytes(&holder).map_err(|e| geo(Value::Null, GeoError(e.to_string())))?;
        let Some(value) = doc.get("g") else { continue };
        if no_location(&value) {
            continue;
        }
        keys.extend(geo2dsphere_keys(value).map_err(|reason| geo(value.into(), reason))?);
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

/// The values a field stores for one value of its path.
fn stored_values<'a>(
    field: &IndexField,
    v: RawValue<'a>,
) -> Result<Vec<KeyValue<'a>>, KeygenError> {
    Ok(match &field.kind {
        FieldKind::Ordered | FieldKind::Wildcard(_) => vec![KeyValue::Raw(v)],
        FieldKind::Hashed => vec![KeyValue::Long(hashed_key(v))],
        _ if no_location(&v) => Vec::new(),
        FieldKind::Sphere => geo2dsphere_keys(v)
            .map_err(|reason| KeygenError::Geo {
                path: field.path.clone(),
                element: v.into(),
                reason,
                bucket: false,
            })?
            .into_iter()
            .map(KeyValue::Long)
            .collect(),
        FieldKind::SphereBucket => {
            bucket_geo_keys(field, v)?.into_iter().map(KeyValue::Long).collect()
        }
        FieldKind::Plane(grid) => geo2d_keys(v, grid)
            .map_err(KeygenError::Plane)?
            .into_iter()
            .map(KeyValue::Geohash)
            .collect(),
    })
}

/// The key values of `doc` under `index`.
pub fn key_values<'a>(index: &IndexDef, doc: RawDoc<'a>) -> Result<KeyValues<'a>, KeygenError> {
    let regular: Vec<&IndexField> =
        index.fields.iter().filter(|f| !matches!(f.kind, FieldKind::Wildcard(_))).collect();
    let paths: Vec<(&str, bool)> = regular
        .iter()
        .map(|f| {
            let geo =
                matches!(f.kind, FieldKind::Sphere | FieldKind::SphereBucket | FieldKind::Plane(_));
            (f.path.as_str(), !geo)
        })
        .collect();
    let found = walk_fields(RawValue::Document(doc), &paths)?;
    for (field, array) in regular.iter().zip(&found.arrays) {
        if let (FieldKind::Hashed, Some(path)) = (&field.kind, array) {
            return Err(KeygenError::HashedArray(path.clone()));
        }
    }
    let wildcard = index.fields.iter().find_map(|f| match &f.kind {
        FieldKind::Wildcard(p) => Some(wildcard_values(doc, p)),
        _ => None,
    });
    let Some(wildcard) = wildcard else {
        let multikey = found.arrays.iter().any(Option::is_some);
        // A sparse index leaves out the keys whose fields are all missing (measured on 9.0.4:
        // `{a.b: 1}` over `a: [{b: 1}, {c: 1}]` has one entry).
        let kept: Vec<Vec<Found<'a>>> = found
            .tuples
            .into_iter()
            .filter(|t| !index.sparse || t.iter().any(|f| !f.missing))
            .collect();
        let tuples = stored_tuples(&regular, &kept)?;
        return Ok(KeyValues { tuples, multikey, multikey_paths: Vec::new() });
    };
    if let Some((field, _)) = regular.iter().zip(&found.arrays).find(|(_, a)| a.is_some()) {
        return Err(compound_array(&field.path));
    }
    let pairs: Vec<[KeyValue<'a>; 2]> = if wildcard.keys.is_empty() {
        if index.fields.len() == 1 {
            Vec::new()
        } else {
            vec![[KeyValue::Raw(RawValue::MinKey), KeyValue::Raw(RawValue::MinKey)]]
        }
    } else {
        wildcard.keys.into_iter().map(|(p, v)| [KeyValue::Path(p), KeyValue::Raw(v)]).collect()
    };
    let others = stored_tuples(&regular, &found.tuples)?;
    let mut tuples = Vec::new();
    for other in &others {
        for pair in &pairs {
            let mut rest = other.iter();
            let mut t = Vec::with_capacity(index.fields.len() + 1);
            for f in &index.fields {
                match f.kind {
                    FieldKind::Wildcard(_) => t.extend(pair.iter().cloned()),
                    _ => t.extend(rest.next().cloned()),
                }
            }
            tuples.push(t);
        }
    }
    let multikey_paths: Vec<String> = wildcard.multikey_paths.into_iter().collect();
    Ok(KeyValues { tuples, multikey: !multikey_paths.is_empty(), multikey_paths })
}

/// The key tuples of the values `raw` of the fields `fields`: what each field stores, every
/// combination of a field that stores several values (cells of a geometry) with the others.
fn stored_tuples<'a>(
    fields: &[&IndexField],
    raw: &[Vec<Found<'a>>],
) -> Result<Vec<Vec<KeyValue<'a>>>, KeygenError> {
    let mut tuples = Vec::new();
    for values in raw {
        let mut partial: Vec<Vec<KeyValue<'a>>> = vec![Vec::new()];
        for (field, v) in fields.iter().zip(values) {
            let stored = stored_values(field, v.value)?;
            partial = partial
                .iter()
                .flat_map(|t| {
                    stored.iter().map(move |k| [&t[..], std::slice::from_ref(k)].concat())
                })
                .collect();
        }
        tuples.extend(partial);
    }
    Ok(tuples)
}

/// The value an entry of index `def` stores: its type bits, preceded by the document's primary key
/// in a unique index ([`teseladb_keys::space::unique_value`]).
pub fn entry_value(def: &IndexDef, pk: &PrimaryKey, bits: &TypeBits) -> Vec<u8> {
    if def.unique { unique_value(pk, bits) } else { bits.as_bytes().to_vec() }
}

/// The entries of `doc` in index `index` of collection `(tenant, collection)`: keys in the format
/// of 003 with the document's primary key `pk` (in a unique index, without it: it goes in the
/// value, [`entry_value`]) and their type bits, without repeats, in key order.
pub fn index_entries(
    tenant: u32,
    collection: u32,
    index: u32,
    def: &IndexDef,
    doc: RawDoc<'_>,
    pk: &PrimaryKey,
    collation: Option<&dyn Collator>,
) -> Result<Vec<(Key, TypeBits)>, KeygenError> {
    let spec = def.key_spec();
    let kv = key_values(def, doc)?;
    let entries: BTreeMap<Key, TypeBits> = kv
        .tuples
        .iter()
        .map(|t| {
            let raw: Vec<RawValue<'_>> = t.iter().map(KeyValue::raw).collect();
            if def.unique {
                KeyBuilder::unique_index(tenant, collection, index, &spec, &raw, collation)
            } else {
                KeyBuilder::index(tenant, collection, index, &spec, &raw, pk, collation)
            }
        })
        .collect();
    Ok(entries.into_iter().collect())
}
