//! Column blocks (010 FR-033): the copy, field by field, of the top-level fields of the documents
//! of one data block, shared by the LSM that writes and reads them and the executor that runs an
//! aggregation over them.
//!
//! ```text
//! columns := docs(varint) · flags(u8) · fields(varint) · field*
//! field   := name(bytes) · 0                                  no column
//!          | name(bytes) · 1 · present(⌈docs/8⌉ bytes) · values(BSON document)
//! ```
//!
//! Flag bit 0: the block counted every top-level name of its documents (at most [`MAX_NAMES`]),
//! so a field it does not name is missing from all of them. A field has no column when one of
//! its values is a document, an array or code with scope, or takes more than [`MAX_CELL`] bytes.
//! Bit `j % 8` of byte `j / 8` of `present` says that document `j` of the block, in key order,
//! holds the field; `values` holds the values present, in that order, each with an empty name.

use teseladb_bson::{RawDoc, RawValue, types};

/// Top-level names a block keeps columns for: those its documents bring first.
pub const MAX_NAMES: usize = 64;
/// Longest value, in the bytes BSON writes for it, a column keeps.
pub const MAX_CELL: usize = 64;

const ALL_NAMES: u8 = 1;
const WITHOUT: u8 = 0;
const WITH: u8 = 1;

/// Bytes that do not read as a column block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadColumns;

/// One field while the block is written.
#[derive(Debug)]
struct Building {
    usable: bool,
    present: Vec<u8>,
    /// The elements of the values present, each with an empty name.
    values: Vec<u8>,
}

/// Builds the column block of one data block as its documents go in, in key order.
#[derive(Debug)]
pub struct ColumnsBuilder {
    docs: usize,
    names: Vec<String>,
    fields: Vec<Building>,
    all_names: bool,
    spoiled: bool,
    /// The fields the document going in has shown so far.
    seen: Vec<bool>,
}

impl Default for ColumnsBuilder {
    fn default() -> ColumnsBuilder {
        ColumnsBuilder {
            docs: 0,
            names: Vec::new(),
            fields: Vec::new(),
            all_names: true,
            spoiled: false,
            seen: Vec::new(),
        }
    }
}

/// Whether a value of `kind` with `bytes` fits in a column.
fn fits(value: &RawValue<'_>, bytes: &[u8]) -> bool {
    !matches!(
        value,
        RawValue::Document(_) | RawValue::Array(_) | RawValue::JavaScriptWithScope { .. }
    ) && bytes.len() <= MAX_CELL
}

impl ColumnsBuilder {
    /// Adds the next document of the block. A document with two fields of the same name leaves
    /// the block without columns.
    pub fn add(&mut self, doc: RawDoc<'_>) {
        if self.spoiled {
            return;
        }
        let row = self.docs;
        self.docs += 1;
        self.seen.iter_mut().for_each(|s| *s = false);
        for (name, value, bytes) in doc.elements() {
            let at = match self.names.iter().position(|n| n == name) {
                Some(at) => at,
                None if self.names.len() < MAX_NAMES => {
                    self.names.push(name.to_owned());
                    self.fields.push(Building {
                        usable: true,
                        present: Vec::new(),
                        values: Vec::new(),
                    });
                    self.seen.push(false);
                    self.names.len() - 1
                }
                None => {
                    self.all_names = false;
                    continue;
                }
            };
            if std::mem::replace(&mut self.seen[at], true) {
                self.spoiled = true;
                return;
            }
            let f = &mut self.fields[at];
            if !f.usable {
                continue;
            }
            if !fits(&value, bytes) {
                *f = Building { usable: false, present: Vec::new(), values: Vec::new() };
                continue;
            }
            if f.present.len() <= row / 8 {
                f.present.resize(row / 8 + 1, 0);
            }
            f.present[row / 8] |= 1 << (row % 8);
            f.values.push(value.element_type());
            f.values.push(0);
            f.values.extend_from_slice(bytes);
        }
    }

    /// The block cannot have columns (an entry that is not a document of one version).
    pub fn spoil(&mut self) {
        self.spoiled = true;
    }

    /// Whether the block may still have columns.
    #[must_use]
    pub fn is_spoiled(&self) -> bool {
        self.spoiled
    }

    /// The block's payload; `None` when it has no columns.
    #[must_use]
    pub fn finish(self) -> Option<Vec<u8>> {
        if self.spoiled || self.docs == 0 {
            return None;
        }
        let mut out = Vec::new();
        put_varint(&mut out, self.docs as u64);
        out.push(if self.all_names { ALL_NAMES } else { 0 });
        put_varint(&mut out, self.names.len() as u64);
        let bitmap = self.docs.div_ceil(8);
        for (name, mut f) in self.names.iter().zip(self.fields) {
            put_varint(&mut out, name.len() as u64);
            out.extend_from_slice(name.as_bytes());
            if !f.usable {
                out.push(WITHOUT);
                continue;
            }
            out.push(WITH);
            f.present.resize(bitmap, 0);
            out.extend_from_slice(&f.present);
            let len = 4 + f.values.len() + 1;
            out.extend_from_slice(&(len as u32).to_le_bytes());
            out.extend_from_slice(&f.values);
            out.push(0);
        }
        Some(out)
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(b: &[u8], at: &mut usize) -> Result<u64, BadColumns> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at).ok_or(BadColumns)?;
        *at += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(v);
        }
    }
    Err(BadColumns)
}

fn take<'a>(b: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], BadColumns> {
    let end = at.checked_add(n).ok_or(BadColumns)?;
    let s = b.get(*at..end).ok_or(BadColumns)?;
    *at = end;
    Ok(s)
}

/// What a block keeps of one field.
#[derive(Clone, Copy, Debug)]
struct FieldView<'a> {
    name: &'a str,
    column: Option<(&'a [u8], RawDoc<'a>)>,
}

/// A column block, read.
#[derive(Clone, Debug)]
pub struct ColumnsView<'a> {
    docs: usize,
    all_names: bool,
    fields: Vec<FieldView<'a>>,
}

/// The values of one field in the documents of a block, in their order: `None` where a document
/// does not hold it.
#[derive(Clone, Debug)]
pub struct Cells<'a> {
    present: Option<&'a [u8]>,
    values: Option<teseladb_bson::RawIter<'a>>,
    row: usize,
    docs: usize,
}

impl<'a> Iterator for Cells<'a> {
    type Item = Option<RawValue<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.row == self.docs {
            return None;
        }
        let row = self.row;
        self.row += 1;
        let holds = self.present.is_some_and(|p| p[row / 8] & (1 << (row % 8)) != 0);
        if !holds {
            return Some(None);
        }
        let values = self.values.as_mut().expect("a column with values present");
        Some(values.next().map(|(_, v)| v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.docs - self.row;
        (left, Some(left))
    }
}

impl ExactSizeIterator for Cells<'_> {}

impl<'a> ColumnsView<'a> {
    /// Reads the payload of a column block.
    ///
    /// # Errors
    ///
    /// [`BadColumns`] for bytes [`ColumnsBuilder::finish`] did not write.
    pub fn parse(b: &'a [u8]) -> Result<ColumnsView<'a>, BadColumns> {
        let mut at = 0;
        let docs = usize::try_from(get_varint(b, &mut at)?).map_err(|_| BadColumns)?;
        let flags = *take(b, &mut at, 1)?.first().ok_or(BadColumns)?;
        let n = usize::try_from(get_varint(b, &mut at)?).map_err(|_| BadColumns)?;
        if docs == 0 || flags & !ALL_NAMES != 0 || n > MAX_NAMES {
            return Err(BadColumns);
        }
        let bitmap = docs.div_ceil(8);
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            let len = usize::try_from(get_varint(b, &mut at)?).map_err(|_| BadColumns)?;
            let name = std::str::from_utf8(take(b, &mut at, len)?).map_err(|_| BadColumns)?;
            if fields.iter().any(|f: &FieldView<'_>| f.name == name) {
                return Err(BadColumns);
            }
            let column = match *take(b, &mut at, 1)?.first().ok_or(BadColumns)? {
                WITHOUT => None,
                WITH => Some(column(b, &mut at, docs, bitmap)?),
                _ => return Err(BadColumns),
            };
            fields.push(FieldView { name, column });
        }
        if at != b.len() {
            return Err(BadColumns);
        }
        Ok(ColumnsView { docs, all_names: flags & ALL_NAMES != 0, fields })
    }

    /// Documents of the block.
    #[must_use]
    pub fn docs(&self) -> usize {
        self.docs
    }

    /// The values of top-level field `name` in each document; `None` when the block keeps no
    /// column for it (a value that does not fit, or a name past those it counted).
    #[must_use]
    pub fn cells(&self, name: &str) -> Option<Cells<'a>> {
        let missing = Cells { present: None, values: None, row: 0, docs: self.docs };
        match self.fields.iter().find(|f| f.name == name) {
            Some(FieldView { column: Some((present, values)), .. }) => {
                Some(Cells { present: Some(present), values: Some(values.iter()), ..missing })
            }
            Some(FieldView { column: None, .. }) => None,
            None => self.all_names.then_some(missing),
        }
    }

    /// Whether the block gives the values of each of `names`.
    #[must_use]
    pub fn has_all<S: AsRef<str>>(&self, names: &[S]) -> bool {
        names.iter().all(|n| self.cells(n.as_ref()).is_some())
    }
}

/// A column's present bits and values, checked: the values are a BSON document of empty names,
/// one for each bit set, and no bit is set past the last document.
fn column<'a>(
    b: &'a [u8],
    at: &mut usize,
    docs: usize,
    bitmap: usize,
) -> Result<(&'a [u8], RawDoc<'a>), BadColumns> {
    let present = take(b, at, bitmap)?;
    let past = bitmap * 8 - docs;
    if past > 0 && present[bitmap - 1] >> (8 - past) != 0 {
        return Err(BadColumns);
    }
    let len = u32::from_le_bytes(
        b.get(*at..*at + 4).ok_or(BadColumns)?.try_into().map_err(|_| BadColumns)?,
    ) as usize;
    let values = RawDoc::from_bytes(take(b, at, len)?).map_err(|_| BadColumns)?;
    let held: u32 = present.iter().map(|x| x.count_ones()).sum();
    let mut count = 0u32;
    for (name, value) in values.iter() {
        if !name.is_empty() || matches!(value.element_type(), types::DOCUMENT | types::ARRAY) {
            return Err(BadColumns);
        }
        count += 1;
    }
    if count != held {
        return Err(BadColumns);
    }
    Ok((present, values))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::{Document, Value};

    use super::*;

    fn built(docs: &[Document]) -> Option<Vec<u8>> {
        let mut b = ColumnsBuilder::default();
        let bytes: Vec<Vec<u8>> = docs.iter().map(|d| d.to_bytes().unwrap()).collect();
        for d in &bytes {
            b.add(RawDoc::from_bytes(d).unwrap());
        }
        b.finish()
    }

    fn values(view: &ColumnsView<'_>, name: &str) -> Option<Vec<Option<Value>>> {
        view.cells(name).map(|c| c.map(|v| v.map(Value::from)).collect())
    }

    #[test]
    fn a_block_gives_back_each_documents_values_in_order() {
        let docs: Vec<Document> = (0..11)
            .map(|i| {
                let mut d = Document::new().with("n", i);
                if i % 3 == 0 {
                    d.push("f", f64::from(i) / 4.0);
                }
                if i == 4 {
                    d.push("s", "x".repeat(MAX_CELL));
                }
                if i == 5 {
                    d.push("o", Document::new().with("t", 1));
                }
                d
            })
            .collect();
        let payload = built(&docs).unwrap();
        let view = ColumnsView::parse(&payload).unwrap();
        assert_eq!(view.docs(), 11);
        let n: Vec<Option<Value>> = (0..11).map(|i| Some(Value::Int32(i))).collect();
        assert_eq!(values(&view, "n"), Some(n));
        let f: Vec<Option<Value>> =
            (0..11).map(|i| (i % 3 == 0).then(|| Value::Double(f64::from(i) / 4.0))).collect();
        assert_eq!(values(&view, "f"), Some(f));
        assert_eq!(values(&view, "s"), None, "a value too long");
        assert_eq!(values(&view, "o"), None, "a document");
        assert_eq!(values(&view, "absent"), Some(vec![None; 11]), "every name counted");
        assert!(view.has_all(&["n", "f", "absent"]));
        assert!(!view.has_all(&["n", "o"]));
        assert_eq!(view.cells("n").unwrap().len(), 11);
    }

    #[test]
    fn a_block_with_names_past_those_it_counts_knows_nothing_of_the_others() {
        let mut d = Document::new();
        for i in 0..=MAX_NAMES {
            d.push(format!("f{i:02}"), 1);
        }
        let payload = built(&[d]).unwrap();
        let view = ColumnsView::parse(&payload).unwrap();
        assert!(view.cells("f00").is_some());
        assert!(view.cells(&format!("f{MAX_NAMES:02}")).is_none());
        assert!(view.cells("absent").is_none());
    }

    #[test]
    fn repeated_names_spoiled_blocks_and_empty_blocks_have_no_columns() {
        let mut twice = Document::new().with("a", 1);
        twice.push("a", 2);
        assert!(built(&[Document::new().with("a", 0), twice]).is_none());
        assert!(built(&[]).is_none());
        let mut b = ColumnsBuilder::default();
        let one = Document::new().with("a", 1).to_bytes().unwrap();
        b.add(RawDoc::from_bytes(&one).unwrap());
        assert!(!b.is_spoiled());
        b.spoil();
        assert!(b.is_spoiled());
        b.add(RawDoc::from_bytes(&one).unwrap());
        assert!(b.finish().is_none());
    }

    #[test]
    fn damaged_column_blocks_do_not_read() {
        let docs = [Document::new().with("a", 1).with("b", "x"), Document::new().with("a", 2)];
        let good = built(&docs).unwrap();
        assert!(ColumnsView::parse(&good).is_ok());
        for cut in 0..good.len() {
            assert!(ColumnsView::parse(&good[..cut]).is_err(), "cut at {cut}");
        }
        assert!(ColumnsView::parse(&[good.clone(), vec![0]].concat()).is_err());
        let mut flags = good.clone();
        flags[1] = 0x80;
        assert!(ColumnsView::parse(&flags).is_err());
        let mut kind = good.clone();
        // `a`'s column tag, after docs, flags, the count, and the name's length and byte.
        kind[5] = 7;
        assert!(ColumnsView::parse(&kind).is_err());
        let mut bits = good.clone();
        // A bit past the last document of `a`'s present bits.
        bits[6] |= 0x80;
        assert!(ColumnsView::parse(&bits).is_err());
        let mut fewer = good.clone();
        // `b`'s present bits say both documents hold it, its values hold one.
        let at = good.len() - (4 + 2 + 6 + 1) - 1;
        assert_eq!(fewer[at], 1);
        fewer[at] = 3;
        assert!(ColumnsView::parse(&fewer).is_err());
        let mut none = Vec::new();
        put_varint(&mut none, 0);
        none.extend([0, 0]);
        assert!(ColumnsView::parse(&none).is_err(), "no documents");
        let mut many = Vec::new();
        put_varint(&mut many, 1);
        many.push(0);
        put_varint(&mut many, (MAX_NAMES + 1) as u64);
        assert!(ColumnsView::parse(&many).is_err());
        let mut twice = Vec::new();
        put_varint(&mut twice, 1);
        twice.push(0);
        put_varint(&mut twice, 2);
        twice.extend([1, b'a', WITHOUT, 1, b'a', WITHOUT]);
        assert!(ColumnsView::parse(&twice).is_err(), "a name twice");
        let mut long = vec![0xff; 10];
        long.push(1);
        assert!(ColumnsView::parse(&long).is_err(), "a varint past 64 bits");
    }

    #[test]
    fn values_with_names_or_nested_values_do_not_read() {
        let doc = |d: Document| d.to_bytes().unwrap();
        for values in
            [doc(Document::new().with("x", 1)), doc(Document::new().with("", Document::new()))]
        {
            let mut b = Vec::new();
            put_varint(&mut b, 1);
            b.push(ALL_NAMES);
            put_varint(&mut b, 1);
            b.extend([1, b'a', WITH, 1]);
            b.extend(&values);
            assert!(ColumnsView::parse(&b).is_err());
        }
    }
}
