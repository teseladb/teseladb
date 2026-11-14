//! The segment file, version 1 (`segment-format.md`).
//!
//! - Header: the magic `TSLSEG01`, the version, the range, the analyzer version and the index
//!   name, padded to 4 KiB.
//! - Sections, each at a 4 KiB boundary: 32 bytes for the nonce and tag of 013's encryption (zero
//!   without encryption), the payload and its CRC32C.
//! - Section table: per section its kind, name, offset and payload length.
//! - Footer of [`FOOTER`] bytes: applied index covered, lowest and highest timestamp, 32 bytes
//!   reserved for 013, the header length, the table's offset and length, and the CRC32C of the
//!   header, the table and the footer.
//!
//! With encryption at rest (013 FR-015) [`build_sealed`] seals each section's payload with the
//! segment's data key, bound to the section's kind and offset: the 32 bytes hold the IV or nonce
//! and the tag, the table gives the ciphertext's length and the CRC32C covers the ciphertext. The
//! data key, wrapped with the master key (64 bytes), goes right after the header, and the footer's
//! 32 reserved bytes say so: `1 · offset(u64 LE) · zeros`. [`open_sealed`] gives back the bytes of
//! the same segment in clear, which [`Segment::open`] reads.
//!
//! A field section holds the term dictionary ([`Fst`], term → offset of its postings), the norms
//! (exact length as four bytes and the norm byte per document) and the postings. The docmap holds
//! per document the indexed version's timestamp and its key; columns, geographic cells and
//! `storedSource` are per-document tables. A vector field's section is its HNSW graph with the
//! vectors ([`Hnsw::encode`]), built when the segment is written.

use std::collections::BTreeMap;

use teseladb_keys::Hlc;
use teseladb_crypto::at_rest::{AtRestError, FileKey, KeyRing, WRAPPED_KEY_LEN};
use teseladb_hash::crc32c;
use teseladb_types::GroupId;

use crate::query::bm25::TermScorer;
use crate::segment::data::{DocEntry, FieldData, SegmentData, VectorData};
use crate::segment::fst::{Builder, Fst};
use crate::segment::hnsw::{DEFAULT_M, Hnsw};
use crate::segment::postings::{self, DiskCursor};
use crate::segment::table::{self, Table};
use crate::segment::{Corrupt, norms, varint};

/// Magic bytes at the start of a segment.
pub const MAGIC: &[u8; 8] = b"TSLSEG01";
/// Format version.
pub const VERSION: u32 = 1;
/// Section alignment.
pub const ALIGN: usize = 4096;
/// Bytes reserved before each section's payload for the nonce and tag of the encryption.
pub const SEALED: usize = 32;
/// Size of the footer.
pub const FOOTER: usize = 8 + 12 + 12 + 32 + 8 + 8 + 8 + 4;

/// Kinds of section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Kind {
    Docmap = 1,
    Field = 2,
    Column = 3,
    Geo = 4,
    Stored = 5,
    Hnsw = 6,
}

impl Kind {
    fn from_byte(b: u8) -> Option<Kind> {
        [Kind::Docmap, Kind::Field, Kind::Column, Kind::Geo, Kind::Stored, Kind::Hnsw]
            .into_iter()
            .find(|k| *k as u8 == b)
    }
}

/// A section: its kind, its name and its payload.
type Section = (u8, String, Vec<u8>);

/// Writes `data` as a segment file.
pub fn build(data: &SegmentData) -> Vec<u8> {
    let (header, sections, applied, min, max) = parts(data);
    assemble(header, &sections, applied, min, max, None)
}

/// Writes `data` as a segment file sealed with a new data key of `ring` (013 FR-015).
pub fn build_sealed(data: &SegmentData, ring: &KeyRing) -> Result<Vec<u8>, AtRestError> {
    let (header, sections, applied, min, max) = parts(data);
    let (key, wrapped) = ring.new_file_key()?;
    Ok(assemble(header, &sections, applied, min, max, Some((&key, wrapped))))
}

/// What a sealed section is bound to: its kind and its offset.
fn aad(kind: u8, offset: usize) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[0] = kind;
    a[1..].copy_from_slice(&(offset as u64).to_le_bytes());
    a
}

/// The bytes of segment `bytes` in clear: itself if it is not sealed, its sections opened with
/// the data key `ring` unwraps otherwise.
pub fn open_sealed(bytes: &[u8], ring: Option<&KeyRing>) -> Result<Vec<u8>, Corrupt> {
    let start = bytes.len().checked_sub(FOOTER).ok_or(Corrupt("segment: short"))?;
    let mut f = Cur { b: &bytes[start..], pos: 0 };
    let applied = f.u64()?;
    let mut ts = [Hlc { physical: 0, logical: 0 }; 2];
    for t in &mut ts {
        *t = Hlc { physical: f.u64()?, logical: f.u32()? };
    }
    let reserved = f.take(32)?;
    if reserved[0] == 0 {
        return Ok(bytes.to_vec());
    }
    let mut at = Cur { b: reserved, pos: 1 };
    let wrapped_at = at.usize()?;
    let (header_len, table_offset, table_len) = (f.usize()?, f.usize()?, f.usize()?);
    let crc = f.u32()?;
    let table_end = table_offset.checked_add(table_len).ok_or(Corrupt("segment: table"))?;
    if reserved[0] != 1 || reserved[9..].iter().any(|&b| b != 0) || table_end != start {
        return Err(Corrupt("segment: sealed footer"));
    }
    let header = bytes.get(..header_len).ok_or(Corrupt("segment: table"))?;
    let table_bytes = bytes.get(table_offset..table_end).ok_or(Corrupt("segment: table"))?;
    if checksum(header, table_bytes, &bytes[start..bytes.len() - 4]) != crc {
        return Err(Corrupt("segment: checksum"));
    }
    let ring = ring.ok_or(Corrupt("segment: sealed without a key ring"))?;
    let wrapped = bytes
        .get(wrapped_at..wrapped_at + WRAPPED_KEY_LEN)
        .filter(|_| wrapped_at == header_len)
        .ok_or(Corrupt("segment: data key"))?;
    let key = ring.open_file_key(wrapped).map_err(|_| Corrupt("segment: data key"))?;
    let mut t = Cur { b: table_bytes, pos: 0 };
    let count = varint::get(table_bytes, &mut t.pos).ok_or(Corrupt("segment: table"))?;
    let mut sections = Vec::new();
    for _ in 0..count {
        let kind = t.take(1)?[0];
        let name = t.name()?;
        let (offset, len) = (t.usize()?, t.usize()?);
        let end = offset
            .checked_add(SEALED + len + 4)
            .filter(|&e| e <= table_offset && offset > header_len && offset % ALIGN == 0)
            .ok_or(Corrupt("segment: section bounds"))?;
        let ct = &bytes[offset + SEALED..end - 4];
        if crc32c(ct).to_le_bytes() != bytes[end - 4..end] {
            return Err(Corrupt("segment: section checksum"));
        }
        let trailer: &[u8; SEALED] =
            bytes[offset..offset + SEALED].try_into().map_err(|_| Corrupt("segment: sealed"))?;
        let payload = key
            .open(&aad(kind, offset), ct, trailer)
            .map_err(|_| Corrupt("segment: sealed section"))?;
        sections.push((kind, name, payload));
    }
    if t.pos != table_bytes.len() {
        return Err(Corrupt("segment: table"));
    }
    Ok(assemble(header.to_vec(), &sections, applied, ts[0], ts[1], None))
}

/// The header and the sections of `data`, with the footer's values.
fn parts(data: &SegmentData) -> (Vec<u8>, Vec<Section>, u64, Hlc, Hlc) {
    let n = data.docs.len();
    assert_eq!(data.stored.len(), n, "one stored entry per document");
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&data.range.0.to_le_bytes());
    out.extend_from_slice(&data.analyzer_version.to_le_bytes());
    varint::put(&mut out, data.index.len() as u64);
    out.extend_from_slice(data.index.as_bytes());
    let mut sections: Vec<(u8, String, Vec<u8>)> = Vec::new();
    let docmap: Vec<Vec<u8>> = data.docs.iter().map(encode_doc).collect();
    let docmap = tabled(docmap.iter().map(|d| Some(d.as_slice())));
    sections.push((Kind::Docmap as u8, String::new(), docmap));
    for (name, field) in &data.fields {
        sections.push((Kind::Field as u8, name.clone(), encode_field(field, n)));
    }
    for (name, col) in &data.columns {
        assert_eq!(col.len(), n, "one column entry per document");
        sections.push((Kind::Column as u8, name.clone(), tabled(col.iter().map(|c| c.as_deref()))));
    }
    for (name, cells) in &data.geo {
        assert_eq!(cells.len(), n, "one geo entry per document");
        let enc: Vec<Vec<u8>> =
            cells.iter().map(|c| c.iter().flat_map(|v| v.to_le_bytes()).collect()).collect();
        let geo = tabled(enc.iter().map(|c| Some(c.as_slice())));
        sections.push((Kind::Geo as u8, name.clone(), geo));
    }
    for (name, v) in &data.vectors {
        assert_eq!(v.values.len(), n, "one vector entry per document");
        let graph = Hnsw::build(v.dims, v.similarity, v.quantization, DEFAULT_M, v.values.clone());
        sections.push((Kind::Hnsw as u8, name.clone(), graph.encode()));
    }
    let stored = tabled(data.stored.iter().map(|s| s.as_deref()));
    sections.push((Kind::Stored as u8, String::new(), stored));
    let zero = Hlc { physical: 0, logical: 0 };
    let (min, max) = data.ts_bounds().unwrap_or((zero, zero));
    (out, sections, data.applied_index, min, max)
}

/// Lays out `sections` after `header` and adds the table and the footer; with `seal`, the
/// wrapped data key goes after the header and each payload is sealed with the key.
fn assemble(
    mut out: Vec<u8>,
    sections: &[Section],
    applied_index: u64,
    min: Hlc,
    max: Hlc,
    seal: Option<(&FileKey, [u8; WRAPPED_KEY_LEN])>,
) -> Vec<u8> {
    let header_len = out.len();
    let mut reserved = [0u8; 32];
    if let Some((_, wrapped)) = &seal {
        reserved[0] = 1;
        reserved[1..9].copy_from_slice(&(header_len as u64).to_le_bytes());
        out.extend_from_slice(wrapped);
    }
    let mut entries = Vec::new();
    for (kind, name, payload) in sections {
        out.resize(out.len().next_multiple_of(ALIGN), 0);
        let offset = out.len();
        let (trailer, body) = match &seal {
            None => ([0; SEALED], payload.clone()),
            // A segment has a handful of sections; a data key seals 2³² blocks.
            Some((key, _)) => {
                let (ct, trailer) = key
                    .seal(&aad(*kind, offset), payload)
                    .expect("a new data key seals a segment's sections");
                (trailer, ct)
            }
        };
        out.extend_from_slice(&trailer);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32c(&body).to_le_bytes());
        entries.push((*kind, name, offset as u64, body.len() as u64));
    }
    let table_offset = out.len();
    varint::put(&mut out, entries.len() as u64);
    for (kind, name, offset, len) in entries {
        out.push(kind);
        varint::put(&mut out, name.len() as u64);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    let table_len = out.len() - table_offset;
    let mut footer = Vec::with_capacity(FOOTER);
    footer.extend_from_slice(&applied_index.to_le_bytes());
    for ts in [min, max] {
        footer.extend_from_slice(&ts.physical.to_le_bytes());
        footer.extend_from_slice(&ts.logical.to_le_bytes());
    }
    footer.extend_from_slice(&reserved);
    footer.extend_from_slice(&(header_len as u64).to_le_bytes());
    footer.extend_from_slice(&(table_offset as u64).to_le_bytes());
    footer.extend_from_slice(&(table_len as u64).to_le_bytes());
    let crc = checksum(&out[..header_len], &out[table_offset..], &footer);
    footer.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&footer);
    out
}

fn checksum(header: &[u8], table: &[u8], footer: &[u8]) -> u32 {
    let crc = teseladb_hash::crc32c_append(crc32c(header), table);
    teseladb_hash::crc32c_append(crc, footer)
}

fn tabled<'a>(entries: impl ExactSizeIterator<Item = Option<&'a [u8]>>) -> Vec<u8> {
    let mut out = Vec::new();
    table::write(&mut out, entries);
    out
}

fn encode_doc(d: &DocEntry) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + d.key.len());
    v.extend_from_slice(&d.ts.physical.to_le_bytes());
    v.extend_from_slice(&d.ts.logical.to_le_bytes());
    v.extend_from_slice(&d.key);
    v
}

/// The terms of a field the table in front of its dictionary holds (FR-029), when it has more.
pub const FREQUENT_TERMS: usize = 256;

/// The [`FREQUENT_TERMS`] terms of `field` in the most documents, with the offsets of their
/// postings (`offsets`, in term order), ties in term order; none when the field has no more terms
/// than that. A segment is written before the queries that will read it, so the number of
/// documents of a term stands for how often queries reach it: a query term found in many
/// documents is the one whose lookups and long postings a query pays for most.
fn frequent_terms<'a>(field: &'a FieldData, offsets: &[u64]) -> Vec<(&'a [u8], u64)> {
    if field.terms.len() <= FREQUENT_TERMS {
        return Vec::new();
    }
    let mut ranked: Vec<(usize, &'a [u8], u64)> = field
        .terms
        .iter()
        .zip(offsets)
        .map(|((term, list), &at)| (list.len(), term.as_slice(), at))
        .collect();
    ranked.sort_by_key(|&(docs, _, _)| std::cmp::Reverse(docs));
    ranked.into_iter().take(FREQUENT_TERMS).map(|(_, term, at)| (term, at)).collect()
}

fn encode_field(field: &FieldData, docs: usize) -> Vec<u8> {
    assert_eq!(field.lengths.len(), docs, "one length per document");
    let norm_of: Vec<u8> = field.lengths.iter().map(|&l| norms::encode(l)).collect();
    let mut area = Vec::new();
    let mut dict = Builder::new();
    let mut offsets = Vec::with_capacity(field.terms.len());
    for (term, list) in &field.terms {
        assert!(
            list.windows(2).all(|w| w[0].doc < w[1].doc)
                && list.iter().all(|p| (p.doc as usize) < docs && !p.occurrences.is_empty())
                && list.iter().all(|p| p.occurrences.windows(2).all(|w| w[0] < w[1])),
            "postings by document, inside the segment, with sorted occurrences"
        );
        dict.insert(term, area.len() as u64);
        offsets.push(area.len() as u64);
        postings::write(&mut area, list, &norm_of);
    }
    let dict = dict.finish_with(&frequent_terms(field, &offsets));
    let mut out = Vec::new();
    out.extend_from_slice(&(dict.len() as u64).to_le_bytes());
    out.extend_from_slice(&dict);
    for &l in &field.lengths {
        out.extend_from_slice(&l.to_le_bytes());
    }
    out.extend_from_slice(&norm_of);
    out.extend_from_slice(&area);
    out
}

/// A text field of an open segment.
#[derive(Clone, Copy, Debug)]
pub struct FieldReader<'a> {
    dict: Fst<'a>,
    lengths: &'a [u8],
    norms: &'a [u8],
    area: &'a [u8],
}

impl<'a> FieldReader<'a> {
    /// Number of distinct terms.
    pub fn terms(&self) -> u64 {
        self.dict.len()
    }

    /// The term dictionary, to run automata over it.
    pub fn dictionary(&self) -> Fst<'a> {
        self.dict
    }

    /// The exact length of the field in document `doc`.
    pub fn length(&self, doc: u32) -> u32 {
        let at = doc as usize * 4;
        u32::from_le_bytes(self.lengths[at..at + 4].try_into().expect("4 bytes"))
    }

    /// The norm byte of document `doc`.
    pub fn norm(&self, doc: u32) -> u8 {
        self.norms[doc as usize]
    }

    /// The postings of `term`, or `None` without them.
    pub fn postings(&self, term: &[u8]) -> Option<&'a [u8]> {
        let at = usize::try_from(self.dict.get(term)?).ok()?;
        self.area.get(at..)
    }

    /// A scoring cursor over `term`.
    pub fn cursor(
        &self,
        term: &[u8],
        scorer: TermScorer,
    ) -> Option<Result<DiskCursor<'a>, Corrupt>> {
        Some(DiskCursor::new(self.postings(term)?, self.norms, scorer))
    }

    fn to_data(self) -> Result<FieldData, Corrupt> {
        let docs = self.norms.len() as u32;
        let mut terms = BTreeMap::new();
        for (term, at) in self.dict.entries() {
            let bytes = usize::try_from(at)
                .ok()
                .and_then(|a| self.area.get(a..))
                .ok_or(Corrupt("postings offset"))?;
            terms.insert(term, postings::read_all(bytes)?);
        }
        Ok(FieldData { lengths: (0..docs).map(|d| self.length(d)).collect(), terms })
    }
}

/// An open segment.
#[derive(Clone, Debug)]
pub struct Segment<'a> {
    /// Range.
    pub range: GroupId,
    /// Index name.
    pub index: String,
    /// Analyzer version.
    pub analyzer_version: u32,
    /// Applied index covered.
    pub applied_index: u64,
    /// Lowest and highest timestamp of its documents (zero without documents).
    pub ts_bounds: (Hlc, Hlc),
    docmap: Table<'a>,
    stored: Table<'a>,
    fields: BTreeMap<String, FieldReader<'a>>,
    columns: BTreeMap<String, Table<'a>>,
    geo: BTreeMap<String, Table<'a>>,
    hnsw: BTreeMap<String, &'a [u8]>,
}

struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Corrupt> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or(Corrupt("segment: short"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, Corrupt> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> Result<u64, Corrupt> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn usize(&mut self) -> Result<usize, Corrupt> {
        usize::try_from(self.u64()?).map_err(|_| Corrupt("segment: size"))
    }

    fn name(&mut self) -> Result<String, Corrupt> {
        let n = varint::get(self.b, &mut self.pos).ok_or(Corrupt("segment: name"))?;
        let n = usize::try_from(n).map_err(|_| Corrupt("segment: name"))?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| Corrupt("segment: name"))
    }
}

impl<'a> Segment<'a> {
    /// Opens a segment, checking its checksums and the structure of every section.
    pub fn open(bytes: &'a [u8]) -> Result<Segment<'a>, Corrupt> {
        let start = bytes.len().checked_sub(FOOTER).ok_or(Corrupt("segment: short"))?;
        let mut f = Cur { b: &bytes[start..], pos: 0 };
        let applied_index = f.u64()?;
        let mut ts = [Hlc { physical: 0, logical: 0 }; 2];
        for t in &mut ts {
            *t = Hlc { physical: f.u64()?, logical: f.u32()? };
        }
        f.take(32)?;
        let (header_len, table_offset, table_len) = (f.usize()?, f.usize()?, f.usize()?);
        let crc = f.u32()?;
        let table_end = table_offset.checked_add(table_len).ok_or(Corrupt("segment: table"))?;
        if header_len > table_offset || table_end != start {
            return Err(Corrupt("segment: table"));
        }
        let table_bytes = &bytes[table_offset..table_end];
        if checksum(&bytes[..header_len], table_bytes, &bytes[start..bytes.len() - 4]) != crc {
            return Err(Corrupt("segment: checksum"));
        }
        let mut h = Cur { b: &bytes[..header_len], pos: 0 };
        if h.take(8)? != MAGIC || h.u32()? != VERSION {
            return Err(Corrupt("segment: magic or version"));
        }
        let range = GroupId(h.u64()?);
        let analyzer_version = h.u32()?;
        let index = h.name()?;
        let mut seg = Segment {
            range,
            index,
            analyzer_version,
            applied_index,
            ts_bounds: (ts[0], ts[1]),
            docmap: Table::open(EMPTY_TABLE).expect("static"),
            stored: Table::open(EMPTY_TABLE).expect("static"),
            fields: BTreeMap::new(),
            columns: BTreeMap::new(),
            geo: BTreeMap::new(),
            hnsw: BTreeMap::new(),
        };
        let mut t = Cur { b: table_bytes, pos: 0 };
        let count = varint::get(table_bytes, &mut t.pos).ok_or(Corrupt("segment: table"))?;
        let mut sections: Vec<(Kind, String, &'a [u8])> = Vec::new();
        for _ in 0..count {
            let kind = Kind::from_byte(t.take(1)?[0]).ok_or(Corrupt("segment: section kind"))?;
            let name = t.name()?;
            let (offset, len) = (t.usize()?, t.usize()?);
            let end = offset
                .checked_add(SEALED)
                .and_then(|o| o.checked_add(len))
                .and_then(|o| o.checked_add(4))
                .filter(|&e| e <= table_offset && offset >= header_len && offset % ALIGN == 0)
                .ok_or(Corrupt("segment: section bounds"))?;
            if bytes[offset..offset + SEALED].iter().any(|&b| b != 0) {
                return Err(Corrupt("segment: sealed section"));
            }
            let payload = &bytes[offset + SEALED..end - 4];
            if crc32c(payload).to_le_bytes() != bytes[end - 4..end] {
                return Err(Corrupt("segment: section checksum"));
            }
            sections.push((kind, name, payload));
        }
        if t.pos != table_bytes.len() {
            return Err(Corrupt("segment: table"));
        }
        let mut have = (false, false);
        for (kind, _, payload) in &sections {
            match kind {
                Kind::Docmap => {
                    seg.docmap = Table::open(payload)?;
                    have.0 = true;
                }
                Kind::Stored => {
                    seg.stored = Table::open(payload)?;
                    have.1 = true;
                }
                _ => {}
            }
        }
        if have != (true, true) {
            return Err(Corrupt("segment: docmap or stored missing"));
        }
        let n = seg.docmap.len();
        for i in 0..n {
            if seg.docmap.get(i).is_none_or(|d| d.len() < 12) {
                return Err(Corrupt("segment: docmap entry"));
            }
        }
        if seg.stored.len() != n {
            return Err(Corrupt("segment: stored count"));
        }
        for (kind, name, payload) in sections {
            match kind {
                Kind::Field => {
                    seg.fields.insert(name, open_field(payload, n)?);
                }
                Kind::Column | Kind::Geo => {
                    let t = Table::open(payload)?;
                    if t.len() != n {
                        return Err(Corrupt("segment: entry count"));
                    }
                    if kind == Kind::Geo
                        && (0..n).any(|i| t.get(i).is_none_or(|c| c.len() % 8 != 0))
                    {
                        return Err(Corrupt("segment: geo entry"));
                    }
                    let map = if kind == Kind::Column { &mut seg.columns } else { &mut seg.geo };
                    map.insert(name, t);
                }
                Kind::Hnsw => {
                    if Hnsw::decode(payload)?.vectors().len() != n {
                        return Err(Corrupt("segment: vector count"));
                    }
                    seg.hnsw.insert(name, payload);
                }
                Kind::Docmap | Kind::Stored => {}
            }
        }
        Ok(seg)
    }

    /// Number of documents.
    pub fn docs(&self) -> usize {
        self.docmap.len()
    }

    /// Document `doc`: its key and the timestamp of the indexed version.
    pub fn doc(&self, doc: u32) -> DocEntry {
        let d = self.docmap.get(doc as usize).expect("checked at open");
        let physical = u64::from_le_bytes(d[..8].try_into().expect("8 bytes"));
        let logical = u32::from_le_bytes(d[8..12].try_into().expect("4 bytes"));
        DocEntry { key: d[12..].to_vec(), ts: Hlc { physical, logical } }
    }

    /// The text field at `path`.
    pub fn field(&self, path: &str) -> Option<FieldReader<'a>> {
        self.fields.get(path).copied()
    }

    /// The column value of `doc` at `path`.
    pub fn column(&self, path: &str, doc: u32) -> Option<&'a [u8]> {
        self.columns.get(path)?.get(doc as usize)
    }

    /// The geographic cells of `doc` at `path`.
    pub fn geo(&self, path: &str, doc: u32) -> Option<Vec<u64>> {
        let b = self.geo.get(path)?.get(doc as usize)?;
        Some(b.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect())
    }

    /// The HNSW graph of the vector field at `path`.
    pub fn vectors(&self, path: &str) -> Option<Hnsw> {
        self.hnsw.get(path).and_then(|p| Hnsw::decode(p).ok())
    }

    /// The `storedSource` of `doc`.
    pub fn stored(&self, doc: u32) -> Option<&'a [u8]> {
        self.stored.get(doc as usize)
    }

    /// The whole content, decoded (for merges).
    pub fn to_data(&self) -> Result<SegmentData, Corrupt> {
        let n = self.docs() as u32;
        let mut data = SegmentData::new(self.range, &self.index, self.analyzer_version);
        data.applied_index = self.applied_index;
        data.docs = (0..n).map(|d| self.doc(d)).collect();
        data.stored = (0..n).map(|d| self.stored(d).map(<[u8]>::to_vec)).collect();
        for (name, f) in &self.fields {
            data.fields.insert(name.clone(), f.to_data()?);
        }
        for name in self.columns.keys() {
            data.columns.insert(
                name.clone(),
                (0..n).map(|d| self.column(name, d).map(<[u8]>::to_vec)).collect(),
            );
        }
        for name in self.geo.keys() {
            data.geo.insert(
                name.clone(),
                (0..n).map(|d| self.geo(name, d).unwrap_or_default()).collect(),
            );
        }
        for (name, payload) in &self.hnsw {
            let g = Hnsw::decode(payload)?;
            data.vectors.insert(
                name.clone(),
                VectorData {
                    dims: g.dims(),
                    similarity: g.similarity(),
                    quantization: g.quantization(),
                    values: g.vectors().to_vec(),
                },
            );
        }
        Ok(data)
    }
}

/// A table with no entry.
const EMPTY_TABLE: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

fn open_field(payload: &[u8], docs: usize) -> Result<FieldReader<'_>, Corrupt> {
    let mut c = Cur { b: payload, pos: 0 };
    let dict_len = c.usize()?;
    let dict = Fst::new(c.take(dict_len)?)?;
    let lengths = c.take(docs.checked_mul(4).ok_or(Corrupt("segment: norms"))?)?;
    let norms = c.take(docs)?;
    Ok(FieldReader { dict, lengths, norms, area: &payload[c.pos..] })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::query::bm25::{Bm25, FieldStats};
    use crate::query::wand::Cursor;
    use crate::segment::data::{Occurrence, PostingEntry};

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    pub(crate) fn sample(seed: u64, docs: u32) -> SegmentData {
        let mut rng = Rng(seed | 1);
        let mut data = SegmentData::new(GroupId(seed), "default", 3);
        data.applied_index = 1000 + seed;
        for d in 0..docs {
            let ts = Hlc { physical: 100 + rng.below(50), logical: rng.below(3) as u32 };
            data.docs.push(DocEntry { key: format!("k{d:05}").into_bytes(), ts });
            data.stored.push((d % 3 != 0).then(|| vec![d as u8; (d % 5) as usize]));
        }
        for name in ["body", "title"] {
            let mut f = FieldData { lengths: vec![0; docs as usize], ..FieldData::default() };
            for t in 0..20_u32 {
                let mut list = Vec::new();
                for d in 0..docs {
                    if rng.below(10) < 3 {
                        let occurrences = (0..1 + rng.below(3) as u32)
                            .map(|i| Occurrence { position: i * 2, start: i * 10, end: i * 10 + 4 })
                            .collect();
                        f.lengths[d as usize] += 3;
                        list.push(PostingEntry { doc: d, occurrences });
                    }
                }
                if !list.is_empty() {
                    f.terms.insert(format!("t{t}").into_bytes(), list);
                }
            }
            data.fields.insert(name.to_owned(), f);
        }
        data.columns.insert(
            "year".to_owned(),
            (0..docs).map(|d| (d % 4 != 0).then(|| (2000 + d).to_le_bytes().to_vec())).collect(),
        );
        data.geo.insert(
            "loc".to_owned(),
            (0..docs).map(|d| (0..d % 3).map(u64::from).collect()).collect(),
        );
        data
    }

    #[test]
    fn the_most_frequent_terms_go_in_front_of_the_dictionary() {
        let docs = 40_u32;
        let mut data = SegmentData::new(GroupId(9), "default", 3);
        for d in 0..docs {
            data.docs.push(DocEntry {
                key: format!("k{d:05}").into_bytes(),
                ts: Hlc { physical: 1, logical: 0 },
            });
            data.stored.push(None);
        }
        let mut f = FieldData { lengths: vec![1; docs as usize], ..FieldData::default() };
        let terms = FREQUENT_TERMS as u32 + 50;
        for t in 0..terms {
            // Term `t` is in `t % docs + 1` documents.
            let list = (0..t % docs + 1)
                .map(|d| PostingEntry {
                    doc: d,
                    occurrences: vec![Occurrence { position: 0, start: 0, end: 1 }],
                })
                .collect();
            f.terms.insert(format!("w{t:04}").into_bytes(), list);
        }
        data.fields.insert("body".to_owned(), f.clone());
        let bytes = build(&data);
        let seg = Segment::open(&bytes).expect("valid");
        let body = seg.field("body").expect("field");
        assert_eq!(body.dictionary().frequent_terms(), FREQUENT_TERMS);
        assert_eq!(seg.to_data(), Ok(data.clone()));
        let offsets: Vec<u64> = f.terms.keys().map(|t| body.dictionary().get(t).unwrap()).collect();
        let hot = frequent_terms(&f, &offsets);
        // The table holds the terms in the most documents, and no term in fewer than them.
        let least = hot.iter().map(|(t, _)| f.terms[*t].len()).min().unwrap();
        assert!(f.terms.iter().all(|(t, l)| l.len() <= least || hot.iter().any(|h| h.0 == t)));
        for (term, list) in &f.terms {
            let at = body.dictionary().get(term).expect("term");
            assert_eq!(
                body.postings(term).map(<[u8]>::len),
                body.area.len().checked_sub(at as usize)
            );
            assert_eq!(
                list.len(),
                (term[1..].iter().fold(0, |n, &b| n * 10 + u32::from(b - b'0')) % docs + 1)
                    as usize
            );
        }
        // A field with no more terms than the table has none.
        let small = sample(5, 10);
        let bytes = build(&small);
        let seg = Segment::open(&bytes).expect("valid");
        assert_eq!(seg.field("body").expect("field").dictionary().frequent_terms(), 0);
    }

    #[test]
    fn round_trips_every_part() {
        for (seed, docs) in [(1, 0), (2, 1), (3, 300), (4, 1000)] {
            let data = sample(seed, docs);
            let bytes = build(&data);
            let seg = Segment::open(&bytes).expect("valid");
            assert_eq!(seg.to_data(), Ok(data.clone()));
            assert_eq!(seg.docs(), docs as usize);
            assert_eq!(seg.range, GroupId(seed));
            assert_eq!(
                (seg.index.as_str(), seg.analyzer_version, seg.applied_index),
                ("default", 3, 1000 + seed)
            );
            let zero = Hlc { physical: 0, logical: 0 };
            assert_eq!(seg.ts_bounds, data.ts_bounds().unwrap_or((zero, zero)));
            assert!(seg.field("missing").is_none() && seg.column("missing", 0).is_none());
            assert!(seg.geo("missing", 0).is_none());
            assert_eq!(&bytes[..8], MAGIC);
        }
    }

    #[test]
    fn field_reader_serves_lengths_norms_and_cursors() {
        let data = sample(9, 500);
        let bytes = build(&data);
        let seg = Segment::open(&bytes).expect("valid");
        let body = seg.field("body").expect("field");
        let want = &data.fields["body"];
        assert_eq!(body.terms(), want.terms.len() as u64);
        assert_eq!(body.dictionary().len(), body.terms());
        for d in 0..500 {
            assert_eq!(body.length(d), want.lengths[d as usize]);
            assert_eq!(body.norm(d), norms::encode(want.lengths[d as usize]));
        }
        let s = Bm25::DEFAULT.term(1.0, FieldStats { docs: 500, total_len: 20_000 }, 10);
        let mut c = body.cursor(b"t3", s).expect("term").expect("valid");
        let docs: Vec<u32> = std::iter::from_fn(|| {
            let d = c.doc();
            c.advance(d.saturating_add(1));
            (d != u32::MAX).then_some(d)
        })
        .collect();
        assert_eq!(docs, want.terms[&b"t3".to_vec()].iter().map(|p| p.doc).collect::<Vec<_>>());
        assert!(body.cursor(b"nope", s).is_none());
    }

    #[test]
    fn any_damage_is_caught_or_harmless() {
        let data = sample(5, 200);
        let bytes = build(&data);
        let mut rng = Rng(77);
        assert!(Segment::open(&bytes[..FOOTER - 1]).is_err());
        for _ in 0..3000 {
            let mut bad = bytes.clone();
            let at = rng.below(bad.len() as u64) as usize;
            bad[at] ^= 1 << rng.below(8);
            // Only the padding between sections is outside every checksum.
            if let Ok(seg) = Segment::open(&bad) {
                assert_eq!(seg.to_data(), Ok(data.clone()));
            }
        }
    }

    /// Rewrites the footer's checksum after an edit, to reach the structural checks.
    fn reseal(bytes: &mut [u8]) {
        let start = bytes.len() - FOOTER;
        let word =
            |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) as usize;
        let header_len = word(bytes, start + 64);
        let (to, tl) = (word(bytes, start + 72), word(bytes, start + 80));
        let crc =
            checksum(&bytes[..header_len], &bytes[to..to + tl], &bytes[start..bytes.len() - 4]);
        let n = bytes.len();
        bytes[n - 4..].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn structural_damage_behind_a_valid_checksum_is_named() {
        let data = sample(6, 10);
        let bytes = build(&data);
        let start = bytes.len() - FOOTER;
        let mut magic = bytes.clone();
        magic[0] = b'X';
        reseal(&mut magic);
        assert_eq!(Segment::open(&magic).unwrap_err(), Corrupt("segment: magic or version"));
        let mut sealed = bytes.clone();
        sealed[ALIGN] = 1;
        reseal(&mut sealed);
        assert_eq!(Segment::open(&sealed).unwrap_err(), Corrupt("segment: sealed section"));
        let mut table = bytes.clone();
        table[start + 72] ^= 1;
        assert_eq!(Segment::open(&table).unwrap_err(), Corrupt("segment: table"));
        let mut order = bytes.clone();
        order[start + 64..start + 72].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Segment::open(&order).is_err());
        let mut kind = bytes.clone();
        let to = u64::from_le_bytes(bytes[start + 72..start + 80].try_into().unwrap()) as usize;
        kind[to + 1] = 99;
        reseal(&mut kind);
        assert_eq!(Segment::open(&kind).unwrap_err(), Corrupt("segment: section kind"));
        let mut payload = bytes.clone();
        payload[ALIGN + SEALED] ^= 1;
        assert_eq!(Segment::open(&payload).unwrap_err(), Corrupt("segment: section checksum"));
        // A segment without sections.
        let mut empty = Vec::new();
        empty.extend_from_slice(MAGIC);
        empty.extend_from_slice(&VERSION.to_le_bytes());
        empty.extend_from_slice(&[0; 12]);
        empty.push(0);
        let hl = empty.len();
        empty.push(0);
        let mut footer = vec![0; 8 + 24 + 32];
        footer.extend_from_slice(&(hl as u64).to_le_bytes());
        footer.extend_from_slice(&(hl as u64).to_le_bytes());
        footer.extend_from_slice(&1_u64.to_le_bytes());
        let crc = checksum(&empty[..hl], &empty[hl..], &footer);
        footer.extend_from_slice(&crc.to_le_bytes());
        empty.extend_from_slice(&footer);
        assert_eq!(
            Segment::open(&empty).unwrap_err(),
            Corrupt("segment: docmap or stored missing")
        );
    }

    fn header() -> Vec<u8> {
        let mut h = MAGIC.to_vec();
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&[0; 12]);
        h.push(0);
        h
    }

    fn file(sections: &[(u8, &str, Vec<u8>)]) -> Vec<u8> {
        let owned: Vec<_> =
            sections.iter().map(|(k, n, p)| (*k, n.to_string(), p.clone())).collect();
        let zero = Hlc { physical: 0, logical: 0 };
        assemble(header(), &owned, 0, zero, zero, None)
    }

    fn table_of(entries: &[Option<&[u8]>]) -> Vec<u8> {
        tabled(entries.iter().copied())
    }

    #[test]
    fn hand_made_sections_are_checked() {
        let doc = [0_u8; 12];
        let docmap = (Kind::Docmap as u8, "", table_of(&[Some(&doc)]));
        let stored = (Kind::Stored as u8, "", table_of(&[None]));
        let ok = file(&[docmap.clone(), stored.clone()]);
        assert_eq!(Segment::open(&ok).expect("valid").docs(), 1);
        let err = |sections: &[(u8, &str, Vec<u8>)]| Segment::open(&file(sections)).unwrap_err().0;
        let short_doc = (Kind::Docmap as u8, "", table_of(&[Some(&doc[..11])]));
        assert_eq!(err(&[short_doc, stored.clone()]), "segment: docmap entry");
        let absent_doc = (Kind::Docmap as u8, "", table_of(&[None]));
        assert_eq!(err(&[absent_doc, stored.clone()]), "segment: docmap entry");
        let two_stored = (Kind::Stored as u8, "", table_of(&[None, None]));
        assert_eq!(err(&[docmap.clone(), two_stored.clone()]), "segment: stored count");
        let column = (Kind::Column as u8, "c", table_of(&[None, None]));
        assert_eq!(err(&[docmap.clone(), stored.clone(), column]), "segment: entry count");
        let geo = (Kind::Geo as u8, "g", table_of(&[Some(&[1, 2, 3])]));
        assert_eq!(err(&[docmap.clone(), stored.clone(), geo]), "segment: geo entry");
        let geo = (Kind::Geo as u8, "g", table_of(&[None]));
        assert_eq!(err(&[docmap.clone(), stored.clone(), geo]), "segment: geo entry");
        let field = (Kind::Field as u8, "f", vec![1, 2]);
        assert_eq!(err(&[docmap.clone(), stored.clone(), field]), "segment: short");
        let bad = (Kind::Docmap as u8, "", vec![1]);
        assert_eq!(err(&[bad, stored.clone()]), "table");
        let bad = (Kind::Stored as u8, "", vec![1]);
        assert_eq!(err(&[docmap.clone(), bad]), "table");
        let bad = (Kind::Column as u8, "c", vec![1]);
        assert_eq!(err(&[docmap.clone(), stored.clone(), bad]), "table");
        assert_eq!(
            err(&[docmap.clone(), stored.clone(), (Kind::Field as u8, "\u{0}", Vec::new())]),
            "segment: short"
        );
        // Version, misplaced sections and trailing table bytes behind a valid checksum.
        let mut version = ok.clone();
        version[8] = 2;
        reseal(&mut version);
        assert_eq!(Segment::open(&version).unwrap_err().0, "segment: magic or version");
        let start = ok.len() - FOOTER;
        let to = u64::from_le_bytes(ok[start + 72..start + 80].try_into().unwrap()) as usize;
        let first_offset = to + 1 + 1 + 1;
        for (offset, what) in
            [(1_u64, "unaligned"), (0, "inside the header"), (u64::MAX - 8, "overflow")]
        {
            let mut moved = ok.clone();
            moved[first_offset..first_offset + 8].copy_from_slice(&offset.to_le_bytes());
            reseal(&mut moved);
            assert_eq!(Segment::open(&moved).unwrap_err().0, "segment: section bounds", "{what}");
        }
        let mut long = ok.clone();
        long[first_offset + 8..first_offset + 16].copy_from_slice(&(1_u64 << 20).to_le_bytes());
        reseal(&mut long);
        assert_eq!(Segment::open(&long).unwrap_err().0, "segment: section bounds");
        let mut count = ok.clone();
        count[to] = 1;
        reseal(&mut count);
        assert_eq!(Segment::open(&count).unwrap_err().0, "segment: table");
        let mut name = ok.clone();
        name[to + 2] = 0x80;
        reseal(&mut name);
        assert!(Segment::open(&name).is_err());
        let mut utf8 = file(&[(Kind::Column as u8, "x", Vec::new())]);
        let at = utf8.len() - FOOTER - 16 - 1;
        utf8[at] = 0xff;
        reseal(&mut utf8);
        assert_eq!(Segment::open(&utf8).unwrap_err().0, "segment: name");
        // A table whose count runs past its end.
        let mut count = file(&[]);
        let at = count.len() - FOOTER - 1;
        count[at] = 0x80;
        reseal(&mut count);
        assert_eq!(Segment::open(&count).unwrap_err().0, "segment: table");
        drop(ok);
    }

    #[test]
    fn inconsistent_postings_are_a_bug() {
        let cases: [fn(&mut FieldData); 4] = [
            |f| f.terms.values_mut().next().unwrap().reverse(),
            |f| f.terms.values_mut().next().unwrap()[0].doc = 10_000,
            |f| f.terms.values_mut().next().unwrap()[0].occurrences.clear(),
            |f| {
                let o = &mut f.terms.values_mut().next().unwrap()[0].occurrences;
                o.push(o[0]);
            },
        ];
        for case in cases {
            let mut data = sample(8, 50);
            case(data.fields.get_mut("body").unwrap());
            assert!(std::panic::catch_unwind(|| build(&data)).is_err());
        }
    }

    #[test]
    #[should_panic(expected = "one stored entry per document")]
    fn inconsistent_data_is_a_bug() {
        let mut data = sample(2, 3);
        data.stored.pop();
        build(&data);
    }
}
