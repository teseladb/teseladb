//! Key spaces (FR-011, FR-012): the first byte says what the key holds, and tenant, collection,
//! index and table go in big-endian so that a collection with its indexes is one contiguous
//! interval.
//!
//! - `00` range-local data: `00 · range · kind · rest`.
//! - `01` documents and index entries: `01 · tenant · collection · index · …`, index 0 for the
//!   documents. A document is `· bucket · enc(_id)`, or `· enc(shard key) · enc(_id)` with a shard
//!   key, `· enc(_id)` in a clustered collection and `· seq` in a capped one. An index entry is
//!   `· [bucket] · enc(values) · primary key`, and an entry of a unique index `· [bucket] ·
//!   enc(values)` with the primary key in its value.
//! - `02` transaction records, `03` system tables, `04` range descriptors of the meta-ranges.

use teseladb_bson::{Collator, RawValue};

use crate::bounds::next_prefix;
use crate::bucket::{DOCUMENT_BUCKETS, bucket, bucket_of_bytes};
use crate::encode::{Direction, encode_value_with};
use crate::typebits::TypeBits;
use crate::version::{Hlc, KeyError, VersionedKey};

// The first byte of each key space.

/// Data local to a range.
pub const RANGE_LOCAL: u8 = 0x00;
/// Documents and index entries.
pub const DATA: u8 = 0x01;
/// Transaction records.
pub const TXN: u8 = 0x02;
/// System tables.
pub const SYSTEM: u8 = 0x03;
/// Range descriptors in the meta-ranges (007).
pub const META: u8 = 0x04;

/// The index number of a collection's documents.
pub const DOCUMENTS: u32 = 0;

/// A logical key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(Vec<u8>);

impl Key {
    /// Its bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The key with version `ts` (FR-013).
    pub fn with_version(&self, ts: Hlc) -> VersionedKey {
        VersionedKey::new(&self.0, ts)
    }
}

/// The primary key of a document, which ends its index entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryKey(Vec<u8>);

impl PrimaryKey {
    /// `enc(_id)`.
    pub fn id(id: RawValue<'_>) -> PrimaryKey {
        PrimaryKey(ascending(&[id]))
    }

    /// `enc(shard key) · enc(_id)`, for collections with a shard key (018).
    pub fn sharded(shard_key: &[RawValue<'_>], id: RawValue<'_>) -> PrimaryKey {
        let mut out = ascending(shard_key);
        out.extend(ascending(&[id]));
        PrimaryKey(out)
    }

    /// The insertion number, for capped collections (015).
    pub fn seq(seq: u64) -> PrimaryKey {
        PrimaryKey(seq.to_be_bytes().to_vec())
    }

    /// Its bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A field of an index key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSpec {
    /// Its order.
    pub direction: Direction,
}

/// The fields of an index key and its number of buckets (0 without them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKeySpec {
    /// Fields in order.
    pub fields: Vec<FieldSpec>,
    /// Buckets of the index.
    pub buckets: u8,
}

/// A split key of range `[start, end)` (`end` empty: no end) that keeps whole every collection
/// `whole(tenant, collection)` names, such as a capped collection (015 research R1): `at` itself,
/// or, when it falls inside such a collection's span, the start of the span or else its end;
/// `None` when neither is strictly inside the range.
pub fn cut_outside(
    at: &[u8],
    (start, end): (&[u8], &[u8]),
    whole: impl Fn(u32, u32) -> bool,
) -> Option<Vec<u8>> {
    let inside = |k: &[u8]| start < k && (end.is_empty() || k < end);
    let word = |i: usize| Some(u32::from_be_bytes(at.get(i..i + 4)?.try_into().ok()?));
    // A key past a document's ending in `00` may be its change meta (014), the next key after it:
    // the cut goes before the document so that both stay in one range.
    let at = match (at.first(), word(9), at.split_last()) {
        (Some(&DATA), Some(DOCUMENTS), Some((0, doc))) if doc.len() > 13 => doc,
        _ => at,
    };
    let (Some(&DATA), Some(t), Some(c)) = (at.first(), word(1), word(5)) else {
        return inside(at).then(|| at.to_vec());
    };
    let span = collection_prefix(t, c);
    if !whole(t, c) || at == span.as_slice() {
        return inside(at).then(|| at.to_vec());
    }
    let after = KeyBuilder::collection_end(t, c).0;
    [span, after].into_iter().find(|k| inside(k))
}

/// Builders of every kind of key.
pub struct KeyBuilder;

impl KeyBuilder {
    /// A document: `01 · tenant · collection · 0 · bucket · enc(_id)`.
    pub fn document(tenant: u32, collection: u32, id: RawValue<'_>) -> Key {
        let mut out = data(tenant, collection, DOCUMENTS);
        out.push(bucket(id, DOCUMENT_BUCKETS));
        out.extend(ascending(&[id]));
        Key(out)
    }

    /// The document of primary key `pk` (an index entry's end) in a collection without a shard
    /// key, clustering or capping: its bucket comes from the key's bytes, as [`Self::document`]
    /// takes it from the `_id`'s encoding.
    pub fn document_of_pk(tenant: u32, collection: u32, pk: &[u8]) -> Key {
        let mut out = data(tenant, collection, DOCUMENTS);
        out.push(bucket_of_bytes(pk, DOCUMENT_BUCKETS));
        out.extend_from_slice(pk);
        Key(out)
    }

    /// A document of a collection with a shard key, without bucket: its order is the shard key's.
    /// A hashed field of the shard key comes already hashed (009, 018).
    pub fn document_sharded(
        tenant: u32,
        collection: u32,
        shard_key: &[RawValue<'_>],
        id: RawValue<'_>,
    ) -> Key {
        let mut out = data(tenant, collection, DOCUMENTS);
        out.extend(PrimaryKey::sharded(shard_key, id).0);
        Key(out)
    }

    /// A document of a clustered collection, without bucket: its order is the `_id`'s (015).
    pub fn document_clustered(tenant: u32, collection: u32, id: RawValue<'_>) -> Key {
        let mut out = data(tenant, collection, DOCUMENTS);
        out.extend(ascending(&[id]));
        Key(out)
    }

    /// A document of a capped collection: its insertion order (015).
    pub fn document_capped(tenant: u32, collection: u32, seq: u64) -> Key {
        let mut out = data(tenant, collection, DOCUMENTS);
        out.extend_from_slice(&seq.to_be_bytes());
        Key(out)
    }

    /// An index entry (index above 0): `01 · tenant · collection · index · [bucket] ·
    /// enc(values) · primary key`, and its type bits. `values` has one value per field of `spec`;
    /// with the index's `collation`, its strings are in the collation's order, and so is the
    /// bucket, so that equal strings for the collation share it.
    pub fn index(
        tenant: u32,
        collection: u32,
        index: u32,
        spec: &IndexKeySpec,
        values: &[RawValue<'_>],
        pk: &PrimaryKey,
        collation: Option<&dyn Collator>,
    ) -> (Key, TypeBits) {
        let (mut out, bits) = index_values(tenant, collection, index, spec, values, collation);
        out.extend_from_slice(&pk.0);
        (Key(out), bits)
    }

    /// An entry of a unique index (009 FR-004): `01 · tenant · collection · index · [bucket] ·
    /// enc(values)`, without the primary key, which goes in its value ([`unique_value`]); so two
    /// documents with the same values write the same key.
    pub fn unique_index(
        tenant: u32,
        collection: u32,
        index: u32,
        spec: &IndexKeySpec,
        values: &[RawValue<'_>],
        collation: Option<&dyn Collator>,
    ) -> (Key, TypeBits) {
        let (out, bits) = index_values(tenant, collection, index, spec, values, collation);
        (Key(out), bits)
    }

    /// The first key of a collection and its indexes.
    pub fn collection_start(tenant: u32, collection: u32) -> Key {
        Key(collection_prefix(tenant, collection))
    }

    /// The first key after a collection and its indexes.
    pub fn collection_end(tenant: u32, collection: u32) -> Key {
        Key(next_prefix(&collection_prefix(tenant, collection)).expect("a data key has a next one"))
    }

    /// `01 · tenant · collection · index`: what [`crate::KeyRange::under`] goes under.
    pub fn index_prefix(tenant: u32, collection: u32, index: u32) -> Key {
        Key(data(tenant, collection, index))
    }

    /// A transaction record (008).
    pub fn txn_record(txn_id: &[u8]) -> Key {
        let mut out = vec![TXN];
        out.extend_from_slice(txn_id);
        Key(out)
    }

    /// A row of a system table.
    pub fn system(tenant: u32, table: u32, key: &[u8]) -> Key {
        let mut out = vec![SYSTEM];
        out.extend_from_slice(&tenant.to_be_bytes());
        out.extend_from_slice(&table.to_be_bytes());
        out.extend_from_slice(key);
        Key(out)
    }

    /// Data local to a range (its applied state, its lease…).
    pub fn range_local(range: u64, kind: u8, rest: &[u8]) -> Key {
        let mut out = vec![RANGE_LOCAL];
        out.extend_from_slice(&range.to_be_bytes());
        out.push(kind);
        out.extend_from_slice(rest);
        Key(out)
    }

    /// A range descriptor in a meta-range of level 1 or 2, by the range's end (007).
    pub fn meta(level: u8, range_end: &[u8]) -> Key {
        assert!(level == 1 || level == 2, "meta-ranges have two levels");
        let mut out = vec![META, level];
        out.extend_from_slice(range_end);
        Key(out)
    }
}

/// The value of a unique index entry: the length of the document's primary key (4 bytes,
/// big-endian), the primary key and the type bits of the values.
pub fn unique_value(pk: &PrimaryKey, bits: &TypeBits) -> Vec<u8> {
    let len = u32::try_from(pk.0.len()).expect("a primary key fits in a document");
    let mut out = len.to_be_bytes().to_vec();
    out.extend_from_slice(&pk.0);
    out.extend_from_slice(bits.as_bytes());
    out
}

/// The primary key and the type bits of a unique index entry's value ([`unique_value`]).
pub fn split_unique_value(value: &[u8]) -> Result<(&[u8], TypeBits), KeyError> {
    const SHORT: KeyError = KeyError("unique index value shorter than its primary key");
    let (len, rest) = take::<4>(value).map_err(|_| SHORT)?;
    let len = u32::from_be_bytes(len) as usize;
    if rest.len() < len {
        return Err(SHORT);
    }
    let (pk, bits) = rest.split_at(len);
    Ok((pk, TypeBits::from_bytes(bits.to_vec())))
}

/// `01 · tenant · collection · index · [bucket] · enc(values)` and the values' type bits.
fn index_values(
    tenant: u32,
    collection: u32,
    index: u32,
    spec: &IndexKeySpec,
    values: &[RawValue<'_>],
    collation: Option<&dyn Collator>,
) -> (Vec<u8>, TypeBits) {
    assert_ne!(index, DOCUMENTS, "index 0 is the documents'");
    assert_eq!(values.len(), spec.fields.len(), "one value per field");
    let mut out = data(tenant, collection, index);
    if spec.buckets > 0 {
        out.push(bucket_of_bytes(&ascending_with(values, collation), spec.buckets));
    }
    let mut bits = TypeBits::default();
    for (v, f) in values.iter().zip(&spec.fields) {
        encode_value_with(&mut out, *v, f.direction, &mut bits, collation);
    }
    (out, bits)
}

/// A key taken apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyParts<'a> {
    /// Range-local data.
    RangeLocal {
        /// Range.
        range: u64,
        /// What it is.
        kind: u8,
        /// The rest.
        rest: &'a [u8],
    },
    /// A document (index 0) or an index entry.
    Data {
        /// Tenant.
        tenant: u32,
        /// Collection.
        collection: u32,
        /// Index (0 for the documents).
        index: u32,
        /// The rest: bucket, values and primary key.
        rest: &'a [u8],
    },
    /// A transaction record.
    Txn {
        /// Transaction.
        id: &'a [u8],
    },
    /// A row of a system table.
    System {
        /// Tenant.
        tenant: u32,
        /// Table.
        table: u32,
        /// Row.
        key: &'a [u8],
    },
    /// A range descriptor.
    Meta {
        /// Level, 1 or 2.
        level: u8,
        /// The range's end.
        end: &'a [u8],
    },
}

impl<'a> KeyParts<'a> {
    /// Takes `key` apart by its space.
    pub fn of(key: &'a [u8]) -> Result<KeyParts<'a>, KeyError> {
        let (&first, rest) = key.split_first().ok_or(KeyError("empty"))?;
        match first {
            RANGE_LOCAL => {
                let (range, rest) = take::<8>(rest)?;
                let (&kind, rest) =
                    rest.split_first().ok_or(KeyError("range-local key without kind"))?;
                Ok(KeyParts::RangeLocal { range: u64::from_be_bytes(range), kind, rest })
            }
            DATA => {
                let (tenant, rest) = take::<4>(rest)?;
                let (collection, rest) = take::<4>(rest)?;
                let (index, rest) = take::<4>(rest)?;
                Ok(KeyParts::Data {
                    tenant: u32::from_be_bytes(tenant),
                    collection: u32::from_be_bytes(collection),
                    index: u32::from_be_bytes(index),
                    rest,
                })
            }
            TXN => Ok(KeyParts::Txn { id: rest }),
            SYSTEM => {
                let (tenant, rest) = take::<4>(rest)?;
                let (table, key) = take::<4>(rest)?;
                Ok(KeyParts::System {
                    tenant: u32::from_be_bytes(tenant),
                    table: u32::from_be_bytes(table),
                    key,
                })
            }
            META => match rest.split_first() {
                Some((&level @ (1 | 2), end)) => Ok(KeyParts::Meta { level, end }),
                _ => Err(KeyError("meta-range key without level 1 or 2")),
            },
            _ => Err(KeyError("unknown key space")),
        }
    }
}

/// `01 · tenant · collection · index`.
fn data(tenant: u32, collection: u32, index: u32) -> Vec<u8> {
    let mut out = collection_prefix(tenant, collection);
    out.extend_from_slice(&index.to_be_bytes());
    out
}

/// Room for a key's prefix and a few short values without growing.
const KEY_CAPACITY: usize = 96;

/// Bytes of `01 · tenant · collection`.
const COLLECTION_PREFIX_LEN: usize = 9;

/// The `01 · tenant · collection` a key of the data space starts with: what its collection's
/// documents and index entries share (004 FR-022, the dictionary of the collection); `None`
/// outside that space.
pub fn collection_of(key: &[u8]) -> Option<&[u8]> {
    key.get(..COLLECTION_PREFIX_LEN).filter(|p| p[0] == DATA)
}

/// `01 · tenant · collection`.
fn collection_prefix(tenant: u32, collection: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(KEY_CAPACITY);
    out.push(DATA);
    out.extend_from_slice(&tenant.to_be_bytes());
    out.extend_from_slice(&collection.to_be_bytes());
    out
}

/// The values encoded ascending, without type bits.
fn ascending(values: &[RawValue<'_>]) -> Vec<u8> {
    ascending_with(values, None)
}

fn ascending_with(values: &[RawValue<'_>], collation: Option<&dyn Collator>) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        encode_value_with(&mut out, *v, Direction::Ascending, &mut TypeBits::default(), collation);
    }
    out
}

/// `N` bytes from the front.
fn take<const N: usize>(b: &[u8]) -> Result<([u8; N], &[u8]), KeyError> {
    let head = b.get(..N).ok_or(KeyError("key cut short"))?;
    Ok((head.try_into().expect("N bytes"), &b[N..]))
}
