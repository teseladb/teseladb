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

    /// `[start, end)` read in every partition: the same interval after the collection number in
    /// each one's. `start` may carry any of the collection's numbers (a read that goes on from a
    /// key it gave); `end` carries the first one, or any of them with the rest of a key (a read
    /// backward that goes on below a key it gave), or the next number with nothing after it, the
    /// end of the collection, which is then the end of each partition. `None` when the interval
    /// is not inside the collection's keys.
    pub fn spans(&self, start: &[u8], end: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let (s, start_rest) = split_number(start)?;
        let (e, end_rest) = split_number(end)?;
        if !self.contains(s) || start[..5] != end[..5] {
            return None;
        }
        let past = match e.checked_sub(self.first)? {
            0 => false,
            1 if end_rest.is_empty() => true,
            _ if self.contains(e) && !end_rest.is_empty() => false,
            _ => return None,
        };
        let spans = self
            .numbers()
            .map(|n| {
                let lo = with_number(start, n, start_rest);
                let hi = match past {
                    true => with_number(end, n + 1, &[]),
                    false => with_number(end, n, end_rest),
                };
                (lo, hi)
            })
            .collect();
        Some(spans)
    }
}

/// Bytes of `01 · tenant` before a data key's collection number.
const NUMBER_AT: usize = 5;

/// The collection number of data key `key` and the bytes after it.
fn split_number(key: &[u8]) -> Option<(u32, &[u8])> {
    if key.first() != Some(&crate::space::DATA) {
        return None;
    }
    let n = u32::from_be_bytes(key.get(NUMBER_AT..NUMBER_AT + 4)?.try_into().ok()?);
    Some((n, &key[NUMBER_AT + 4..]))
}

/// `key`'s first bytes with collection number `n` and `rest` after it.
fn with_number(key: &[u8], n: u32, rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(NUMBER_AT + 4 + rest.len());
    out.extend_from_slice(&key[..NUMBER_AT]);
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(rest);
    out
}

/// Data key `key` under collection number `n`, with the same bytes after the number: a key one of
/// a collection's partitions gave, in the intervals of the collection's own number, which the
/// partitions' reads take as their start. `None` when `key` is not a data key.
pub fn with_collection_number(key: &[u8], n: u32) -> Option<Vec<u8>> {
    let (_, rest) = split_number(key)?;
    Some(with_number(key, n, rest))
}

/// The bytes of data key `key` after its collection number: what orders the keys of the
/// partitions of one collection among themselves.
pub fn after_number(key: &[u8]) -> &[u8] {
    key.get(NUMBER_AT + 4..).unwrap_or_default()
}

/// The rows of the partitions' reads `lists`, each in key order (backward: from the highest),
/// merged in the order of their keys after the collection number, at most `limit`.
pub fn merge<V>(lists: Vec<Vec<(Vec<u8>, V)>>, limit: usize, backward: bool) -> Vec<(Vec<u8>, V)> {
    let mut lanes: Vec<std::collections::VecDeque<(Vec<u8>, V)>> =
        lists.into_iter().map(Into::into).collect();
    let mut out = Vec::new();
    while out.len() < limit {
        let heads = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.front().map(|(k, _)| (i, after_number(k))));
        let next = match backward {
            true => heads.max_by(|a, b| a.1.cmp(b.1)),
            false => heads.min_by(|a, b| a.1.cmp(b.1)),
        };
        let Some((i, _)) = next else { break };
        out.extend(lanes[i].pop_front());
    }
    out
}
