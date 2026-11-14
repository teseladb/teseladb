//! A scan's filter evaluated by the leader of a range on another node (010 FR-027): the node of
//! the client sends the filter as a document when it decides each document from the document
//! alone and the command has no collation, and the leader compiles it again and keeps only the
//! documents it accepts. The leader orders its conditions by the statistics of the collection, as
//! the node of the client does, and evaluates them a batch at a time on the stored bytes. Behind
//! the filter's BSON go the numbers of its names in the collection's dictionary of field names
//! (003 FR-022), which the leader's program compares.

use std::sync::Arc;

use teseladb_keys::fields::FieldNumbers;

use teseladb_mql::{Filter, FilterUse, MatchContext};
use teseladb_bson::{Document, RawDoc};

use super::Rows;
use super::engines::CollatedExtensions;
use crate::error::CommandError;
use crate::msg::command_limits;

/// The documents of a scan a leader evaluates the filter on at once: a batch of the register
/// machine (010 FR-028).
const BATCH: usize = super::SCAN_BATCH;

/// The filter's bytes when the leader of a range may evaluate it for this node: it decides each
/// document from the document alone ([`Filter::reads_document_alone`]) and there is no collation.
/// The numbers its names have in `fields` follow its BSON.
pub(crate) fn sendable(
    filter: &Document,
    parsed: &Filter,
    collation: Option<&Document>,
    fields: Option<&FieldNumbers>,
) -> Result<Option<Vec<u8>>, CommandError> {
    if collation.is_some() || !parsed.reads_document_alone() {
        return Ok(None);
    }
    let mut bytes = super::query::raw_bytes(filter)?;
    if let Some(numbers) = fields.map(|f| parsed.fields_read(f)).filter(|n| !n.is_empty()) {
        bytes.extend(numbers.to_bytes());
    }
    Ok(Some(bytes))
}

/// The filter's BSON of sent bytes `bytes` and the numbers that follow it.
fn split_sent(bytes: &[u8]) -> Option<(&[u8], Option<FieldNumbers>)> {
    let n = usize::try_from(i32::from_le_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let (bson, rest) = (bytes.get(..n)?, &bytes[n..]);
    let numbers = if rest.is_empty() { None } else { Some(FieldNumbers::from_bytes(rest)?) };
    Some((bson, numbers))
}

/// What the leader kept of the rows of a part of its range: the documents the filter accepted,
/// in key order, and for each of them how many documents it had examined when it took it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Kept {
    /// The rows the filter accepted.
    pub rows: Rows,
    /// For each of them, the documents examined up to it, itself included.
    pub seen: Vec<usize>,
}

/// A filter that came from another node, compiled on the leader.
pub struct SentFilter {
    filter: Filter,
    ext: CollatedExtensions,
}

impl SentFilter {
    /// The filter of `bytes` for the documents of the collection of `key`; `None` when it does
    /// not parse or reads more than the document.
    pub fn parse(bytes: &[u8], key: &[u8]) -> Option<SentFilter> {
        let (bytes, numbers) = split_sent(bytes)?;
        let raw = RawDoc::from_bytes_with(bytes, command_limits()).ok()?;
        let filter = Filter::parse(raw, FilterUse::Query).ok()?;
        if !filter.reads_document_alone() {
            return None;
        }
        let filter =
            super::query::by_statistics_of_key(filter, key).with_fields(numbers.map(Arc::new));
        Some(SentFilter { filter, ext: CollatedExtensions::new(None) })
    }

    /// The rows of `rows` the filter accepts; `None` when a document does not read or the filter
    /// fails on one, and the node that asked reads them itself, which gives the error in the
    /// order of the documents.
    pub fn keep(&self, rows: Rows) -> Option<Kept> {
        let ctx = MatchContext { collation: None, ext: &self.ext, vars: None };
        let mut out = Kept::default();
        let mut seen = 0;
        let mut rows = rows.into_iter();
        loop {
            let batch: Rows = rows.by_ref().take(BATCH).collect();
            if batch.is_empty() {
                return Some(out);
            }
            let docs: Vec<teseladb_keys::stored::Stored<'_>> = batch
                .iter()
                .map(|(_, d)| teseladb_keys::stored::split(d))
                .collect::<Result<_, _>>()
                .ok()?;
            let kept = self.filter.matches_batch(&docs, &ctx).ok()?;
            for (l, row) in batch.into_iter().enumerate() {
                seen += 1;
                if (kept >> l) & 1 == 1 {
                    out.seen.push(seen);
                    out.rows.push(row);
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_bson::Value;

    fn doc(i: i32) -> (Vec<u8>, Vec<u8>) {
        let d = Document::new().with("_id", i).with("v", i % 3);
        (vec![u8::try_from(i).unwrap()], d.to_bytes().unwrap())
    }

    #[test]
    fn the_leader_keeps_what_the_filter_accepts_with_what_it_examined() {
        let f = Document::new().with("v", 1);
        let sent = SentFilter::parse(&f.to_bytes().unwrap(), &[]).expect("a filter");
        let rows: Rows = (0..200).map(doc).collect();
        let kept = sent.keep(rows.clone()).expect("every document reads");
        let want: Vec<_> = rows.iter().filter(|(k, _)| k[0] % 3 == 1).cloned().collect();
        assert_eq!(kept.rows, want);
        let seen: Vec<usize> = (0..200).filter(|i| i % 3 == 1).map(|i| i + 1).collect();
        assert_eq!(kept.seen, seen);
    }

    #[test]
    fn the_numbers_of_the_filter_go_with_it_and_keep_the_same_documents() {
        use teseladb_keys::stored::{MIN_TABLE_FIELDS, encode_numbered};
        let dict = FieldNumbers::from_names(9, [&b"_id"[..], b"f1", b"f5"]);
        // Half the documents are written before `f5` got its number, and the first without any.
        let rows: Rows = (0..150)
            .map(|i: i32| {
                let mut d = Document::new().with("_id", i);
                for f in 0..MIN_TABLE_FIELDS {
                    d.push(format!("f{f}"), (i + f as i32) % 4);
                }
                let numbers = match i {
                    0 => FieldNumbers::default(),
                    1..75 => dict.subset([&b"f1"[..]]),
                    _ => dict.clone(),
                };
                let stored = encode_numbered(d.to_bytes().unwrap(), &numbers, None);
                (vec![u8::try_from(i).unwrap()], stored)
            })
            .collect();
        let f = Document::new().with("f1", 2).with("f5", Document::new().with("$gte", 1));
        let parsed =
            Filter::parse(RawDoc::from_bytes(&f.to_bytes().unwrap()).unwrap(), FilterUse::Query)
                .unwrap();
        let plain = sendable(&f, &parsed, None, None).unwrap().unwrap();
        let numbered = sendable(&f, &parsed, None, Some(&dict)).unwrap().unwrap();
        assert!(numbered.len() > plain.len(), "the numbers follow the filter");
        let by_name = SentFilter::parse(&plain, &[]).unwrap().keep(rows.clone()).unwrap();
        let by_number = SentFilter::parse(&numbered, &[]).unwrap().keep(rows).unwrap();
        assert!(!by_name.rows.is_empty());
        assert_eq!(by_number, by_name);
        // Numbers that do not read decline.
        assert!(SentFilter::parse(&[&plain[..], &[1, 2]].concat(), &[]).is_none());
    }

    #[test]
    fn a_document_that_does_not_read_or_a_filter_that_reads_more_declines() {
        let f = Document::new().with("v", 1).to_bytes().unwrap();
        let sent = SentFilter::parse(&f, &[]).expect("a filter");
        assert!(sent.keep(vec![(vec![1], vec![9, 0, 0])]).is_none());
        let expr = Document::new()
            .with("$expr", Document::new().with("$eq", Value::Array(vec!["$v".into(), 1.into()])));
        assert!(SentFilter::parse(&expr.to_bytes().unwrap(), &[]).is_none());
        assert!(SentFilter::parse(b"junk", &[]).is_none());
    }

    #[test]
    fn only_a_filter_of_the_document_alone_without_collation_is_sent() {
        let f = Document::new().with("v", 1);
        let parsed =
            Filter::parse(RawDoc::from_bytes(&f.to_bytes().unwrap()).unwrap(), FilterUse::Query)
                .unwrap();
        assert!(sendable(&f, &parsed, None, None).unwrap().is_some());
        let simple = Document::new().with("locale", "simple");
        assert!(sendable(&f, &parsed, Some(&simple), None).unwrap().is_none());
        let expr = Document::new()
            .with("$expr", Document::new().with("$eq", Value::Array(vec!["$v".into(), 1.into()])));
        let parsed =
            Filter::parse(RawDoc::from_bytes(&expr.to_bytes().unwrap()).unwrap(), FilterUse::Query)
                .unwrap();
        assert!(sendable(&expr, &parsed, None, None).unwrap().is_none());
    }
}
