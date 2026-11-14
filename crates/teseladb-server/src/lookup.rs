//! Documents read by their keys in one request to the leader of their range (010 FR-027): the
//! node that recorded an index scan's entries sends the keys of their documents together
//! ([`crate::data_wire::RemoteReq::Lookup`]), with the scan's residual filter when it decides from
//! the document alone. The leader reads each key as a point read on its core and answers, for
//! each, whether the document is missing, the document, or what the filter made of it. A key whose
//! read did not answer a value (an intent, a version the leader does not serve) answers nothing,
//! and the node that asked reads it on its own. A filter that does not read or fails on a document
//! leaves every document unevaluated, for the node that asked to give the error.

use teseladb_range::HISTORY_VALUE;
use teseladb_range::host::RangeError;
use teseladb_types::{DecodeError, Decoder, Encoder};
use teseladb_wire::commands::Fetched;
use teseladb_wire::commands::pushed_filter::SentFilter;

use crate::data_wire::RemoteReply;

/// What the leader read for each key of a lookup: `None` for a key the node that asked reads on
/// its own.
pub type Looked = Vec<Option<Fetched>>;

const AGAIN: u8 = 0;
const MISSING: u8 = 1;
const DOC: u8 = 2;
const KEPT: u8 = 3;
const REFUSED: u8 = 4;

/// The answer to a lookup of `keys` from the reads of each key, in order, with `filter` in BSON
/// when the node that asked sent one. A replica that does not lead the range refuses the whole
/// lookup, so that the node's handle sends it on to the leader.
pub(crate) fn answer(
    keys: &[Vec<u8>],
    reads: Vec<Result<RemoteReply, RangeError>>,
    filter: Option<&[u8]>,
) -> Result<RemoteReply, RangeError> {
    if let Some(Err(refused)) =
        reads.iter().find(|r| matches!(r, Err(RangeError::NotLeader { .. })))
    {
        return Err(refused.clone());
    }
    let found: Looked = reads
        .into_iter()
        .map(|r| match r {
            // A version that only serves the history is no document, as in a point read.
            Ok(RemoteReply::Value { value, .. }) => {
                Some(match value.filter(|(t, _)| *t != HISTORY_VALUE) {
                    Some((_, doc)) => Fetched::Doc(doc),
                    None => Fetched::Missing,
                })
            }
            _ => None,
        })
        .collect();
    Ok(RemoteReply::Looked(match filter {
        Some(filter) => filtered(keys, found, filter),
        None => found,
    }))
}

/// `found` with each document the filter decided as kept or refused; as it was when the filter
/// does not read or fails on one of them.
fn filtered(keys: &[Vec<u8>], found: Looked, filter: &[u8]) -> Looked {
    let rows: Vec<(Vec<u8>, Vec<u8>)> = keys
        .iter()
        .zip(&found)
        .filter_map(|(k, f)| match f {
            Some(Fetched::Doc(doc)) => Some((k.clone(), doc.clone())),
            _ => None,
        })
        .collect();
    let first = keys.first().map_or(&[][..], Vec::as_slice);
    let Some(kept) = SentFilter::parse(filter, first).and_then(|f| f.keep(rows)) else {
        return found;
    };
    // The place of each document among the rows, from one: `seen` names those kept.
    let mut kept = kept.seen.into_iter().peekable();
    let mut row = 0;
    found
        .into_iter()
        .map(|f| match f {
            Some(Fetched::Doc(doc)) => {
                row += 1;
                match kept.next_if_eq(&row) {
                    Some(_) => Some(Fetched::Kept(doc)),
                    None => Some(Fetched::Refused),
                }
            }
            other => other,
        })
        .collect()
}

/// Writes `found` at the end of `e`.
pub(crate) fn put(found: &Looked, e: &mut Encoder) {
    e.var(found.len() as u64);
    for f in found {
        match f {
            None => {
                e.u8(AGAIN);
            }
            Some(Fetched::Missing) => {
                e.u8(MISSING);
            }
            Some(Fetched::Doc(doc)) => {
                e.u8(DOC).bytes(doc);
            }
            Some(Fetched::Kept(doc)) => {
                e.u8(KEPT).bytes(doc);
            }
            Some(Fetched::Refused) => {
                e.u8(REFUSED);
            }
        }
    }
}

/// Reads what [`put`] wrote.
pub(crate) fn get(d: &mut Decoder<'_>) -> Result<Looked, DecodeError> {
    let at = d.position();
    let n = d.var()?;
    if n > d.remaining() as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    (0..n)
        .map(|_| {
            let at = d.position();
            Ok(match d.u8()? {
                AGAIN => None,
                MISSING => Some(Fetched::Missing),
                DOC => Some(Fetched::Doc(d.bytes()?.to_vec())),
                KEPT => Some(Fetched::Kept(d.bytes()?.to_vec())),
                REFUSED => Some(Fetched::Refused),
                _ => return Err(DecodeError::InvalidValue { offset: at }),
            })
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_bson::Document;
    use teseladb_hlc::Hlc;
    use teseladb_types::NodeId;

    fn value(doc: Option<Vec<u8>>) -> Result<RemoteReply, RangeError> {
        Ok(RemoteReply::Value { ts: Hlc { physical: 1, logical: 0 }, value: doc.map(|d| (0, d)) })
    }

    fn doc(v: i32) -> Vec<u8> {
        Document::new().with("v", v).to_bytes().unwrap()
    }

    fn keys(n: u8) -> Vec<Vec<u8>> {
        (0..n).map(|i| vec![i]).collect()
    }

    #[test]
    fn each_key_answers_its_document_or_nothing() {
        let reads = vec![
            value(Some(doc(1))),
            value(None),
            Ok(RemoteReply::Value {
                ts: Hlc { physical: 1, logical: 0 },
                value: Some((HISTORY_VALUE, doc(2))),
            }),
            Err(RangeError::Unavailable),
        ];
        let Ok(RemoteReply::Looked(found)) = answer(&keys(4), reads, None) else { panic!() };
        assert_eq!(
            found,
            vec![Some(Fetched::Doc(doc(1))), Some(Fetched::Missing), Some(Fetched::Missing), None]
        );
    }

    #[test]
    fn a_replica_that_does_not_lead_refuses_the_whole_lookup() {
        let hint = NodeId(3);
        let reads = vec![value(None), Err(RangeError::NotLeader { hint })];
        assert_eq!(answer(&keys(2), reads, None), Err(RangeError::NotLeader { hint }));
    }

    #[test]
    fn the_filter_keeps_or_refuses_each_document() {
        let filter = Document::new().with("v", 1).to_bytes().unwrap();
        let reads =
            vec![value(Some(doc(1))), value(None), value(Some(doc(2))), value(Some(doc(1)))];
        let Ok(RemoteReply::Looked(found)) = answer(&keys(4), reads, Some(&filter)) else {
            panic!()
        };
        assert_eq!(
            found,
            vec![
                Some(Fetched::Kept(doc(1))),
                Some(Fetched::Missing),
                Some(Fetched::Refused),
                Some(Fetched::Kept(doc(1))),
            ]
        );
    }

    #[test]
    fn a_filter_that_does_not_read_leaves_the_documents_unevaluated() {
        let reads = vec![value(Some(doc(1))), value(Some(vec![9, 0, 0]))];
        let filter = Document::new().with("v", 1).to_bytes().unwrap();
        let Ok(RemoteReply::Looked(found)) = answer(&keys(2), reads.clone(), Some(b"junk")) else {
            panic!()
        };
        assert_eq!(found[0], Some(Fetched::Doc(doc(1))));
        // A document that does not read: nothing is decided, for the node that asked to fail on it.
        let Ok(RemoteReply::Looked(found)) = answer(&keys(2), reads, Some(&filter)) else {
            panic!()
        };
        assert_eq!(found, vec![Some(Fetched::Doc(doc(1))), Some(Fetched::Doc(vec![9, 0, 0]))]);
    }

    #[test]
    fn the_answers_read_back_and_nothing_else_does() {
        let found: Looked = vec![
            None,
            Some(Fetched::Missing),
            Some(Fetched::Doc(vec![1, 2])),
            Some(Fetched::Kept(Vec::new())),
            Some(Fetched::Refused),
        ];
        let mut e = Encoder::new();
        put(&found, &mut e);
        let b = e.finish();
        assert_eq!(get(&mut Decoder::new(&b)).unwrap(), found);
        assert!(get(&mut Decoder::new(&[1, 9])).is_err());
        assert!(get(&mut Decoder::new(&[200])).is_err());
    }
}
