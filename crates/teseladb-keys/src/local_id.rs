//! Local identifiers of documents (009 FR-023): a number of the range, given to a document when it
//! is inserted, that its stored value and each of its index entries carry, so that the entries of
//! an index give sets of 32-bit numbers to intersect and join without reading the documents.
//!
//! - A stored document carries it at its end: `· id (u32, little-endian) · D8`. The last byte of a
//!   stored document is otherwise the BSON's final zero or the table's mark `D7`, so `D8` says the
//!   identifier is there.
//! - An index entry carries it at the start of its value: `D8 · id (u32, little-endian) · rest`,
//!   where `rest` is the value it had without one. Type bits start with a tag below 8 and the value
//!   of a unique entry with the big-endian length of a primary key, whose first byte is never `D8`.
//!
//! [`NO_LOCAL_ID`] is the number of a document that has none (a transaction's write, or a version
//! written while some node of the cluster did not read the format): its entries never join a set,
//! and a plan that meets one reads its document. Numbers from [`FIRST_PROVISIONAL`] up are the
//! places a write leaves for the range to fill when it proposes it: the leader replaces each with
//! the next number of the range ([`provisional`], [`slot`]).

/// The value type of an index entry (a document's is 1).
pub const INDEX_ENTRY_VALUE: u8 = 2;
/// The byte that marks a local identifier.
pub const LOCAL_ID_MARK: u8 = 0xD8;
/// Bytes the identifier takes with its mark.
pub const LOCAL_ID_LEN: usize = 5;
/// The number of a document without a local identifier.
pub const NO_LOCAL_ID: u32 = u32::MAX;
/// The first provisional number: a range gives numbers below it.
pub const FIRST_PROVISIONAL: u32 = 0xFFF0_0000;

/// The provisional number of the `n`-th new document of a write; [`NO_LOCAL_ID`] past the last one.
#[must_use]
pub fn provisional(n: u32) -> u32 {
    match FIRST_PROVISIONAL.checked_add(n) {
        Some(id) if id < NO_LOCAL_ID => id,
        _ => NO_LOCAL_ID,
    }
}

/// Which new document of its write a provisional number stands for; `None` for any other number.
#[must_use]
pub fn slot(id: u32) -> Option<u32> {
    (FIRST_PROVISIONAL..NO_LOCAL_ID).contains(&id).then(|| id - FIRST_PROVISIONAL)
}

/// Whether `id` is a number a range gave.
#[must_use]
pub fn is_given(id: u32) -> bool {
    id < FIRST_PROVISIONAL
}

/// A stored document's value with its identifier `id` at the end.
#[must_use]
pub fn with_document_id(mut stored: Vec<u8>, id: u32) -> Vec<u8> {
    stored.extend_from_slice(&id.to_le_bytes());
    stored.push(LOCAL_ID_MARK);
    stored
}

/// Where the identifier of a stored document starts, when it carries one.
#[must_use]
pub fn document_id_at(stored: &[u8]) -> Option<usize> {
    match stored.last() {
        Some(&LOCAL_ID_MARK) => stored.len().checked_sub(LOCAL_ID_LEN),
        _ => None,
    }
}

/// The identifier a stored document carries, given or provisional; `None` without one or with
/// [`NO_LOCAL_ID`].
#[must_use]
pub fn document_id(stored: &[u8]) -> Option<u32> {
    let at = document_id_at(stored)?;
    read(stored, at)
}

/// A stored document's value without its identifier.
#[must_use]
pub fn document_body(stored: &[u8]) -> &[u8] {
    match document_id_at(stored) {
        Some(at) => &stored[..at],
        None => stored,
    }
}

/// An index entry's value `rest` with identifier `id` before it.
#[must_use]
pub fn with_entry_id(id: u32, rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LOCAL_ID_LEN + rest.len());
    out.push(LOCAL_ID_MARK);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(rest);
    out
}

/// Where the identifier of an index entry's value starts, when it carries one.
#[must_use]
pub fn entry_id_at(value: &[u8]) -> Option<usize> {
    (value.len() >= LOCAL_ID_LEN && value[0] == LOCAL_ID_MARK).then_some(1)
}

/// The identifier an index entry carries, given or provisional; `None` without one or with
/// [`NO_LOCAL_ID`].
#[must_use]
pub fn entry_id(value: &[u8]) -> Option<u32> {
    read(value, entry_id_at(value)?)
}

/// An index entry's value without its identifier: the type bits, or the primary key and type bits
/// of a unique entry.
#[must_use]
pub fn entry_rest(value: &[u8]) -> &[u8] {
    match entry_id_at(value) {
        Some(_) => &value[LOCAL_ID_LEN..],
        None => value,
    }
}

/// The identifier `value` holds at `at`, `None` for [`NO_LOCAL_ID`].
fn read(value: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = value.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes)).filter(|id| *id != NO_LOCAL_ID)
}

/// `value` with the identifier at `at` replaced by `id`.
///
/// # Panics
///
/// When `value` has no four bytes at `at`: the place came from [`document_id_at`] or
/// [`entry_id_at`] of this same value.
pub fn set_at(value: &mut [u8], at: usize, id: u32) {
    value[at..at + 4].copy_from_slice(&id.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_keeps_its_bytes_and_carries_its_number_at_the_end() {
        let bson = vec![5, 0, 0, 0, 0];
        let stored = with_document_id(bson.clone(), 42);
        assert_eq!(document_id(&stored), Some(42));
        assert_eq!(document_body(&stored), bson.as_slice());
        assert_eq!(document_id(&bson), None);
        assert_eq!(document_body(&bson), bson.as_slice());
        let table = [&bson[..], &[1, 0, 1, 0xD7]].concat();
        assert_eq!(document_id(&table), None, "a table's mark is not an identifier");
        let none = with_document_id(bson, NO_LOCAL_ID);
        assert_eq!(document_id(&none), None);
        assert_eq!(document_id_at(&none), Some(5));
    }

    #[test]
    fn a_stored_document_with_its_number_reads_as_before() {
        let mut doc = teseladb_bson::Document::new();
        for i in 0..10 {
            doc.push(format!("f{i}"), i);
        }
        let bson = doc.to_bytes().unwrap();
        for stored in [crate::stored::encode(bson.clone()), bson.clone()] {
            let with = with_document_id(stored.clone(), 11);
            assert_eq!(crate::stored::bson(&with).unwrap(), bson.as_slice());
            let a = crate::stored::split(&stored).unwrap();
            let b = crate::stored::split(&with).unwrap();
            assert_eq!(a.table().is_some(), b.table().is_some());
            assert_eq!(crate::stored::into_bson(with).unwrap(), bson);
        }
        let unique = crate::space::unique_value(
            &crate::PrimaryKey::from_bytes(vec![1, 2, 3]),
            &crate::TypeBits::default(),
        );
        let entry = with_entry_id(4, &unique);
        let (pk, _) = crate::space::split_unique_value(&entry).unwrap();
        assert_eq!(pk, &[1, 2, 3]);
    }

    #[test]
    fn an_entry_carries_its_number_before_its_old_value() {
        for rest in [&[][..], &[0, 2, 1][..], &[0, 0, 0, 3, 9, 9, 9][..]] {
            let v = with_entry_id(7, rest);
            assert_eq!(entry_id(&v), Some(7));
            assert_eq!(entry_rest(&v), rest);
            assert_eq!(entry_id(rest), None);
            assert_eq!(entry_rest(rest), rest);
        }
        let mut v = with_entry_id(provisional(3), &[2]);
        assert_eq!(entry_id(&v).and_then(slot), Some(3));
        set_at(&mut v, 1, 99);
        assert_eq!(entry_id(&v), Some(99));
        assert!(entry_id(&with_entry_id(NO_LOCAL_ID, &[])).is_none());
    }

    #[test]
    fn provisional_numbers_never_meet_given_ones() {
        assert_eq!(slot(provisional(0)), Some(0));
        assert_eq!(slot(provisional(5)), Some(5));
        assert_eq!(provisional(u32::MAX), NO_LOCAL_ID);
        assert_eq!(provisional(NO_LOCAL_ID - FIRST_PROVISIONAL), NO_LOCAL_ID);
        assert_eq!(slot(NO_LOCAL_ID), None);
        assert_eq!(slot(FIRST_PROVISIONAL - 1), None);
        assert!(is_given(0) && is_given(FIRST_PROVISIONAL - 1));
        assert!(!is_given(FIRST_PROVISIONAL) && !is_given(NO_LOCAL_ID));
    }
}
