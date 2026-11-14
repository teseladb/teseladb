//! The partial groups of a `$group` (010 FR-034) and their bytes, as they travel from the leader
//! of a range on another node to the coordinator of an aggregation: each group's canonical key,
//! its key and its accumulators' states, bit for bit, so that the coordinator merges them as it
//! merges those of its own cores.

use teseladb_bson::{Document, Limits, RawDoc, Value};

use super::Group;
use crate::accum::Part;

/// The groups of a `$group` over one part of its input, before their values are finished (010
/// FR-034): in the order they appeared, each with its canonical key, its key and its
/// accumulators' states. The groups of consecutive parts merge into those of the whole
/// ([`Group::merge_parts`]).
#[derive(Clone, Debug, Default)]
pub struct PartialGroups {
    pub(super) list: Vec<(Vec<u8>, Value, Vec<Part>)>,
}

impl PartialGroups {
    /// How many groups.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

impl Group {
    /// Whether `states` hold one state of the kind of each of the group's accumulators, as a part
    /// decoded from another node has to before it merges.
    pub(super) fn holds(&self, states: &[Part]) -> bool {
        states.len() == self.outputs.len()
            && self.outputs.iter().zip(states).all(|((_, acc), st)| acc.holds(st))
    }
}

/// Levels the encoding adds above a group's key: the document, its list, the group's entry.
const WRAPPING_DEPTH: usize = 4;

/// The limits of the encoding: any group key a stored document holds, at any depth, and a table
/// of groups up to the bytes a reply carries.
pub(crate) fn wire_limits() -> Limits {
    Limits { max_size: usize::MAX >> 1, max_depth: teseladb_bson::MAX_DEPTH + WRAPPING_DEPTH }
}

impl PartialGroups {
    /// The groups in bytes; `None` when they cannot be written (a key that is not BSON).
    pub fn encode(&self) -> Option<Vec<u8>> {
        let groups = self
            .list
            .iter()
            .map(|(canon, key, parts)| {
                let parts = parts.iter().map(Part::to_value).collect();
                Value::Array(vec![
                    Value::Binary { subtype: 0, bytes: canon.clone() },
                    key.clone(),
                    Value::Array(parts),
                ])
            })
            .collect();
        Document::new().with("g", Value::Array(groups)).to_bytes_with(wire_limits()).ok()
    }

    /// What [`PartialGroups::encode`] wrote; `None` for anything else.
    pub fn decode(bytes: &[u8]) -> Option<PartialGroups> {
        let doc = Document::from(RawDoc::from_bytes_with(bytes, wire_limits()).ok()?);
        if doc.len() != 1 {
            return None;
        }
        let Some(Value::Array(groups)) = doc.get("g") else { return None };
        let mut list = Vec::with_capacity(groups.len());
        for g in groups {
            let Value::Array(entry) = g else { return None };
            let [Value::Binary { subtype: 0, bytes: canon }, key, Value::Array(parts)] =
                entry.as_slice()
            else {
                return None;
            };
            let parts: Option<Vec<Part>> = parts.iter().map(Part::from_value).collect();
            list.push((canon.clone(), key.clone(), parts?));
        }
        Some(PartialGroups { list })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn anything_but_an_encoding_does_not_decode() {
        let not = |d: Document| PartialGroups::decode(&d.to_bytes().unwrap()).is_none();
        assert!(PartialGroups::decode(b"junk").is_none());
        assert!(not(Document::new()));
        assert!(not(Document::new().with("g", 1)));
        assert!(not(Document::new().with("g", Value::Array(vec![1.into()]))));
        assert!(not(Document::new().with("g", Value::Array(vec![Value::Array(vec![])]))));
        let bad_part = Value::Array(vec![
            Value::Binary { subtype: 0, bytes: vec![1] },
            Value::Null,
            Value::Array(vec![Value::Array(vec![Value::Int32(9), Value::Null])]),
        ]);
        assert!(not(Document::new().with("g", Value::Array(vec![bad_part]))));
        let two = Document::new().with("g", Value::Array(vec![])).with("h", 1);
        assert!(not(two));
        let empty = Document::new().with("g", Value::Array(vec![])).to_bytes().unwrap();
        assert!(PartialGroups::decode(&empty).is_some_and(|p| p.is_empty()));
    }
}
