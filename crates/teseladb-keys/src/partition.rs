//! Collections spread over partitions (003 FR-012, D-186). The catalog gives a collection `count`
//! consecutive collection numbers from `first`; a document goes with all its non-unique index entries
//! under the number of its partition, `crc32c(primary key) mod count`, so that each partition is one
//! interval with its documents and their entries and a write touches a single range. A unique index
//! other than `_id` lives whole in partition 0, where its uniqueness is checked on one key.

use crate::bucket::bucket_of_bytes;
use crate::space::{Key, KeyBuilder, PrimaryKey};

/// The most partitions a collection is spread over.
pub const MAX_PARTITIONS: u8 = 64;

/// The collection numbers of a collection and how its documents fall among them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partitions {
    first: u32,
    count: u8,
}

impl Partitions {
    /// `count` partitions with the numbers `first..first + count`.
    ///
    /// # Panics
    ///
    /// If `count` is 0 or more than [`MAX_PARTITIONS`], or the numbers pass `u32::MAX`.
    pub fn new(first: u32, count: u8) -> Partitions {
        assert!((1..=MAX_PARTITIONS).contains(&count), "between 1 and 64 partitions, not {count}");
        assert!(first.checked_add(u32::from(count) - 1).is_some(), "partitions past u32::MAX");
        Partitions { first, count }
    }

    /// A collection that is not spread: its one number.
    pub fn single(number: u32) -> Partitions {
        Partitions { first: number, count: 1 }
    }

    /// How many partitions.
    pub fn count(&self) -> u8 {
        self.count
    }

    /// The collection number of the document with primary key `pk` and of its non-unique index
    /// entries.
    pub fn number_of(&self, pk: &PrimaryKey) -> u32 {
        self.number_of_bytes(pk.as_bytes())
    }

    /// [`Self::number_of`] for a primary key's bytes, as an index entry ends in them.
    pub fn number_of_bytes(&self, pk: &[u8]) -> u32 {
        match self.count {
            1 => self.first,
            n => self.first + u32::from(bucket_of_bytes(pk, n)),
        }
    }

    /// The number a unique index other than `_id` keeps all its entries under.
    pub fn unique_number(&self) -> u32 {
        self.first
    }

    /// The document with primary key `pk`, in `_id` order within its partition:
    /// `01 · tenant · (first + partition) · 0 · pk`.
    pub fn document(&self, tenant: u32, pk: &PrimaryKey) -> Key {
        KeyBuilder::document_of_sharded_pk(tenant, self.number_of(pk), pk.as_bytes())
    }

    /// The collection numbers, in order.
    pub fn numbers(&self) -> impl Iterator<Item = u32> + use<> {
        let first = self.first;
        (0..u32::from(self.count)).map(move |p| first + p)
    }

    /// Whether `number` is one of the collection's.
    pub fn contains(&self, number: u32) -> bool {
        self.partition_of_number(number).is_some()
    }

    /// The partition of collection number `number`, if it is one of the collection's.
    pub fn partition_of_number(&self, number: u32) -> Option<u8> {
        let p = number.checked_sub(self.first)?;
        u8::try_from(p).ok().filter(|p| *p < self.count)
    }

    /// The first key of the collection, its documents and indexes in every partition.
    pub fn start(&self, tenant: u32) -> Key {
        KeyBuilder::collection_start(tenant, self.first)
    }

    /// The first key after the collection's last partition.
    pub fn end(&self, tenant: u32) -> Key {
        KeyBuilder::collection_end(tenant, self.first + u32::from(self.count) - 1)
    }
}
