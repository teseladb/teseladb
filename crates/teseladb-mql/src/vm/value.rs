//! The registers of the machine: [`ValueRef`], 16 bytes that hold a scalar or the place of a
//! value in the document's bytes, never memory of their own; and the walk of BSON that finds a
//! field or the next element of an array on the bytes.

use std::marker::PhantomData;

use teseladb_keys::stored::{FieldKey, Lookup, Stored};
use teseladb_bson::{Decimal128, RawDoc, RawValue};

/// What a register holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Tag {
    /// The path is missing (or ends past a value that is not a document).
    Missing = 0,
    Null,
    Bool,
    I32,
    I64,
    F64,
    Str,
    Doc,
    Array,
    Undefined,
    ObjectId,
    Date,
    Regex,
    DbPointer,
    Js,
    Symbol,
    JsScope,
    Timestamp,
    Decimal,
    Binary,
    MinKey,
    MaxKey,
    /// The path crosses an array before its end: the tree evaluator decides.
    ThroughArray,
    /// An iterator over an array: the next element's offset and the array's end.
    Iter,
}

/// A register: a type tag, a 64-bit payload (an integer, a double's bits, a boolean, an offset in
/// the document) and a length, in 16 bytes. Strings, documents, arrays and the other values of
/// variable size are offsets and lengths into the document's bytes.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(crate) struct ValueRef<'d> {
    pub tag: Tag,
    pub len: u32,
    pub payload: u64,
    _doc: PhantomData<&'d [u8]>,
}

impl<'d> ValueRef<'d> {
    pub(crate) const MISSING: ValueRef<'static> = ValueRef::new(Tag::Missing, 0, 0);

    pub(crate) const fn new(tag: Tag, len: u32, payload: u64) -> ValueRef<'d> {
        ValueRef { tag, len, payload, _doc: PhantomData }
    }

    pub(crate) const fn boolean(b: bool) -> ValueRef<'d> {
        ValueRef::new(Tag::Bool, 0, b as u64)
    }

    /// Whether the register holds `true`.
    #[inline(always)]
    pub(crate) fn truth(self) -> bool {
        matches!(self.tag, Tag::Bool) && self.payload != 0
    }

    /// The offset of a value kept in the document.
    #[inline(always)]
    fn at(self) -> usize {
        self.payload as usize
    }

    /// The bytes of a value kept in the document.
    #[inline(always)]
    pub(crate) fn bytes(self, doc: &'d [u8]) -> Option<&'d [u8]> {
        doc.get(self.at()..self.at().checked_add(self.len as usize)?)
    }

    /// The value as BSON, for the generic comparisons; `None` when missing.
    pub(crate) fn raw(self, doc: &'d [u8]) -> Option<RawValue<'d>> {
        let text = |v: ValueRef<'d>| v.bytes(doc).and_then(|b| std::str::from_utf8(b).ok());
        Some(match self.tag {
            Tag::Missing | Tag::ThroughArray | Tag::Iter => return None,
            Tag::Null => RawValue::Null,
            Tag::Undefined => RawValue::Undefined,
            Tag::MinKey => RawValue::MinKey,
            Tag::MaxKey => RawValue::MaxKey,
            Tag::Bool => RawValue::Boolean(self.payload != 0),
            Tag::I32 => RawValue::Int32(self.payload as i32),
            Tag::I64 => RawValue::Int64(self.payload as i64),
            Tag::F64 => RawValue::Double(f64::from_bits(self.payload)),
            Tag::Date => RawValue::DateTime(self.payload as i64),
            Tag::Timestamp => RawValue::Timestamp {
                time: (self.payload >> 32) as u32,
                increment: self.payload as u32,
            },
            Tag::ObjectId => {
                let mut id = [0u8; 12];
                id[..8].copy_from_slice(&self.payload.to_le_bytes());
                id[8..].copy_from_slice(&self.len.to_le_bytes());
                RawValue::ObjectId(id)
            }
            Tag::Str => RawValue::String(text(self)?),
            Tag::Symbol => RawValue::Symbol(text(self)?),
            Tag::Js => RawValue::JavaScript(text(self)?),
            Tag::Doc => RawValue::Document(RawDoc::from_stored(self.bytes(doc)?).ok()?),
            Tag::Array => return decode_at(doc, self.at(), teseladb_bson::types::ARRAY),
            Tag::Decimal => {
                let b: [u8; 16] = self.bytes(doc)?.try_into().ok()?;
                RawValue::Decimal128(Decimal128::from_le_bytes(b))
            }
            Tag::Regex | Tag::DbPointer | Tag::JsScope | Tag::Binary => {
                return element_value(doc, self);
            }
        })
    }
}

/// A value of a type the register keeps only as its place: decoded from its bytes.
fn element_value<'d>(doc: &'d [u8], v: ValueRef<'d>) -> Option<RawValue<'d>> {
    let ty = match v.tag {
        Tag::Regex => teseladb_bson::types::REGEX,
        Tag::DbPointer => teseladb_bson::types::DB_POINTER,
        Tag::JsScope => teseladb_bson::types::JAVASCRIPT_WITH_SCOPE,
        _ => teseladb_bson::types::BINARY,
    };
    decode_at(doc, v.at(), ty)
}

/// The value of type `ty` whose bytes start at `at`, read by `teseladb_bson` from the document that
/// holds it, found by walking down from the root (only the generic comparisons need it).
fn decode_at(doc: &[u8], at: usize, ty: u8) -> Option<RawValue<'_>> {
    find_value_at(RawDoc::from_stored(doc).ok()?, doc, at, ty)
}

/// The value at byte offset `at` of `root`'s bytes (`base`), searching the documents and arrays
/// that contain it.
fn find_value_at<'d>(d: RawDoc<'d>, base: &'d [u8], at: usize, ty: u8) -> Option<RawValue<'d>> {
    let start = offset_in(base, d.as_bytes())?;
    if at < start || at >= start + d.as_bytes().len() {
        return None;
    }
    let mut pos = start + 4;
    for (_, v) in d.iter() {
        let (vt, vstart, vlen) = element(base, pos)?;
        if vstart == at && vt == ty {
            return Some(v);
        }
        if at >= vstart && at < vstart + vlen {
            return match v {
                RawValue::Document(inner) => find_value_at(inner, base, at, ty),
                RawValue::Array(a) => find_value_at(a.as_doc(), base, at, ty),
                RawValue::JavaScriptWithScope { scope, .. } => find_value_at(scope, base, at, ty),
                _ => None,
            };
        }
        pos = vstart + vlen;
    }
    None
}

/// Where `inner` (a slice of `base`) starts in `base`.
fn offset_in(base: &[u8], inner: &[u8]) -> Option<usize> {
    let b = base.as_ptr() as usize;
    let i = inner.as_ptr() as usize;
    (i >= b && i + inner.len() <= b + base.len()).then(|| i - b)
}

fn le_u64(doc: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(doc.get(at..at + 8)?.try_into().ok()?))
}

/// The end of the C string that starts at `at` (its zero byte).
#[inline(always)]
fn cstring_end(doc: &[u8], at: usize) -> Option<usize> {
    doc.get(at..)?.iter().position(|&b| b == 0).map(|n| at + n)
}

/// The element at `pos`: its type, where its value starts and how long the value is.
#[inline(always)]
pub(crate) fn element(doc: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
    teseladb_keys::stored::element(doc, pos).map(|e| (e.ty, e.value, e.len))
}

/// The register of the element at `pos` whose name ends at its value `v`.
#[inline(always)]
fn value_ref<'d>(doc: &'d [u8], ty: u8, v: usize, len: usize) -> Option<ValueRef<'d>> {
    let place = |tag| Some(ValueRef::new(tag, u32::try_from(len).ok()?, v as u64));
    let text = |tag| {
        // A string's bytes without its length and its final zero.
        Some(ValueRef::new(tag, u32::try_from(len.checked_sub(5)?).ok()?, (v + 4) as u64))
    };
    match ty {
        0x01 => Some(ValueRef::new(Tag::F64, 0, le_u64(doc, v)?)),
        0x02 => text(Tag::Str),
        0x03 => place(Tag::Doc),
        0x04 => place(Tag::Array),
        0x05 => place(Tag::Binary),
        0x06 => Some(ValueRef::new(Tag::Undefined, 0, 0)),
        0x07 => {
            let id = doc.get(v..v + 12)?;
            let low = u64::from_le_bytes(id[..8].try_into().ok()?);
            let high = u32::from_le_bytes(id[8..].try_into().ok()?);
            Some(ValueRef::new(Tag::ObjectId, high, low))
        }
        0x08 => Some(ValueRef::boolean(*doc.get(v)? != 0)),
        0x09 => Some(ValueRef::new(Tag::Date, 0, le_u64(doc, v)?)),
        0x0A => Some(ValueRef::new(Tag::Null, 0, 0)),
        0x0B => place(Tag::Regex),
        0x0C => place(Tag::DbPointer),
        0x0D => text(Tag::Js),
        0x0E => text(Tag::Symbol),
        0x0F => place(Tag::JsScope),
        0x10 => {
            let b: [u8; 4] = doc.get(v..v + 4)?.try_into().ok()?;
            Some(ValueRef::new(Tag::I32, 0, i64::from(i32::from_le_bytes(b)) as u64))
        }
        0x11 => {
            // Stored as the increment, then the seconds.
            let raw = le_u64(doc, v)?;
            Some(ValueRef::new(Tag::Timestamp, 0, raw.rotate_left(32)))
        }
        0x12 => Some(ValueRef::new(Tag::I64, 0, le_u64(doc, v)?)),
        0x13 => place(Tag::Decimal),
        0xFF => Some(ValueRef::new(Tag::MinKey, 0, 0)),
        0x7F => Some(ValueRef::new(Tag::MaxKey, 0, 0)),
        _ => None,
    }
}

/// The first field named `name` of the document or array at `d` (a register of tag `Doc` or
/// `Array`), walked on the bytes.
#[inline(always)]
pub(crate) fn field<'d>(doc: &'d [u8], d: ValueRef<'d>, name: &[u8]) -> ValueRef<'d> {
    find(doc, d.at(), d.len as usize, name).unwrap_or(ValueRef::MISSING)
}

fn find<'d>(doc: &'d [u8], at: usize, len: usize, name: &[u8]) -> Option<ValueRef<'d>> {
    let end = at + len - 1;
    let mut pos = at + 4;
    while pos < end {
        let ty = *doc.get(pos)?;
        let name_end = cstring_end(doc, pos + 1)?;
        let (_, v, vlen) = element(doc, pos)?;
        if doc.get(pos + 1..name_end)? == name {
            return value_ref(doc, ty, v, vlen);
        }
        pos = v + vlen;
    }
    None
}

/// The first field named `name` of document `d`, through the stored document's table of
/// offsets when `d` has one (003 FR-021), else walked on the bytes as [`field`] walks it.
#[inline(always)]
pub(crate) fn stored_field<'d>(
    stored: &Stored<'d>,
    d: ValueRef<'d>,
    name: &[u8],
    key: FieldKey,
) -> ValueRef<'d> {
    let doc = stored.bson();
    match stored.lookup_key(d.at(), name, key) {
        Lookup::NoTable => field(doc, d, name),
        Lookup::Missing => ValueRef::MISSING,
        Lookup::At(pos) => element(doc, pos)
            .and_then(|(ty, v, len)| value_ref(doc, ty, v, len))
            .unwrap_or(ValueRef::MISSING),
    }
}

/// The register of the whole document.
pub(crate) fn root(doc: &[u8]) -> ValueRef<'_> {
    ValueRef::new(Tag::Doc, u32::try_from(doc.len()).unwrap_or(0), 0)
}

/// An iterator over array `a`: the first element's offset and the array's last byte.
#[inline(always)]
pub(crate) fn iter(a: ValueRef<'_>) -> ValueRef<'_> {
    ValueRef::new(Tag::Iter, (a.at() + a.len as usize - 1) as u32, (a.at() + 4) as u64)
}

/// The next element of iterator `it`, advancing it; `None` at the end.
#[inline(always)]
pub(crate) fn next<'d>(doc: &'d [u8], it: &mut ValueRef<'d>) -> Option<ValueRef<'d>> {
    let pos = it.at();
    if pos >= it.len as usize {
        return None;
    }
    let (ty, v, vlen) = element(doc, pos)?;
    it.payload = (v + vlen) as u64;
    value_ref(doc, ty, v, vlen)
}

/// The elements of array `a`.
pub(crate) fn elements<'d>(doc: &'d [u8], a: ValueRef<'d>) -> impl Iterator<Item = ValueRef<'d>> {
    let mut it = iter(a);
    std::iter::from_fn(move || next(doc, &mut it))
}

/// The BSON element type of a register's value.
pub(crate) fn element_type(tag: Tag) -> u8 {
    use teseladb_bson::types as t;
    match tag {
        Tag::F64 => t::DOUBLE,
        Tag::Str => t::STRING,
        Tag::Doc => t::DOCUMENT,
        Tag::Array => t::ARRAY,
        Tag::Binary => t::BINARY,
        Tag::Undefined => t::UNDEFINED,
        Tag::ObjectId => t::OBJECT_ID,
        Tag::Bool => t::BOOLEAN,
        Tag::Date => t::DATE_TIME,
        Tag::Null => t::NULL,
        Tag::Regex => t::REGEX,
        Tag::DbPointer => t::DB_POINTER,
        Tag::Js => t::JAVASCRIPT,
        Tag::Symbol => t::SYMBOL,
        Tag::JsScope => t::JAVASCRIPT_WITH_SCOPE,
        Tag::I32 => t::INT32,
        Tag::Timestamp => t::TIMESTAMP,
        Tag::I64 => t::INT64,
        Tag::Decimal => t::DECIMAL128,
        Tag::MinKey => t::MIN_KEY,
        Tag::MaxKey => t::MAX_KEY,
        Tag::Missing | Tag::ThroughArray | Tag::Iter => 0,
    }
}
