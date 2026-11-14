//! Capped collections on a range (015 FR-003, data model §2, research R1, R5): the documents of
//! a capped collection are keyed by their insertion order (`seq`, 003 FR-012) and live in one
//! range that never cuts the collection (`teseladb_keys::space::cut_outside`). The collection's state (how many bytes
//! and documents it holds and the next `seq`) is a key of its own key span, after its indexes
//! ([`state_key`]), so splits, merges, snapshots and `drop` carry it with the documents; an insert
//! that goes over `size` or `max` deletes the oldest documents in the same `Write`.
//!
//! This module is the arithmetic of that: the limits as the reference server accepts them (`size`
//! from 1 to 1 PiB, used as given: measured on the reference server 7.0.45 and 9.0.4, where `size`
//! 257 evicts at 260 bytes although the documentation rounds it up to a multiple of 256; `max` of 0
//! or less is no limit, and `size` takes precedence), the state and its key, and the plan of an
//! insert, an update and a deletion. The newest document always stays, even alone above `size`. An
//! update may make a document larger and the collection go over `size` (measured on both); the next
//! insert evicts the excess. The writes themselves are evaluated by the layer above, which latches
//! the state key with the documents.

use teseladb_types::{DecodeError, Decoder, Encoder};

/// Index number of a capped collection's state: after every index the catalog numbers.
pub const STATE_INDEX: u32 = u32::MAX;
/// Index number of a capped collection's `_id_` index, which keeps `_id` unique (its documents
/// are keyed by `seq`, not by `_id`).
pub const ID_INDEX: u32 = u32::MAX - 1;
/// First byte of the data key space (`teseladb-keys`).
const DATA: u8 = 0x01;
/// Largest `size` the reference server accepts (1024⁵ bytes).
pub const MAX_CAPPED_SIZE: u64 = 1 << 50;
/// The reference server's code of a `size` out of bounds in `create` (`BadValue` on the reference
/// server 9.0.4; 7.0.45 answered 51024).
pub const BAD_SIZE_CODE: i32 = 2;

/// The limits of a capped collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CappedLimits {
    /// Bytes.
    pub size: u64,
    /// Documents, `None` without a limit.
    pub max: Option<u64>,
}

/// Why a capped write is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CappedError {
    /// `size` is not above 0 or is above [`MAX_CAPPED_SIZE`].
    BadSize,
    /// The oldest documents given do not reach as far as the write needs: the caller scans more.
    NeedOlder,
    /// One of the oldest documents given was deleted since the scan: the caller scans again.
    Stale,
}

/// Every refusal.
const REFUSALS: [CappedError; 3] =
    [CappedError::BadSize, CappedError::NeedOlder, CappedError::Stale];

impl CappedError {
    /// The message of the refusal, which travels in the error of the write.
    pub fn message(self) -> &'static str {
        match self {
            CappedError::BadSize => "capped collection size out of bounds",
            CappedError::NeedOlder => "capped write needs more of the oldest documents",
            CappedError::Stale => "capped write read outdated oldest documents",
        }
    }

    /// The refusal whose message is `msg`, if it is one of these.
    pub fn from_message(msg: &str) -> Option<CappedError> {
        REFUSALS.into_iter().find(|c| c.message() == msg)
    }
}

impl CappedLimits {
    /// The limits of `create` with `size` and `max` as the client sent them.
    pub fn new(size: i64, max: i64) -> Result<CappedLimits, CappedError> {
        let size = u64::try_from(size).map_err(|_| CappedError::BadSize)?;
        if size == 0 || size > MAX_CAPPED_SIZE {
            return Err(CappedError::BadSize);
        }
        let max = u64::try_from(max).ok().filter(|m| *m > 0);
        Ok(CappedLimits { size, max })
    }

    /// The reference server's message for `create` with a `size` out of bounds (measured on the
    /// reference server 9.0.4).
    pub fn bad_size_message(size: i64) -> String {
        if size < 1 {
            format!("BSON field 'size' value must be >= 1, actual value '{size}'")
        } else {
            format!("BSON field 'size' value must be <= {MAX_CAPPED_SIZE}, actual value '{size}'")
        }
    }

    fn over(&self, bytes: u64, docs: u64) -> bool {
        // Faulty variant (015 T010): an insert evicts only past `max`, never past `size`.
        #[cfg(teseladb_bug = "capped_exceeds_size")]
        let bytes = bytes.min(self.size);
        bytes > self.size || self.max.is_some_and(|m| docs > m)
    }
}

/// What the range knows of a capped collection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CappedState {
    /// Bytes of its documents.
    pub bytes: u64,
    /// Documents.
    pub docs: u64,
    /// `seq` the next insert takes.
    pub next: u64,
}

impl CappedState {
    /// Encodes it as the value of its key.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.bytes).u64(self.docs).u64(self.next);
        e.finish()
    }

    /// Decodes a value written by [`CappedState::encode`].
    pub fn decode(b: &[u8]) -> Result<CappedState, DecodeError> {
        let mut d = Decoder::new(b);
        let state = CappedState { bytes: d.u64()?, docs: d.u64()?, next: d.u64()? };
        d.finish()?;
        Ok(state)
    }
}

/// `01 · tenant · collection · index`.
fn index_prefix(tenant: u32, collection: u32, index: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(13);
    k.push(DATA);
    k.extend_from_slice(&tenant.to_be_bytes());
    k.extend_from_slice(&collection.to_be_bytes());
    k.extend_from_slice(&index.to_be_bytes());
    k
}

/// Key of the state of capped collection `(tenant, collection)`: `01 · tenant · collection ·
/// ffffffff`, the last key of the collection's span.
pub fn state_key(tenant: u32, collection: u32) -> Vec<u8> {
    index_prefix(tenant, collection, STATE_INDEX)
}

/// Key of document `seq` of capped collection `(tenant, collection)`: `01 · tenant · collection ·
/// 0 · seq`.
pub fn doc_key(tenant: u32, collection: u32, seq: u64) -> Vec<u8> {
    let mut k = index_prefix(tenant, collection, 0);
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// The `seq` of a document key of capped collection `(tenant, collection)`.
pub fn seq_of(tenant: u32, collection: u32, key: &[u8]) -> Option<u64> {
    let tail = key.strip_prefix(index_prefix(tenant, collection, 0).as_slice())?;
    Some(u64::from_be_bytes(tail.try_into().ok()?))
}

/// The `(tenant, collection)` of a capped collection's state key.
pub fn of_state_key(key: &[u8]) -> Option<(u32, u32)> {
    let (&first, rest) = key.split_first()?;
    let rest: &[u8; 12] = rest.try_into().ok()?;
    let word = |i: usize| u32::from_be_bytes([rest[i], rest[i + 1], rest[i + 2], rest[i + 3]]);
    (first == DATA && word(8) == STATE_INDEX).then(|| (word(0), word(4)))
}

/// An insert: the `seq` of the new document, the `seq`s of the oldest documents it deletes in the
/// same write (oldest first) and the state after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertPlan {
    /// `seq` of the new document.
    pub seq: u64,
    /// Documents deleted.
    pub evict: Vec<u64>,
    /// State after the write.
    pub state: CappedState,
}

/// Plans the insert of a document of `size` bytes. `oldest` holds the collection's oldest
/// documents in order, `(seq, bytes)`, as a scan gave them; it has to reach as far as the insert
/// evicts, or the answer is [`CappedError::NeedOlder`] and the caller reads more.
pub fn plan_insert(
    state: CappedState,
    limits: CappedLimits,
    size: u64,
    oldest: &[(u64, u64)],
) -> Result<InsertPlan, CappedError> {
    let seq = state.next;
    let mut bytes = state.bytes + size;
    let mut docs = state.docs + 1;
    let mut evict = Vec::new();
    let mut older = oldest.iter();
    // The new document always stays: eviction stops when it is the only one left.
    while docs > 1 && limits.over(bytes, docs) {
        let &(s, b) = older.next().ok_or(CappedError::NeedOlder)?;
        evict.push(s);
        bytes = bytes.saturating_sub(b);
        docs -= 1;
    }
    let state = CappedState { bytes, docs, next: seq + 1 };
    Ok(InsertPlan { seq, evict, state })
}

/// Plans an update of a document from `old` to `new` bytes: the state after it.
pub fn plan_update(state: CappedState, old: u64, new: u64) -> CappedState {
    CappedState { bytes: state.bytes.saturating_sub(old) + new, ..state }
}

/// Plans the deletion of a document of `size` bytes: the state after it.
pub fn plan_delete(state: CappedState, size: u64) -> CappedState {
    CappedState {
        bytes: state.bytes.saturating_sub(size),
        docs: state.docs.saturating_sub(1),
        ..state
    }
}
