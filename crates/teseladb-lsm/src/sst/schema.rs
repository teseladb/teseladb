//! Packed documents (003 FR-024): a data file keeps, for the documents of each collection, the
//! shapes it found (the names, order and types of their top-level fields), and a document with a
//! shape of the file is stored as the shape's number and its values alone. Reading the block
//! rebuilds the document, byte for byte the one written ([`super::block::unpack`]).
//!
//! A shape is the bytes `(type · name · 00)*`: an element of the document without its value. The
//! file's shapes go in its properties, by number; the writer takes a new shape for a collection
//! while it has fewer than [`SHAPES_PER_COLLECTION`] for it and the file's shapes take fewer than
//! [`SHAPE_BYTES`], so the shapes most of a collection's documents share come in with its first
//! documents, and one document of another shape after them is stored whole.
//!
//! ```text
//! stored document := DOCUMENT_VALUE · length(i32 LE) · (type · name · 00 · value)* · 00 · table?
//! packed document := shape(varint) · value* · table?
//! ```
//!
//! The table is the one of 003 FR-021 ([`teseladb_keys::stored`]), whose offsets point into the
//! BSON: the rebuilt BSON is the one written, so the table goes after it as it was.

use std::collections::HashMap;

use teseladb_keys::space;

use super::{Bad, get_varint, put_varint};
use crate::DOCUMENT_VALUE;

/// Most shapes a file takes for one collection.
pub(crate) const SHAPES_PER_COLLECTION: u32 = 16;
/// Most bytes the shapes of a file take.
pub(crate) const SHAPE_BYTES: usize = 64 << 10;

/// Bytes of a BSON document's length and of its final `00`.
const FRAME: usize = 5;

/// Whether `t` is the type of a BSON value.
fn known(t: u8) -> bool {
    matches!(t, 0x01..=0x13 | 0x7F | 0xFF)
}

/// The bytes of the value of type `t` at the start of `rest`, when they are there.
fn value_len(t: u8, rest: &[u8]) -> Option<usize> {
    let int = |at: usize| -> Option<usize> {
        let b = rest.get(at..at + 4)?;
        usize::try_from(i32::from_le_bytes(b.try_into().ok()?)).ok()
    };
    let cstring = |from: usize| -> Option<usize> {
        rest.get(from..)?.iter().position(|b| *b == 0).map(|p| from + p + 1)
    };
    // A string: its length, counting its final `00`, then its bytes.
    let string = |at: usize| -> Option<usize> {
        let n = int(at)?;
        let end = (at + 4).checked_add(n)?;
        (n >= 1 && rest.get(end - 1) == Some(&0)).then_some(end)
    };
    let len = match t {
        0x06 | 0x0A | 0x7F | 0xFF => 0,
        0x08 => 1,
        0x10 => 4,
        0x01 | 0x09 | 0x11 | 0x12 => 8,
        0x07 => 12,
        0x13 => 16,
        0x02 | 0x0D | 0x0E => string(0)?,
        // A document, an array and code with scope give their whole length.
        0x03 | 0x04 | 0x0F => int(0).filter(|n| *n >= FRAME)?,
        0x05 => int(0)?.checked_add(5)?,
        0x0B => cstring(cstring(0)?)?,
        0x0C => string(0)? + 12,
        _ => return None,
    };
    (len <= rest.len()).then_some(len)
}

/// Whether `doc` (a stored document without its first byte) is a BSON document followed by a
/// table that holds.
fn tabled(doc: &[u8]) -> bool {
    teseladb_keys::stored::split(doc).is_ok_and(|s| s.table().is_some())
}

/// Splits stored document `doc` (without its first byte) into its shape, its values and the
/// length of its BSON, the table after it left out: `None` when its frame, an element or its
/// table does not hold, which leaves it whole.
fn split(doc: &[u8], shape: &mut Vec<u8>, values: &mut Vec<u8>) -> Option<usize> {
    shape.clear();
    values.clear();
    let declared = doc.first_chunk::<4>().map(|b| i32::from_le_bytes(*b));
    let n = declared.and_then(|n| usize::try_from(n).ok()).filter(|n| *n <= doc.len())?;
    if n < FRAME || doc[n - 1] != 0 || (n < doc.len() && !tabled(doc)) {
        return None;
    }
    let doc = &doc[..n];
    let body = &doc[4..doc.len() - 1];
    let mut at = 0;
    while at < body.len() {
        let t = body[at];
        let name = body[at + 1..].iter().position(|b| *b == 0)?;
        let value = at + 1 + name + 1;
        let len = value_len(t, &body[value..])?;
        shape.extend_from_slice(&body[at..value]);
        values.extend_from_slice(&body[value..value + len]);
        at = value + len;
    }
    Some(n)
}

/// Checks that `shape` is a sequence of elements without values, as a file's properties keep
/// it.
pub(crate) fn check_shape(shape: &[u8]) -> Result<(), Bad> {
    let mut at = 0;
    while at < shape.len() {
        // A type a value can have, then its name.
        if !known(shape[at]) {
            return Err(Bad);
        }
        let name = shape[at + 1..].iter().position(|b| *b == 0).ok_or(Bad)?;
        at += 1 + name + 1;
    }
    Ok(())
}

/// Rebuilds into `out` the stored document `packed` holds, with the shapes of its file. The bytes
/// after the values are the document's table, which must hold on the rebuilt BSON.
pub(crate) fn unpack(shapes: &[Vec<u8>], packed: &[u8], out: &mut Vec<u8>) -> Result<(), Bad> {
    let mut at = 0;
    let number = usize::try_from(get_varint(packed, &mut at)?).map_err(|_| Bad)?;
    let shape = shapes.get(number).ok_or(Bad)?;
    let start = out.len();
    out.push(DOCUMENT_VALUE);
    out.extend_from_slice(&[0; 4]);
    let mut s = 0;
    while s < shape.len() {
        let name = shape[s + 1..].iter().position(|b| *b == 0).ok_or(Bad)?;
        let element = &shape[s..s + 1 + name + 1];
        let len = value_len(shape[s], &packed[at..]).ok_or(Bad)?;
        out.extend_from_slice(element);
        out.extend_from_slice(&packed[at..at + len]);
        at += len;
        s += element.len();
    }
    out.push(0);
    let len = i32::try_from(out.len() - start - 1).map_err(|_| Bad)?;
    out[start + 1..start + FRAME].copy_from_slice(&len.to_le_bytes());
    if at < packed.len() {
        out.extend_from_slice(&packed[at..]);
        if !tabled(&out[start + 1..]) {
            return Err(Bad);
        }
    }
    Ok(())
}

/// The shapes a writer has taken and the documents it packs with them.
#[derive(Debug, Default)]
pub(crate) struct Packer {
    /// The file's shapes, by number.
    shapes: Vec<Vec<u8>>,
    /// The number of each shape.
    numbers: HashMap<Vec<u8>, u32>,
    /// How many shapes each collection took, by the collection's prefix.
    taken: HashMap<Vec<u8>, u32>,
    bytes: usize,
    shape: Vec<u8>,
    values: Vec<u8>,
}

impl Packer {
    /// The packed form of `value`, the value of a `Put` at logical key `logical`, when it is a
    /// document of a collection with a shape of the file or one the file can still take.
    pub(crate) fn pack(&mut self, logical: &[u8], value: &[u8]) -> Option<Vec<u8>> {
        if !space::in_documents(logical) || value.first() != Some(&DOCUMENT_VALUE) {
            return None;
        }
        let n = split(&value[1..], &mut self.shape, &mut self.values)?;
        let number = match self.numbers.get(&self.shape) {
            Some(n) => *n,
            None => self.take(logical)?,
        };
        let table = &value[1 + n..];
        let mut out = Vec::with_capacity(self.values.len() + table.len() + 5);
        put_varint(&mut out, u64::from(number));
        out.extend_from_slice(&self.values);
        out.extend_from_slice(table);
        Some(out)
    }

    /// Takes the shape just split as a new one of the collection of `logical`, when there is
    /// room.
    fn take(&mut self, logical: &[u8]) -> Option<u32> {
        let collection = space::collection_of(logical)?;
        let taken = self.taken.get(collection).copied().unwrap_or(0);
        if taken == SHAPES_PER_COLLECTION || self.bytes + self.shape.len() > SHAPE_BYTES {
            return None;
        }
        let number = u32::try_from(self.shapes.len()).ok()?;
        self.taken.insert(collection.to_vec(), taken + 1);
        self.bytes += self.shape.len();
        self.numbers.insert(self.shape.clone(), number);
        self.shapes.push(self.shape.clone());
        Some(number)
    }

    /// The file's shapes, by number.
    pub(crate) fn finish(self) -> Vec<Vec<u8>> {
        self.shapes
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::{Document, RawValue};

    use super::*;

    /// A document's key in collection `coll`.
    pub(crate) fn doc_key(coll: u32, i: u32) -> Vec<u8> {
        teseladb_keys::KeyBuilder::document(1, coll, RawValue::Int32(i as i32)).as_bytes().to_vec()
    }

    fn stored(doc: &Document) -> Vec<u8> {
        let mut v = vec![DOCUMENT_VALUE];
        v.extend_from_slice(&doc.to_bytes().unwrap());
        v
    }

    /// A document with a value of every type.
    fn every_type() -> Document {
        let ext = r#"{"_id": {"$oid": "65a1b2c3d4e5f60718293a4b"}, "d": 1.5, "s": "héllo",
            "o": {"a": 1, "b": [1, "x"]}, "arr": [{"k": null}], "bin": {"$binary": {"base64":
            "AAEC", "subType": "04"}}, "u": {"$undefined": true}, "t": true, "dt": {"$date":
            {"$numberLong": "1700000000000"}}, "n": null, "re": {"$regularExpression":
            {"pattern": "a.c", "options": "im"}}, "ptr": {"$dbPointer": {"$ref": "c", "$id":
            {"$oid": "65a1b2c3d4e5f60718293a4b"}}}, "js": {"$code": "f()"}, "sym": {"$symbol":
            "z"}, "jss": {"$code": "g()", "$scope": {"x": 1}}, "i": 7, "ts": {"$timestamp":
            {"t": 5, "i": 6}}, "l": {"$numberLong": "9"}, "dec": {"$numberDecimal": "1.25"},
            "min": {"$minKey": 1}, "max": {"$maxKey": 1}, "": 0}"#;
        teseladb_bson::extjson::parse(ext).unwrap()
    }

    #[test]
    fn a_document_of_every_type_packs_and_comes_back_byte_for_byte() {
        let mut p = Packer::default();
        let value = stored(&every_type());
        let packed = p.pack(&doc_key(7, 1), &value).expect("a document packs");
        let shapes = p.finish();
        assert_eq!(shapes.len(), 1);
        // Without the value's tag, the frame, the types and the names, with the shape's number.
        assert_eq!(packed.len(), value.len() - 1 - FRAME - shapes[0].len() + 1);
        check_shape(&shapes[0]).unwrap();
        let mut out = Vec::new();
        unpack(&shapes, &packed, &mut out).unwrap();
        assert_eq!(out, value);
        // An empty document has an empty shape.
        let mut p = Packer::default();
        let empty = stored(&Document::new());
        let packed = p.pack(&doc_key(7, 1), &empty).unwrap();
        let mut out = Vec::new();
        unpack(&p.finish(), &packed, &mut out).unwrap();
        assert_eq!(out, empty);
    }

    #[test]
    fn a_document_with_its_table_packs_and_comes_back_byte_for_byte() {
        let mut p = Packer::default();
        let mut value = vec![DOCUMENT_VALUE];
        value.extend(teseladb_keys::stored::encode(every_type().to_bytes().unwrap()));
        assert!(tabled(&value[1..]), "the document has a table");
        let packed = p.pack(&doc_key(7, 1), &value).expect("a document with a table packs");
        let shapes = p.finish();
        let mut out = Vec::new();
        unpack(&shapes, &packed, &mut out).unwrap();
        assert_eq!(out, value);
        // A table that does not hold on the rebuilt document is refused, and so is a document
        // with bytes after its BSON that are not a table.
        let mut broken = packed.clone();
        *broken.last_mut().unwrap() ^= 1;
        out.clear();
        assert_eq!(unpack(&shapes, &broken, &mut out), Err(Bad));
        let mut p = Packer::default();
        let mut trailing = stored(&Document::new().with("a", 1));
        trailing.extend_from_slice(&[1, 2, 3, 4]);
        assert!(p.pack(&doc_key(7, 1), &trailing).is_none());
    }

    #[test]
    fn documents_of_one_shape_share_its_number() {
        let mut p = Packer::default();
        let a = stored(&Document::new().with("_id", 1).with("x", "a"));
        let b = stored(&Document::new().with("_id", 2).with("x", "bb"));
        let other = stored(&Document::new().with("_id", 3).with("x", 4));
        let pa = p.pack(&doc_key(7, 1), &a).unwrap();
        let pb = p.pack(&doc_key(7, 2), &b).unwrap();
        let po = p.pack(&doc_key(7, 3), &other).unwrap();
        assert_eq!((pa[0], pb[0], po[0]), (0, 0, 1));
        // The same shape in another collection is the same shape.
        assert_eq!(p.pack(&doc_key(8, 1), &a).unwrap()[0], 0);
        let shapes = p.finish();
        let mut out = Vec::new();
        for (packed, whole) in [(pa, a), (pb, b), (po, other)] {
            out.clear();
            unpack(&shapes, &packed, &mut out).unwrap();
            assert_eq!(out, whole);
        }
    }

    #[test]
    fn a_collection_takes_a_bounded_number_of_shapes() {
        let mut p = Packer::default();
        for i in 0..SHAPES_PER_COLLECTION + 4 {
            let doc = stored(&Document::new().with(format!("f{i}"), 1));
            let packed = p.pack(&doc_key(7, i), &doc);
            assert_eq!(packed.is_some(), i < SHAPES_PER_COLLECTION, "shape {i}");
        }
        // Another collection still takes its own.
        let doc = stored(&Document::new().with("g", 1));
        assert!(p.pack(&doc_key(9, 0), &doc).is_some());
        // And the shapes of all of them take a bounded number of bytes.
        let mut p = Packer::default();
        let long = "n".repeat(1000);
        let packed = (0..100u32)
            .filter_map(|i| {
                let doc = stored(&Document::new().with(format!("{long}{i}"), 1));
                p.pack(&doc_key(i, 0), &doc)
            })
            .count();
        assert_eq!(packed, SHAPE_BYTES / 1006);
    }

    #[test]
    fn what_is_not_a_document_of_a_collection_stays_whole() {
        let mut p = Packer::default();
        let doc = stored(&Document::new().with("a", 1));
        // An index entry, a key of another space and a value that is not a document.
        let index = teseladb_keys::KeyBuilder::index_prefix(1, 7, 2).as_bytes().to_vec();
        assert!(p.pack(&index, &doc).is_none());
        assert!(p.pack(&[space::SYSTEM, 1], &doc).is_none());
        assert!(p.pack(&doc_key(7, 1), &[crate::HISTORY_VALUE, 1, 2]).is_none());
        assert!(p.pack(&doc_key(7, 1), &[]).is_none());
        // Broken frames and elements.
        let mut short = doc.clone();
        short[1] += 1;
        assert!(p.pack(&doc_key(7, 1), &short).is_none(), "a length that is not its own");
        let mut unended = doc.clone();
        *unended.last_mut().unwrap() = 1;
        assert!(p.pack(&doc_key(7, 1), &unended).is_none(), "no final zero");
        let mut bad_type = doc.clone();
        bad_type[5] = 0x42;
        assert!(p.pack(&doc_key(7, 1), &bad_type).is_none(), "an unknown type");
        let mut no_name_end = vec![DOCUMENT_VALUE, 8, 0, 0, 0, 0x10, b'a', b'b', 0];
        assert!(p.pack(&doc_key(7, 1), &no_name_end).is_none(), "a value cut short");
        no_name_end[5] = 0x02;
        assert!(p.pack(&doc_key(7, 1), &no_name_end).is_none(), "a string cut short");
        assert!(p.finish().is_empty());
    }

    #[test]
    fn broken_packed_values_and_shapes_are_errors() {
        let mut p = Packer::default();
        let doc = stored(&Document::new().with("a", 1).with("s", "xy"));
        let packed = p.pack(&doc_key(7, 1), &doc).unwrap();
        let shapes = p.finish();
        let mut out = Vec::new();
        assert_eq!(unpack(&shapes, &[1], &mut out), Err(Bad), "a shape the file lacks");
        assert_eq!(unpack(&shapes, &packed[..packed.len() - 1], &mut out), Err(Bad), "cut short");
        let longer = [&packed[..], &[0]].concat();
        assert_eq!(unpack(&shapes, &longer, &mut out), Err(Bad), "bytes after the values");
        assert_eq!(unpack(&shapes, &[], &mut out), Err(Bad));
        assert_eq!(check_shape(&[0x42, b'a', 0]), Err(Bad), "an unknown type");
        assert_eq!(check_shape(&[0x10, b'a']), Err(Bad), "a name without its end");
        assert_eq!(check_shape(&[0x10, 0]), Ok(()));
        let unnamed = vec![vec![0x10, b'a']];
        assert_eq!(unpack(&unnamed, &[0, 1, 0, 0, 0], &mut out), Err(Bad));
    }
}
