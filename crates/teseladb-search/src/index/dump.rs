//! A segment of a search index as an immutable file (011 FR-012): the documents of a
//! [`MemIndex`] written in the segment format (`segment-format.md`, [`crate::segment::file`]) with
//! the mark of the versions it applied, and read back without analyzing any text again.
//!
//! The deleted documents are left out and the live ones renumbered in their order, so a dump is
//! also the merge that drops deleted versions. Each text field keeps its postings (a term's
//! positions; a term twice at one position takes a second occurrence numbered by its offset), its
//! scoring lengths, and two reserved columns whose names hold a NUL, which no BSON field name can:
//! the number of tokens of each document with the field (stacked tokens included, the mean length
//! comes from them) and, in the first document, the field's query analyzer. The stored documents
//! go in `storedSource`'s table. The applied mark goes in the index name, `@physical.logical`.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc};
use teseladb_keys::Hlc;
use teseladb_types::GroupId;

use super::{MemIndex, TextField};
use crate::analysis::ANALYZERS_VERSION;
use crate::definition::Definition;
use crate::segment::Corrupt;
use crate::segment::data::{DocEntry, FieldData, Occurrence, PostingEntry, SegmentData};
use crate::segment::file::{self, Segment};

/// Prefix of the column with each document's number of tokens in a field.
const TOKENS: &str = "\0tokens\0";
/// Prefix of the column with a field's query analyzer.
const SEARCH: &str = "\0search\0";

/// Why a segment file cannot be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DumpError {
    /// The file is damaged or not a segment of a search index.
    Corrupt(&'static str),
    /// Other analyzers made its terms (FR-011): the segment is built again from the documents.
    Analyzers,
    /// The ranges of the files are not in key order, one after the other.
    Ranges,
}

impl From<Corrupt> for DumpError {
    fn from(c: Corrupt) -> DumpError {
        DumpError::Corrupt(c.0)
    }
}

/// The index name that carries `mark`.
fn mark_name(mark: Hlc) -> String {
    format!("@{}.{}", mark.physical, mark.logical)
}

/// The mark an index name written by [`mark_name`] carries.
fn parse_mark(name: &str) -> Option<Hlc> {
    let (physical, logical) = name.strip_prefix('@')?.split_once('.')?;
    Some(Hlc { physical: physical.parse().ok()?, logical: logical.parse().ok()? })
}

/// The mark the segment file `bytes` applied the versions up to.
pub fn applied_of(bytes: &[u8]) -> Result<Hlc, DumpError> {
    let seg = Segment::open(bytes)?;
    parse_mark(&seg.index).ok_or(DumpError::Corrupt("segment: applied mark"))
}

/// The occurrences of a term in one document from its positions (sorted, maybe repeated).
fn occurrences(positions: &[u32]) -> Vec<Occurrence> {
    let mut sorted = positions.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<Occurrence> = Vec::with_capacity(sorted.len());
    for p in sorted {
        let start = match out.last() {
            Some(o) if o.position == p => o.start + 1,
            _ => 0,
        };
        out.push(Occurrence { position: p, start, end: 0 });
    }
    out
}

impl MemIndex {
    /// This index as a segment file covering the versions up to `mark`.
    pub fn dump(&self, mark: Hlc) -> Vec<u8> {
        let live: Vec<u32> = self.live().collect();
        let number: BTreeMap<u32, u32> =
            live.iter().enumerate().map(|(n, &id)| (id, n as u32)).collect();
        let mut data = SegmentData::new(GroupId(0), &mark_name(mark), ANALYZERS_VERSION);
        for &id in &live {
            let key = self.keys[id as usize].clone().unwrap_or_default();
            data.docs.push(DocEntry { key, ts: mark });
            let doc = self.docs[id as usize].as_ref().and_then(|d| d.to_bytes().ok());
            data.stored.push(doc);
        }
        for (path, f) in &self.fields {
            if f.lengths.is_empty() {
                continue;
            }
            let mut field = FieldData { lengths: vec![0; live.len()], ..FieldData::default() };
            let mut tokens = vec![None; live.len()];
            for (id, &len) in &f.lengths {
                let n = number[id] as usize;
                field.lengths[n] = len;
                let t = f.tokens.get(id).copied().unwrap_or(0);
                tokens[n] = Some(t.to_le_bytes().to_vec());
            }
            for (term, docs) in &f.postings {
                let list = docs
                    .iter()
                    .map(|(id, ps)| PostingEntry { doc: number[id], occurrences: occurrences(ps) })
                    .collect();
                field.terms.insert(term.as_bytes().to_vec(), list);
            }
            let mut search = vec![None; live.len()];
            search[0] = Some(f.search_analyzer.as_bytes().to_vec());
            data.fields.insert(path.clone(), field);
            data.columns.insert(format!("{TOKENS}{path}"), tokens);
            data.columns.insert(format!("{SEARCH}{path}"), search);
        }
        file::build(&data)
    }

    /// The index of `definition` a segment file written by [`MemIndex::dump`] holds.
    pub fn load(definition: Definition, bytes: &[u8]) -> Result<MemIndex, DumpError> {
        let seg = Segment::open(bytes)?;
        if seg.analyzer_version != ANALYZERS_VERSION {
            return Err(DumpError::Analyzers);
        }
        let data = seg.to_data()?;
        parse_mark(&data.index).ok_or(DumpError::Corrupt("segment: applied mark"))?;
        let mut index = MemIndex::new(definition);
        for (id, (entry, stored)) in data.docs.iter().zip(&data.stored).enumerate() {
            let bytes = stored.as_deref().ok_or(DumpError::Corrupt("segment: stored document"))?;
            let raw = RawDoc::from_bytes(bytes)
                .map_err(|_| DumpError::Corrupt("segment: stored document"))?;
            index.keys.push(Some(entry.key.clone()));
            index.docs.push(Some(Document::from(raw)));
            index.by_key.insert(entry.key.clone(), id as u32);
        }
        for (path, field) in &data.fields {
            let column = |prefix: &str| {
                data.columns
                    .get(&format!("{prefix}{path}"))
                    .ok_or(DumpError::Corrupt("segment: field columns"))
            };
            let (tokens, search) = (column(TOKENS)?, column(SEARCH)?);
            let analyzer = search.first().cloned().flatten().unwrap_or_default();
            let mut f = TextField {
                search_analyzer: String::from_utf8(analyzer)
                    .map_err(|_| DumpError::Corrupt("segment: query analyzer"))?,
                ..TextField::default()
            };
            for (id, t) in tokens.iter().enumerate() {
                let Some(t) = t else { continue };
                let t: [u8; 4] =
                    t.as_slice().try_into().map_err(|_| DumpError::Corrupt("segment: tokens"))?;
                f.lengths.insert(id as u32, field.lengths[id]);
                f.tokens.insert(id as u32, u32::from_le_bytes(t));
            }
            for (term, list) in &field.terms {
                let term = String::from_utf8(term.clone())
                    .map_err(|_| DumpError::Corrupt("segment: term"))?;
                let docs = list
                    .iter()
                    .map(|p| (p.doc, p.occurrences.iter().map(|o| o.position).collect()))
                    .collect();
                f.postings.insert(term, docs);
            }
            for (id, positions) in f.postings.values().flat_map(|d| d.iter()) {
                let end = positions.iter().max().map_or(0, |p| p + 1);
                let e = f.ends.entry(*id).or_insert(0);
                *e = (*e).max(end);
            }
            index.fields.insert(path.clone(), f);
        }
        Ok(index)
    }
}
