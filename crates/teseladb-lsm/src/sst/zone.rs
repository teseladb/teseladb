//! Zone maps (FR-039): for the top-level fields that most documents of a file hold, the lowest
//! and the highest value of each data block, so that a scan with a filter skips the blocks whose
//! interval the filter rules out without reading them.
//!
//! ```text
//! zones := fields(varint) · name(bytes)* · blocks(varint) · block*
//! block := flags(u8) · field*            (the fields only when the block is summarised)
//! field := 0 (unknown) | 1 (absent) | 2 · min(bytes) · max(bytes)
//! ```
//!
//! Flag bit 0: the block is summarised, every version in it is a document, the history of one or
//! a deletion. Flag bit 1: the logical key of its last version goes on in the next block.
//!
//! The bounds are encodings of `teseladb_keys::encode_value` and cover every version of the
//! block, old ones too, since a read below the newest version of a key sees an older one. A
//! deletion and a history value match no filter and leave the bounds as they are. A block with
//! an intent, a `Retract`, a value in a value file or a value that is not a document is not
//! summarised, nor is a field whose value is an array or encodes to more than [`MAX_VALUE`]
//! bytes in that block.

use teseladb_keys::zone::{Bounds, Zones};
use teseladb_keys::{Direction, TypeBits, encode_value, space};
use teseladb_bson::{RawDoc, RawValue};

use super::{Bad, get_bytes, get_varint, put_bytes, put_varint};
use crate::entry::{self, Kind};
use crate::{DOCUMENT_VALUE, HISTORY_VALUE};

/// Longest encoded value a zone keeps as a bound.
pub(crate) const MAX_VALUE: usize = 64;
/// Distinct top-level fields a file counts: the first ones its documents bring.
const MAX_TRACKED: usize = 64;
/// Fields a file keeps zones for, those most of its documents hold.
const MAX_FIELDS: usize = 16;

const SUMMARISED: u8 = 1;
const CONTINUES: u8 = 2;

const UNKNOWN: u8 = 0;
const ABSENT: u8 = 1;
const WITHIN: u8 = 2;

/// What a block knows of one field.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Field {
    Unknown,
    Absent,
    Within(Box<[u8]>, Box<[u8]>),
}

impl Field {
    fn widen(&mut self, encoded: Option<&[u8]>) {
        let Some(v) = encoded else {
            *self = Field::Unknown;
            return;
        };
        match self {
            Field::Unknown => {}
            Field::Absent => *self = Field::Within(v.into(), v.into()),
            Field::Within(min, max) => {
                if v < &**min {
                    *min = v.into();
                }
                if v > &**max {
                    *max = v.into();
                }
            }
        }
    }
}

/// One block while it is written.
#[derive(Debug, Default)]
struct Open {
    summarised: bool,
    continues: bool,
    /// By tracked field; a field beyond the end is absent from every version so far.
    fields: Vec<Field>,
}

/// Builds the zone map of a file as its versions go in.
#[derive(Debug)]
pub(crate) struct ZoneBuilder {
    names: Vec<String>,
    /// Documents holding each tracked field.
    counts: Vec<u64>,
    docs: u64,
    blocks: Vec<Open>,
    open: Open,
    any: bool,
    encoded: Vec<u8>,
}

impl Default for ZoneBuilder {
    fn default() -> ZoneBuilder {
        ZoneBuilder {
            names: Vec::new(),
            counts: Vec::new(),
            docs: 0,
            blocks: Vec::new(),
            open: Open { summarised: true, ..Open::default() },
            any: false,
            encoded: Vec::new(),
        }
    }
}

impl ZoneBuilder {
    /// Counts a version of the open block; `value` is `None` when it is in a value file.
    pub(crate) fn add(&mut self, ikey: &[u8], kind: Kind, value: Option<&[u8]>) {
        self.any = true;
        if !self.open.summarised {
            return;
        }
        let logical = entry::logical(ikey);
        let tag = value.and_then(|v| v.first().copied());
        match (kind, tag) {
            (Kind::Delete, _) => {}
            (Kind::Put, Some(HISTORY_VALUE)) if space::in_documents(logical) => {}
            (Kind::Put, Some(DOCUMENT_VALUE)) if space::in_documents(logical) => {
                let doc = value.and_then(|v| teseladb_keys::stored::raw(&v[1..]).ok());
                match doc {
                    Some(doc) => self.document(doc),
                    None => self.open.summarised = false,
                }
            }
            _ => self.open.summarised = false,
        }
    }

    fn document(&mut self, doc: RawDoc<'_>) {
        self.docs += 1;
        for (name, value) in doc.iter() {
            let at = match self.names.iter().position(|n| n == name) {
                Some(at) => at,
                None if self.names.len() < MAX_TRACKED => {
                    self.names.push(name.to_owned());
                    self.counts.push(0);
                    self.names.len() - 1
                }
                None => continue,
            };
            self.counts[at] += 1;
            if self.open.fields.len() <= at {
                self.open.fields.resize(at + 1, Field::Absent);
            }
            let encoded = match value {
                RawValue::Array(_) => None,
                v => {
                    self.encoded.clear();
                    encode_value(
                        &mut self.encoded,
                        v,
                        Direction::Ascending,
                        &mut TypeBits::default(),
                    );
                    (self.encoded.len() <= MAX_VALUE).then_some(self.encoded.as_slice())
                }
            };
            self.open.fields[at].widen(encoded);
        }
    }

    /// Ends the open block.
    pub(crate) fn end_block(&mut self) {
        let next = Open { summarised: true, ..Open::default() };
        self.blocks.push(std::mem::replace(&mut self.open, next));
    }

    /// The block just ended ends with a key whose versions go on in the next one.
    pub(crate) fn continues(&mut self) {
        if let Some(b) = self.blocks.last_mut() {
            b.continues = true;
        }
    }

    /// The zone block's payload, when a field is held by most documents and a block is
    /// summarised.
    pub(crate) fn finish(self) -> Option<Vec<u8>> {
        if !self.any || self.blocks.iter().all(|b| !b.summarised) {
            return None;
        }
        let mut kept: Vec<usize> =
            (0..self.names.len()).filter(|&i| 2 * self.counts[i] > self.docs).collect();
        kept.sort_by_key(|&i| std::cmp::Reverse(self.counts[i]));
        kept.truncate(MAX_FIELDS);
        if kept.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        put_varint(&mut out, kept.len() as u64);
        for &i in &kept {
            put_bytes(&mut out, self.names[i].as_bytes());
        }
        put_varint(&mut out, self.blocks.len() as u64);
        for b in &self.blocks {
            let flags = (u8::from(b.summarised) * SUMMARISED) | (u8::from(b.continues) * CONTINUES);
            out.push(flags);
            if !b.summarised {
                continue;
            }
            for &i in &kept {
                match b.fields.get(i).unwrap_or(&Field::Absent) {
                    Field::Unknown => out.push(UNKNOWN),
                    Field::Absent => out.push(ABSENT),
                    Field::Within(min, max) => {
                        out.push(WITHIN);
                        put_bytes(&mut out, min);
                        put_bytes(&mut out, max);
                    }
                }
            }
        }
        Some(out)
    }
}

/// One block of a zone map.
#[derive(Debug, PartialEq, Eq)]
struct Block {
    flags: u8,
    fields: Box<[Field]>,
}

/// The zone map of a file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ZoneMap {
    names: Box<[Box<str>]>,
    blocks: Box<[Block]>,
}

impl ZoneMap {
    /// From the zone block's payload, for a file of `blocks` data blocks.
    pub(crate) fn decode(b: &[u8], blocks: usize) -> Result<ZoneMap, Bad> {
        let mut at = 0;
        let n = usize::try_from(get_varint(b, &mut at)?).map_err(|_| Bad)?;
        if n == 0 || n > MAX_FIELDS {
            return Err(Bad);
        }
        let mut names = Vec::with_capacity(n);
        for _ in 0..n {
            let name = std::str::from_utf8(get_bytes(b, &mut at)?).map_err(|_| Bad)?;
            names.push(Box::from(name));
        }
        if get_varint(b, &mut at)? != blocks as u64 {
            return Err(Bad);
        }
        let mut out = Vec::with_capacity(blocks);
        for _ in 0..blocks {
            let flags = *b.get(at).ok_or(Bad)?;
            at += 1;
            if flags & !(SUMMARISED | CONTINUES) != 0 {
                return Err(Bad);
            }
            let mut fields = Vec::new();
            if flags & SUMMARISED != 0 {
                for _ in 0..n {
                    let tag = *b.get(at).ok_or(Bad)?;
                    at += 1;
                    fields.push(match tag {
                        UNKNOWN => Field::Unknown,
                        ABSENT => Field::Absent,
                        WITHIN => {
                            let min = get_bytes(b, &mut at)?;
                            let max = get_bytes(b, &mut at)?;
                            if min > max {
                                return Err(Bad);
                            }
                            Field::Within(min.into(), max.into())
                        }
                        _ => return Err(Bad),
                    });
                }
            }
            out.push(Block { flags, fields: fields.into() });
        }
        if at != b.len() {
            return Err(Bad);
        }
        Ok(ZoneMap { names: names.into(), blocks: out.into() })
    }

    /// The zones of block `i`, when it was summarised and its last key does not go on in the
    /// next block: only such a block may be skipped whole.
    pub(crate) fn block(&self, i: usize) -> Option<BlockZones<'_>> {
        let b = self.blocks.get(i)?;
        (b.flags == SUMMARISED).then_some(BlockZones { map: self, block: b })
    }
}

/// The zones of one data block.
#[derive(Debug)]
pub(crate) struct BlockZones<'a> {
    map: &'a ZoneMap,
    block: &'a Block,
}

impl Zones for BlockZones<'_> {
    fn field(&self, name: &str) -> Bounds<'_> {
        let Some(i) = self.map.names.iter().position(|n| &**n == name) else {
            return Bounds::Unknown;
        };
        match &self.block.fields[i] {
            Field::Unknown => Bounds::Unknown,
            Field::Absent => Bounds::Absent,
            Field::Within(min, max) => Bounds::Within { min, max },
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_keys::{Hlc, KeyBuilder};
    use teseladb_bson::Document;

    use super::*;

    fn ikey(id: i32, ts: u64) -> Vec<u8> {
        let key = KeyBuilder::document(1, 7, RawValue::Int32(id));
        entry::internal_key(key.as_bytes(), Hlc { physical: ts, logical: 0 })
    }

    fn stored(d: Document) -> Vec<u8> {
        [&[DOCUMENT_VALUE][..], &d.to_bytes().unwrap()].concat()
    }

    fn enc(v: RawValue<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        encode_value(&mut out, v, Direction::Ascending, &mut TypeBits::default());
        out
    }

    #[test]
    fn a_zone_map_bounds_every_version_and_round_trips() {
        let mut z = ZoneBuilder::default();
        let doc = |a: i32, b: &str| stored(Document::new().with("a", a).with("b", b));
        z.add(&ikey(1, 9), Kind::Put, Some(&doc(5, "x")));
        // An older version of the same key widens the bounds too.
        z.add(&ikey(1, 3), Kind::Put, Some(&doc(40, "y")));
        z.add(&ikey(2, 9), Kind::Delete, Some(b""));
        z.end_block();
        z.add(&ikey(3, 9), Kind::Put, Some(&doc(7, "z")));
        z.end_block();
        // The next block starts with a version of key 3: the block just ended goes on there.
        z.continues();
        // A block with an intent is not summarised.
        z.add(&ikey(4, 9), Kind::Intent, Some(b"i"));
        z.end_block();
        // A block with only a deletion holds none of the fields.
        z.add(&ikey(5, 9), Kind::Delete, Some(b""));
        z.end_block();
        let map = ZoneMap::decode(&z.finish().unwrap(), 4).unwrap();
        let b0 = map.block(0).unwrap();
        let (five, forty) = (enc(RawValue::Int32(5)), enc(RawValue::Double(40.0)));
        assert_eq!(b0.field("a"), Bounds::Within { min: &five, max: &forty });
        assert_eq!(b0.field("missing"), Bounds::Unknown);
        assert!(map.block(1).is_none(), "its last key goes on");
        assert!(map.block(2).is_none(), "not summarised");
        assert_eq!(map.block(3).unwrap().field("b"), Bounds::Absent);
        assert!(ZoneMap::decode(&ZoneBuilder::default().finish().unwrap_or_default(), 0).is_err());
    }

    #[test]
    fn a_document_stored_with_its_table_is_summarised_as_its_bson() {
        let wide = |a: i32| {
            let mut d = Document::new().with("a", a);
            for i in 0..teseladb_keys::stored::MIN_TABLE_FIELDS {
                d.push(format!("f{i}"), i as i32);
            }
            d
        };
        let (mut plain, mut tabled) = (ZoneBuilder::default(), ZoneBuilder::default());
        for a in [3, 11] {
            let bson = wide(a).to_bytes().unwrap();
            let kept = teseladb_keys::stored::encode(bson.clone());
            assert!(kept.len() > bson.len());
            plain.add(&ikey(a, 9), Kind::Put, Some(&[&[DOCUMENT_VALUE][..], &bson].concat()));
            tabled.add(&ikey(a, 9), Kind::Put, Some(&[&[DOCUMENT_VALUE][..], &kept].concat()));
        }
        plain.end_block();
        tabled.end_block();
        let (p, t) = (plain.finish().unwrap(), tabled.finish().unwrap());
        assert_eq!(p, t);
        let map = ZoneMap::decode(&t, 1).unwrap();
        let (three, eleven) = (enc(RawValue::Int32(3)), enc(RawValue::Int32(11)));
        assert_eq!(map.block(0).unwrap().field("a"), Bounds::Within { min: &three, max: &eleven });
    }

    #[test]
    fn arrays_long_values_and_rare_fields_are_not_bounded() {
        let mut z = ZoneBuilder::default();
        let arr = Document::new()
            .with("a", vec![teseladb_bson::Value::from(1i32), teseladb_bson::Value::from(2i32)])
            .with("s", "x".repeat(MAX_VALUE));
        z.add(&ikey(1, 9), Kind::Put, Some(&stored(arr.with("rare", 1i32))));
        for i in 2..5 {
            z.add(
                &ikey(i, 9),
                Kind::Put,
                Some(&stored(Document::new().with("a", i).with("s", "y"))),
            );
        }
        z.end_block();
        let map = ZoneMap::decode(&z.finish().unwrap(), 1).unwrap();
        let b = map.block(0).unwrap();
        assert_eq!(b.field("a"), Bounds::Unknown);
        assert_eq!(b.field("s"), Bounds::Unknown);
        assert_eq!(b.field("rare"), Bounds::Unknown, "held by one document in four");
    }

    #[test]
    fn values_that_are_not_documents_leave_the_block_unsummarised() {
        for (key, value) in [
            (ikey(1, 9), vec![9u8, 1, 2]),
            (ikey(1, 9), vec![DOCUMENT_VALUE, 1, 2, 3]),
            (
                entry::internal_key(b"\x02system", Hlc { physical: 1, logical: 0 }),
                stored(Document::new()),
            ),
        ] {
            let mut z = ZoneBuilder::default();
            z.add(&ikey(2, 9), Kind::Put, Some(&stored(Document::new().with("a", 1i32))));
            z.add(&key, Kind::Put, Some(&value));
            z.end_block();
            z.add(&ikey(3, 9), Kind::Put, Some(&stored(Document::new().with("a", 1i32))));
            z.end_block();
            let map = ZoneMap::decode(&z.finish().unwrap(), 2).unwrap();
            assert!(map.block(0).is_none());
            assert!(map.block(1).is_some());
        }
        let mut z = ZoneBuilder::default();
        z.add(&ikey(2, 9), Kind::Put, None);
        z.end_block();
        assert!(z.finish().is_none(), "a value in a value file");
    }

    #[test]
    fn a_damaged_zone_block_does_not_decode() {
        let mut z = ZoneBuilder::default();
        z.add(&ikey(1, 9), Kind::Put, Some(&stored(Document::new().with("a", 1i32))));
        z.end_block();
        let good = z.finish().unwrap();
        assert!(ZoneMap::decode(&good, 1).is_ok());
        assert!(ZoneMap::decode(&good, 2).is_err(), "another number of blocks");
        assert!(ZoneMap::decode(&good[..good.len() - 1], 1).is_err());
        assert!(ZoneMap::decode(&[good.clone(), vec![0]].concat(), 1).is_err());
        let mut flags = good.clone();
        let at = good.len() - 1 - 2 * (1 + enc(RawValue::Int32(1)).len()) - 1;
        flags[at] = 0x80;
        assert!(ZoneMap::decode(&flags, 1).is_err());
    }

    #[test]
    fn a_zone_block_with_no_fields_too_many_or_bounds_upside_down_does_not_decode() {
        // One field `a`, one summarised block with `a` within `[min, max]`.
        let payload = |min: &[u8], max: &[u8]| {
            let mut out = Vec::new();
            put_varint(&mut out, 1);
            put_bytes(&mut out, b"a");
            put_varint(&mut out, 1);
            out.push(SUMMARISED);
            out.push(WITHIN);
            put_bytes(&mut out, min);
            put_bytes(&mut out, max);
            out
        };
        let (three, five) = (enc(RawValue::Int32(3)), enc(RawValue::Int32(5)));
        let map = ZoneMap::decode(&payload(&three, &five), 1).unwrap();
        assert_eq!(map.block(0).unwrap().field("a"), Bounds::Within { min: &three, max: &five });
        assert!(ZoneMap::decode(&payload(&five, &three), 1).is_err());
        let mut unknown_tag = payload(&three, &five);
        let tag_at = unknown_tag.len() - 2 * (1 + three.len()) - 1;
        unknown_tag[tag_at] = 9;
        assert!(ZoneMap::decode(&unknown_tag, 1).is_err());
        let mut none = Vec::new();
        put_varint(&mut none, 0);
        put_varint(&mut none, 0);
        assert!(ZoneMap::decode(&none, 0).is_err());
        let mut many = Vec::new();
        put_varint(&mut many, (MAX_FIELDS + 1) as u64);
        assert!(ZoneMap::decode(&many, 0).is_err());
    }

    #[test]
    fn history_values_keep_the_block_summarised_and_fields_past_the_tracked_ones_are_dropped() {
        let mut z = ZoneBuilder::default();
        // Nothing ended yet: there is no block whose versions go on.
        z.continues();
        let mut wide = Document::new().with("a", 1i32);
        for i in 0..MAX_TRACKED {
            wide = wide.with(format!("f{i:02}").as_str(), 1i32);
        }
        z.add(&ikey(1, 9), Kind::Put, Some(&stored(wide.clone())));
        z.add(&ikey(1, 5), Kind::Put, Some(&[HISTORY_VALUE, 1, 2]));
        z.add(&ikey(2, 9), Kind::Put, Some(&stored(wide)));
        z.end_block();
        let map = ZoneMap::decode(&z.finish().unwrap(), 1).unwrap();
        let b = map.block(0).expect("summarised, its last key does not go on");
        let one = enc(RawValue::Int32(1));
        assert_eq!(b.field("a"), Bounds::Within { min: &one, max: &one });
        let last = format!("f{:02}", MAX_TRACKED - 1);
        assert_eq!(b.field(&last), Bounds::Unknown, "past the names it tracks");
    }
}
