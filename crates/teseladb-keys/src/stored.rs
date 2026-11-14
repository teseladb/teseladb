//! The value a document is stored as (003 FR-021): its BSON exactly as it came in, followed by a
//! table of the offsets of its fields, so that reading `a.b.c` jumps to the value instead of
//! walking the fields before it. Every reader of a stored document goes through [`split`] (or
//! [`bson`] and [`into_bson`] when it only wants the BSON); a value written before the table
//! existed is its BSON alone and reads the same way.
//!
//! The layout after the BSON, whose length is the one its first four bytes declare:
//!
//! ```text
//! entries    7 bytes each: name tag (u16), element type (u8), element offset (u32)
//! directory  10 bytes per table: document offset (u32), first entry (u32), entries (u16)
//! dictionary version 2 only: the identifier of the collection's dictionary (u32)
//! trailer    tables (u16), version (u8), mark (u8)
//! ```
//!
//! Integers are little-endian. A table lists the fields of one document in their order; the
//! document offset is where that document starts in the BSON (0 for the whole document) and the
//! directory goes in increasing document offset. An element offset is where the element starts
//! (its type byte) in the BSON.
//!
//! In version 1 the name tag is a 16-bit hash of the field's name: a lookup compares tags and then
//! the name itself, so two names with the same tag never mix. In version 2 (FR-022) it is the
//! name's number in the collection's dictionary ([`crate::fields`]), or [`UNNUMBERED`] for a name
//! that had none when the document was written. A reader that has the name's number in the
//! table's dictionary compares tags, and the names of the unnumbered fields; any other reader
//! compares the name of each field. A number names one field for as long as its dictionary
//! exists, so a tag equal to the number is that field.

use crate::fields::{FieldNumbers, UNNUMBERED};
use teseladb_bson::{BsonError, RawDoc};

/// The version of the layout whose tags hash the names.
pub const TABLE_VERSION: u8 = 1;
/// The version of the layout whose tags are the names' numbers in the collection's dictionary.
pub const TABLE_VERSION_NUMBERED: u8 = 2;
/// The last byte of a stored value that has a table.
const TABLE_MARK: u8 = 0xD7;
/// A document (the whole one or an embedded one) gets a table from this many fields: with fewer,
/// walking its names costs less than reading the table.
pub const MIN_TABLE_FIELDS: usize = 8;
const ENTRY_LEN: usize = 7;
const DIRECTORY_LEN: usize = 10;
const TRAILER_LEN: usize = 4;
const DICTIONARY_LEN: usize = 4;
const DOCUMENT: u8 = 0x03;

/// A stored document: its BSON and, when it was stored with one, its table.
#[derive(Clone, Copy, Debug)]
pub struct Stored<'a> {
    doc: RawDoc<'a>,
    table: Option<Table<'a>>,
}

/// The table of a stored document (FR-021).
#[derive(Clone, Copy, Debug)]
pub struct Table<'a> {
    entries: &'a [u8],
    directory: &'a [u8],
    /// The identifier of the dictionary of its tags; `None` in version 1, whose tags hash names.
    dictionary: Option<u32>,
}

/// A field name as a lookup through a table takes it: its hash, for the tables of version 1, and
/// its number in a dictionary, for those of version 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldKey {
    tag: u16,
    number: u16,
    dictionary: u32,
}

impl FieldKey {
    /// The key of `name` without a number.
    #[must_use]
    pub fn of(name: &[u8]) -> FieldKey {
        FieldKey { tag: name_tag(name), number: UNNUMBERED, dictionary: 0 }
    }

    /// The key of `name` with its number in `numbers`, if it has one.
    #[must_use]
    pub fn in_dictionary(name: &[u8], numbers: &FieldNumbers) -> FieldKey {
        let number = numbers.get(name).unwrap_or(UNNUMBERED);
        FieldKey { tag: name_tag(name), number, dictionary: numbers.dictionary() }
    }

    /// Its number in a dictionary, if it has one.
    #[must_use]
    pub fn number(&self) -> Option<(u32, u16)> {
        (self.number != UNNUMBERED).then_some((self.dictionary, self.number))
    }
}

/// What a table says about a field of one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The document has no table: the caller walks its fields.
    NoTable,
    /// The document has no field of that name.
    Missing,
    /// The first field of that name starts at this offset of the BSON.
    At(usize),
}

/// One element of a BSON document, located by [`element`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Element {
    /// Its type byte.
    pub ty: u8,
    /// Where its name ends (the name's zero byte).
    pub name_end: usize,
    /// Where its value starts.
    pub value: usize,
    /// How long its value is.
    pub len: usize,
}

impl Element {
    /// Where the element after it starts.
    #[must_use]
    pub fn end(&self) -> usize {
        self.value + self.len
    }
}

fn broken(offset: usize, reason: &'static str) -> BsonError {
    BsonError::InvalidBson { offset, reason }
}

fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<usize> {
    let v = u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?);
    usize::try_from(v).ok()
}

fn le_i32(b: &[u8], at: usize) -> Option<usize> {
    let v = i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?);
    usize::try_from(v).ok()
}

/// The length the BSON at the start of `stored` declares.
fn declared(stored: &[u8]) -> Result<usize, BsonError> {
    match le_i32(stored, 0) {
        Some(n) if n >= 5 && n <= stored.len() && stored[n - 1] == 0 => Ok(n),
        _ => Err(broken(0, "stored document with a broken frame")),
    }
}

/// Splits a stored value into its BSON and its table, checking the frame of both: the BSON's
/// declared length and final zero, and the table's trailer, directory and entries. The BSON's
/// elements are not walked again (it was validated when it came in).
///
/// # Errors
///
/// [`BsonError::InvalidBson`] when the frame of the BSON or of the table does not hold.
pub fn split(stored: &[u8]) -> Result<Stored<'_>, BsonError> {
    // A document's local identifier (009 FR-023) ends the value after its table.
    let stored = crate::local_id::document_body(stored);
    let n = declared(stored)?;
    let doc = RawDoc::from_stored(&stored[..n])?;
    if n == stored.len() {
        return Ok(Stored { doc, table: None });
    }
    let rest = &stored[n..];
    let bad = |reason| broken(n, reason);
    let t = rest.len().checked_sub(TRAILER_LEN).ok_or_else(|| bad("table without its trailer"))?;
    let known = matches!(rest[t + 2], TABLE_VERSION | TABLE_VERSION_NUMBERED);
    if !known || rest[t + 3] != TABLE_MARK {
        return Err(bad("table of an unknown version"));
    }
    let (t, dictionary) = match rest[t + 2] {
        TABLE_VERSION => (t, None),
        _ => {
            let d =
                t.checked_sub(DICTIONARY_LEN).ok_or_else(|| bad("table without its trailer"))?;
            let id = u32::from_le_bytes(rest[d..t].try_into().unwrap_or_default());
            (d, Some(id))
        }
    };
    let tables = usize::from(le_u16(rest, rest.len() - TRAILER_LEN).unwrap_or(0));
    let dir_at = t
        .checked_sub(tables * DIRECTORY_LEN)
        .ok_or_else(|| bad("table directory past its start"))?;
    if tables == 0 || dir_at % ENTRY_LEN != 0 {
        return Err(bad("table entries of a broken length"));
    }
    let table = Table { entries: &rest[..dir_at], directory: &rest[dir_at..t], dictionary };
    table.check(n)?;
    Ok(Stored { doc, table: Some(table) })
}

/// The BSON of a stored value.
///
/// # Errors
///
/// As [`split`].
pub fn bson(stored: &[u8]) -> Result<&[u8], BsonError> {
    split(stored).map(|s| s.bson())
}

/// The BSON of a stored value, keeping its buffer: the table is cut off the end.
///
/// # Errors
///
/// As [`split`].
pub fn into_bson(mut stored: Vec<u8>) -> Result<Vec<u8>, BsonError> {
    let n = bson(&stored)?.len();
    stored.truncate(n);
    Ok(stored)
}

/// The BSON of a stored value that goes on to a reader that parses it: the table is cut off the end
/// when the frame holds, and a value whose frame does not hold goes on as it is, for that reader
/// to refuse.
#[must_use]
pub fn strip(stored: Vec<u8>) -> Vec<u8> {
    match bson(&stored).map(<[u8]>::len) {
        Ok(n) => {
            let mut stored = stored;
            stored.truncate(n);
            stored
        }
        Err(_) => stored,
    }
}

/// The value of `key` as a reader of documents takes it: [`strip`] for a key in the documents of a
/// collection ([`crate::space::in_documents`]), any other value as it is.
#[must_use]
pub fn document_value(key: &[u8], value: Vec<u8>) -> Vec<u8> {
    if crate::space::in_documents(key) { strip(value) } else { value }
}

/// [`document_value`] on each row of a scan.
#[must_use]
pub fn document_rows(rows: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    rows.into_iter()
        .map(|(key, value)| {
            let value = document_value(&key, value);
            (key, value)
        })
        .collect()
}

/// The view of a stored value's BSON.
///
/// # Errors
///
/// As [`split`].
pub fn raw(stored: &[u8]) -> Result<RawDoc<'_>, BsonError> {
    split(stored).map(|s| s.raw())
}

impl<'a> Stored<'a> {
    /// A document without a table, as a value written before tables existed is stored.
    #[must_use]
    pub fn plain(doc: RawDoc<'a>) -> Stored<'a> {
        Stored { doc, table: None }
    }

    /// The BSON, exactly as it came in.
    #[must_use]
    pub fn bson(&self) -> &'a [u8] {
        self.doc.as_bytes()
    }

    /// The view of the BSON.
    #[must_use]
    pub fn raw(&self) -> RawDoc<'a> {
        self.doc
    }

    /// The table, if it was stored with one.
    #[must_use]
    pub fn table(&self) -> Option<Table<'a>> {
        self.table
    }

    /// The first field named `name` of the document that starts at offset `doc_at` of the BSON,
    /// through the table.
    #[must_use]
    pub fn lookup(&self, doc_at: usize, name: &[u8]) -> Lookup {
        self.lookup_key(doc_at, name, FieldKey::of(name))
    }

    /// [`Stored::lookup`] with the name's key, worked out once for many lookups.
    #[inline]
    #[must_use]
    pub fn lookup_key(&self, doc_at: usize, name: &[u8], key: FieldKey) -> Lookup {
        self.table.map_or(Lookup::NoTable, |t| t.lookup_key(self.bson(), doc_at, name, key))
    }
}

impl<'a> From<RawDoc<'a>> for Stored<'a> {
    fn from(doc: RawDoc<'a>) -> Stored<'a> {
        Stored::plain(doc)
    }
}

impl<'a> Table<'a> {
    fn tables(&self) -> usize {
        self.directory.len() / DIRECTORY_LEN
    }

    /// Directory row `i`: the document's offset, its first entry and how many it has.
    fn row(&self, i: usize) -> (usize, usize, usize) {
        let at = i * DIRECTORY_LEN;
        let d = self.directory;
        (
            le_u32(d, at).unwrap_or(usize::MAX),
            le_u32(d, at + 4).unwrap_or(usize::MAX),
            usize::from(le_u16(d, at + 8).unwrap_or(0)),
        )
    }

    /// Entry `i`: its name tag, element type and element offset.
    fn entry(&self, i: usize) -> (u16, u8, usize) {
        let at = i * ENTRY_LEN;
        let e = self.entries;
        (le_u16(e, at).unwrap_or(0), e[at + 2], le_u32(e, at + 3).unwrap_or(usize::MAX))
    }

    /// Every directory row and entry lies inside the table and the BSON of length `n`, and the
    /// rows go in increasing document offset.
    fn check(&self, n: usize) -> Result<(), BsonError> {
        let entries = self.entries.len() / ENTRY_LEN;
        let mut last = None;
        for i in 0..self.tables() {
            let (doc_at, first, count) = self.row(i);
            let ordered = last.is_none_or(|l| doc_at > l);
            if !ordered || doc_at >= n || first.checked_add(count).is_none_or(|e| e > entries) {
                return Err(broken(n, "table directory out of its bounds"));
            }
            last = Some(doc_at);
        }
        if (0..entries).any(|i| self.entry(i).2 >= n) {
            return Err(broken(n, "table entry past its document"));
        }
        Ok(())
    }

    /// The directory row of the document at `doc_at`.
    fn find_row(&self, doc_at: usize) -> Option<(usize, usize)> {
        let (mut lo, mut hi) = (0, self.tables());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (at, first, count) = self.row(mid);
            match at.cmp(&doc_at) {
                std::cmp::Ordering::Equal => return Some((first, count)),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// The first field named `name` of the document at `doc_at` of `bson`.
    #[must_use]
    pub fn lookup(&self, bson: &[u8], doc_at: usize, name: &[u8]) -> Lookup {
        self.lookup_key(bson, doc_at, name, FieldKey::of(name))
    }

    /// [`Table::lookup`] with the name's key: by its hash in version 1; in version 2 by its number
    /// when the key has one in the table's dictionary, else by the name of each field.
    #[must_use]
    pub fn lookup_key(&self, bson: &[u8], doc_at: usize, name: &[u8], key: FieldKey) -> Lookup {
        let Some((first, count)) = self.find_row(doc_at) else { return Lookup::NoTable };
        let entries = first..first + count;
        let found = match (self.dictionary, key.number()) {
            (None, _) => entries
                .map(|i| self.entry(i))
                .find(|&(t, ty, at)| t == key.tag && names_at(bson, at, ty, name)),
            (Some(d), Some((kd, number))) if d == kd => {
                entries.map(|i| self.entry(i)).find(|&(t, ty, at)| {
                    (t == number && bson.get(at) == Some(&ty))
                        || (t == UNNUMBERED && names_at(bson, at, ty, name))
                })
            }
            (Some(_), _) => {
                entries.map(|i| self.entry(i)).find(|&(_, ty, at)| names_at(bson, at, ty, name))
            }
        };
        found.map_or(Lookup::Missing, |(_, _, at)| Lookup::At(at))
    }

    /// The identifier of the dictionary of its tags (version 2).
    #[must_use]
    pub fn dictionary(&self) -> Option<u32> {
        self.dictionary
    }

    /// The numbered names of its fields in `bson`, with their numbers: what a table of version 2
    /// says of the dictionary it was written with.
    fn numbers_into(&self, bson: &[u8], out: &mut FieldNumbers) {
        for i in 0..self.entries.len() / ENTRY_LEN {
            let (t, _, at) = self.entry(i);
            if t == UNNUMBERED {
                continue;
            }
            if let Some(name) = cstring_end(bson, at + 1).and_then(|e| bson.get(at + 1..e)) {
                out.insert(name, t);
            }
        }
    }
}

/// Whether the element at `at` of `bson` has type `ty` and is named `name`.
fn names_at(bson: &[u8], at: usize, ty: u8, name: &[u8]) -> bool {
    let start = at + 1;
    bson.get(at) == Some(&ty)
        && bson.get(start..start + name.len()) == Some(name)
        && bson.get(start + name.len()) == Some(&0)
}

/// The 16-bit tag of a field name: FNV-1a folded to 16 bits.
#[must_use]
pub fn name_tag(name: &[u8]) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in name {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    ((h >> 16) ^ (h & 0xFFFF)) as u16
}

/// The end of the zero-terminated name that starts at `at`.
fn cstring_end(doc: &[u8], at: usize) -> Option<usize> {
    doc.get(at..)?.iter().position(|&b| b == 0).map(|n| at + n)
}

/// The element that starts at `pos` of BSON `doc`: its type, where its name ends and where and
/// how long its value is; `None` when the bytes there are not an element.
#[inline]
#[must_use]
pub fn element(doc: &[u8], pos: usize) -> Option<Element> {
    let ty = *doc.get(pos)?;
    let name_end = cstring_end(doc, pos + 1)?;
    let v = name_end + 1;
    let len = match ty {
        0x01 | 0x09 | 0x11 | 0x12 => 8,
        0x02 | 0x0D | 0x0E => 4 + le_i32(doc, v)?,
        0x03 | 0x04 | 0x0F => le_i32(doc, v)?,
        0x05 => 5 + le_i32(doc, v)?,
        0x06 | 0x0A | 0xFF | 0x7F => 0,
        0x07 => 12,
        0x08 => 1,
        0x0B => {
            let p = cstring_end(doc, v)?;
            cstring_end(doc, p + 1)? + 1 - v
        }
        0x0C => 4 + le_i32(doc, v)? + 12,
        0x10 => 4,
        0x13 => 16,
        _ => return None,
    };
    Some(Element { ty, name_end, value: v, len })
}

/// The value to store for a validated document: its BSON, followed by its table when it or one
/// of the documents embedded in it through documents has [`MIN_TABLE_FIELDS`] fields or more;
/// otherwise its BSON alone.
#[must_use]
pub fn encode(bson: Vec<u8>) -> Vec<u8> {
    build(bson, &|name| name_tag(name), None)
}

/// [`encode`] with the tags of version 2 (FR-022): the numbers of `numbers`, and those the table
/// of `old` (the version the document replaces) gives when it is of the same dictionary. A
/// collection without a dictionary yet (`numbers` empty, `old` without one) gets version 1.
#[must_use]
pub fn encode_numbered(bson: Vec<u8>, numbers: &FieldNumbers, old: Option<&Stored<'_>>) -> Vec<u8> {
    let old = old.and_then(|o| o.table.and_then(|t| t.dictionary.map(|d| (o.bson(), t, d))));
    let dictionary = match old {
        _ if !numbers.is_empty() => numbers.dictionary(),
        Some((_, _, d)) => d,
        None => return encode(bson),
    };
    let mut merged = std::borrow::Cow::Borrowed(numbers);
    if let Some((old_bson, t, d)) = old
        && d == dictionary
    {
        let m = merged.to_mut();
        if m.is_empty() {
            *m = FieldNumbers::new(d);
        }
        t.numbers_into(old_bson, m);
    }
    let tag = |name: &[u8]| merged.get(name).unwrap_or(UNNUMBERED);
    build(bson, &tag, Some(dictionary))
}

/// The numbered names a stored document's tables of version 2 give, under their dictionary;
/// `None` for a document without such a table.
#[must_use]
pub fn numbers_of(stored: &Stored<'_>) -> Option<FieldNumbers> {
    let t = stored.table?;
    let mut out = FieldNumbers::new(t.dictionary?);
    t.numbers_into(stored.bson(), &mut out);
    Some(out)
}

/// The value to store: `bson` and, when a document of it has enough fields, its table with the
/// tags `tag` gives, in version 2 with `dictionary`'s identifier or else in version 1.
fn build(mut bson: Vec<u8>, tag: &dyn Fn(&[u8]) -> u16, dictionary: Option<u32>) -> Vec<u8> {
    let mut entries = Vec::new();
    let mut directory: Vec<(usize, usize, usize)> = Vec::new();
    collect(&bson, 0, (&mut entries, &mut directory), tag);
    let fits = |v: usize| u32::try_from(v).is_ok();
    if directory.is_empty()
        || !fits(bson.len())
        || directory.len() > usize::from(u16::MAX)
        || directory.iter().any(|&(_, _, c)| c > usize::from(u16::MAX))
    {
        return bson;
    }
    directory.sort_unstable_by_key(|&(at, _, _)| at);
    bson.reserve(
        entries.len() * ENTRY_LEN + directory.len() * DIRECTORY_LEN + DICTIONARY_LEN + TRAILER_LEN,
    );
    for (tag, ty, at) in entries {
        bson.extend_from_slice(&tag.to_le_bytes());
        bson.push(ty);
        bson.extend_from_slice(&(at as u32).to_le_bytes());
    }
    for &(at, first, count) in &directory {
        bson.extend_from_slice(&(at as u32).to_le_bytes());
        bson.extend_from_slice(&(first as u32).to_le_bytes());
        bson.extend_from_slice(&(count as u16).to_le_bytes());
    }
    if let Some(d) = dictionary {
        bson.extend_from_slice(&d.to_le_bytes());
    }
    bson.extend_from_slice(&(directory.len() as u16).to_le_bytes());
    bson.push(if dictionary.is_some() { TABLE_VERSION_NUMBERED } else { TABLE_VERSION });
    bson.push(TABLE_MARK);
    bson
}

/// The fields of the document at `doc_at` (and of the documents embedded in it), into `entries`
/// and `directory` when they have enough fields. Embedded documents are reached through
/// documents only: an element of an array is reached through its array.
type Collected<'c> = (&'c mut Vec<(u16, u8, usize)>, &'c mut Vec<(usize, usize, usize)>);

fn collect(
    bson: &[u8],
    doc_at: usize,
    (entries, directory): Collected<'_>,
    tag: &dyn Fn(&[u8]) -> u16,
) {
    let Some(len) = le_i32(bson, doc_at) else { return };
    let end = doc_at + len - 1;
    let mut fields = Vec::new();
    let mut pos = doc_at + 4;
    while pos < end {
        let Some(e) = element(bson, pos) else { return };
        fields.push((tag(&bson[pos + 1..e.name_end]), e.ty, pos, e.value));
        pos = e.end();
    }
    if fields.len() >= MIN_TABLE_FIELDS {
        directory.push((doc_at, entries.len(), fields.len()));
        entries.extend(fields.iter().map(|&(tag, ty, at, _)| (tag, ty, at)));
    }
    for &(_, ty, _, value) in &fields {
        if ty == DOCUMENT {
            collect(bson, value, (&mut *entries, &mut *directory), tag);
        }
    }
}
