//! The local identifiers of a range's documents (009 FR-023): the leader gives each new document
//! of a write the next numbers of the range before it proposes the write, and every replica raises
//! its next number with the entry, so a later leader starts past every number given.
//!
//! The upper layer writes a provisional number in each new document's value and in each of its
//! index entries ([`teseladb_keys::local_id::provisional`]), and [`fill`] replaces them with the
//! range's numbers, the same for every value of one document. A write that does not go through the
//! leader's log of the range (a transaction's intents) or that the cluster cannot propose with the
//! numbers yet gets [`settle`]: its values keep [`teseladb_keys::local_id::NO_LOCAL_ID`].

use teseladb_keys::local_id::{
    FIRST_PROVISIONAL, INDEX_ENTRY_VALUE, NO_LOCAL_ID, document_id_at, entry_id_at, set_at, slot,
};
use teseladb_keys::space::in_documents;

use crate::command::Put;

/// Where a value holds a provisional number: the put and the offset in its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hole {
    /// The put, by its position in the write.
    pub put: u32,
    /// The offset of the number in the put's value.
    pub at: u32,
}

/// The places of `puts` that hold a provisional number: a document's value (its key in the
/// documents of a collection, of value type [`teseladb_lsm::DOCUMENT_VALUE`]) or an index entry's
/// ([`INDEX_ENTRY_VALUE`]).
pub fn holes(puts: &[Put]) -> Vec<Hole> {
    let mut out = Vec::new();
    for (i, p) in puts.iter().enumerate() {
        let at = match p.value_type {
            teseladb_lsm::DOCUMENT_VALUE if in_documents(&p.key) => document_id_at(&p.value),
            INDEX_ENTRY_VALUE if p.key.first() == Some(&teseladb_keys::space::DATA) => {
                entry_id_at(&p.value)
            }
            _ => None,
        };
        let (Some(at), Ok(put)) = (at, u32::try_from(i)) else { continue };
        let hole = Hole { put, at: u32::try_from(at).unwrap_or(u32::MAX) };
        if provisional_at(puts, &hole).is_some() {
            out.push(hole);
        }
    }
    out
}

/// The provisional number at `hole` of `puts`.
fn provisional_at(puts: &[Put], hole: &Hole) -> Option<u32> {
    let p = puts.get(hole.put as usize)?;
    let at = hole.at as usize;
    let bytes: [u8; 4] = p.value.get(at..at + 4)?.try_into().ok()?;
    slot(u32::from_le_bytes(bytes))
}

/// Gives the new documents of `puts` the numbers from `next` on: the document of slot `s` gets
/// `next + s`. Returns the range's next number after them, or `next` when no hole holds a
/// provisional number. A number past the range's last one leaves its hole without number.
pub fn fill(puts: &mut [Put], next: u32) -> u32 {
    let mut end = next;
    for h in &holes(puts) {
        let Some(s) = provisional_at(puts, h) else { continue };
        let id = next.checked_add(s).filter(|id| *id < FIRST_PROVISIONAL).unwrap_or(NO_LOCAL_ID);
        if id != NO_LOCAL_ID {
            end = end.max(id + 1);
        }
        set_at(&mut puts[h.put as usize].value, h.at as usize, id);
    }
    end
}

/// Leaves every hole of `puts` without number.
pub fn settle(puts: &mut [Put]) {
    for h in holes(puts) {
        set_at(&mut puts[h.put as usize].value, h.at as usize, NO_LOCAL_ID);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teseladb_keys::local_id::{
        document_id, entry_id, provisional, with_document_id, with_entry_id,
    };
    use teseladb_keys::{KeyBuilder, space::DOCUMENTS};

    fn doc_key(i: u8) -> Vec<u8> {
        let mut k = KeyBuilder::index_prefix(1, 2, DOCUMENTS).as_bytes().to_vec();
        k.push(i);
        k
    }

    fn entry_key(i: u8) -> Vec<u8> {
        let mut k = KeyBuilder::index_prefix(1, 2, 3).as_bytes().to_vec();
        k.push(i);
        k
    }

    fn write(slots: &[u32]) -> Vec<Put> {
        let mut puts = Vec::new();
        for (i, s) in slots.iter().enumerate() {
            let i = u8::try_from(i).unwrap();
            let doc = with_document_id(vec![5, 0, 0, 0, 0], provisional(*s));
            puts.push(Put {
                key: doc_key(i),
                value_type: teseladb_lsm::DOCUMENT_VALUE,
                value: doc,
            });
            let entry = with_entry_id(provisional(*s), &[2]);
            puts.push(Put { key: entry_key(i), value_type: INDEX_ENTRY_VALUE, value: entry });
        }
        puts
    }

    #[test]
    fn each_new_document_and_its_entries_get_one_number_of_the_range() {
        let mut puts = write(&[0, 1, 2]);
        assert_eq!(holes(&puts).len(), 6);
        assert_eq!(fill(&mut puts, 40), 43);
        let docs: Vec<_> = puts.iter().step_by(2).map(|p| document_id(&p.value)).collect();
        let entries: Vec<_> = puts.iter().skip(1).step_by(2).map(|p| entry_id(&p.value)).collect();
        assert_eq!(docs, vec![Some(40), Some(41), Some(42)]);
        assert_eq!(entries, docs);
        // Filled once, the numbers are no longer provisional: a second fill keeps them.
        let again = puts.clone();
        assert!(holes(&puts).is_empty());
        assert_eq!(fill(&mut puts, 90), 90);
        assert_eq!(puts, again);
    }

    #[test]
    fn a_write_that_cannot_number_its_documents_leaves_them_without() {
        let mut puts = write(&[0, 1]);
        settle(&mut puts);
        assert!(
            puts.iter().all(|p| document_id(&p.value).is_none() && entry_id(&p.value).is_none())
        );
        let mut puts = write(&[0, 1]);
        assert_eq!(fill(&mut puts, FIRST_PROVISIONAL - 1), FIRST_PROVISIONAL);
        assert_eq!(document_id(&puts[0].value), Some(FIRST_PROVISIONAL - 1));
        assert_eq!(document_id(&puts[2].value), None, "past the range's last number");
    }

    #[test]
    fn only_documents_and_index_entries_hold_numbers() {
        let value = with_document_id(vec![5, 0, 0, 0, 0], provisional(0));
        let other = [
            Put {
                key: vec![0, 1, 2],
                value_type: teseladb_lsm::DOCUMENT_VALUE,
                value: value.clone(),
            },
            Put {
                key: entry_key(1),
                value_type: teseladb_lsm::DOCUMENT_VALUE,
                value: value.clone(),
            },
            Put { key: doc_key(1), value_type: 7, value: value.clone() },
            Put { key: vec![0, 1], value_type: INDEX_ENTRY_VALUE, value: value.clone() },
            Put { key: entry_key(2), value_type: INDEX_ENTRY_VALUE, value: vec![2, 0] },
        ];
        assert!(holes(&other).is_empty());
    }
}
