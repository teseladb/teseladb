//! The stored value of a document (003 FR-021, T035): its BSON comes back byte for byte with every
//! type and nesting, a value written before the table existed reads the same, a lookup through the
//! table finds the same field a walk of the BSON finds, and a damaged table is refused.

mod common;

use common::{pool, raw};
use teseladb_keys::stored::{self, Lookup, MIN_TABLE_FIELDS};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

/// A document of `n` fields whose values come from the pool of every type, with an embedded
/// document of `n` fields and an array of documents.
fn wide(seed: u64, n: usize) -> Vec<u8> {
    let values: Vec<Value> = pool(seed, 2 * n)
        .iter()
        .map(|d| Document::from(RawDoc::from_bytes(d).unwrap()).get("v").cloned().unwrap())
        .collect();
    let mut inner = Document::new();
    for (i, v) in values[n..].iter().enumerate() {
        inner.push(format!("in{i}"), v.clone());
    }
    let mut doc = Document::new().with("_id", 1);
    for (i, v) in values[..n].iter().enumerate() {
        doc.push(format!("f{i}"), v.clone());
    }
    doc.push("nested", inner.clone());
    doc.push("list", vec![Value::from(inner.clone()), Value::from(1)]);
    doc.push("f0", "a second field named f0");
    doc.push("deep", Document::new().with("down", inner));
    doc.to_bytes().unwrap()
}

/// Every name of the document at `d` and of the documents embedded in it, with the offset of the
/// document that holds it in `base`.
fn names<'a>(base: &'a [u8], d: RawDoc<'a>, out: &mut Vec<(usize, String)>) {
    let at = d.as_bytes().as_ptr() as usize - base.as_ptr() as usize;
    for (name, v) in d.iter() {
        out.push((at, name.to_owned()));
        if let RawValue::Document(inner) = v {
            names(base, inner, out);
        }
    }
    out.push((at, "absent".to_owned()));
}

/// The first field named `name` of `d`, walked.
fn walked<'a>(d: RawDoc<'a>, name: &str) -> Option<RawValue<'a>> {
    d.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// The document of `base` that starts at `at`.
fn doc_at(base: &[u8], at: usize) -> RawDoc<'_> {
    let len = i32::from_le_bytes(base[at..at + 4].try_into().unwrap()) as usize;
    RawDoc::from_stored(&base[at..at + len]).unwrap()
}

#[test]
fn the_bson_comes_back_byte_for_byte_with_every_type_and_nesting() {
    for seed in 0..40 {
        let bson = wide(seed, MIN_TABLE_FIELDS + (seed as usize % 7));
        let kept = stored::encode(bson.clone());
        assert!(kept.len() > bson.len(), "a wide document gets its table");
        let s = stored::split(&kept).unwrap();
        assert_eq!(s.bson(), &bson[..]);
        assert!(s.table().is_some());
        assert_eq!(stored::bson(&kept).unwrap(), &bson[..]);
        assert_eq!(stored::into_bson(kept.clone()).unwrap(), bson);
        assert_eq!(stored::raw(&kept).unwrap().as_bytes(), &bson[..]);
    }
}

#[test]
fn a_value_written_without_a_table_reads_the_same() {
    let bson = wide(7, MIN_TABLE_FIELDS);
    let s = stored::split(&bson).unwrap();
    assert_eq!(s.bson(), &bson[..]);
    assert!(s.table().is_none());
    assert_eq!(s.lookup(0, b"f1"), Lookup::NoTable);
    assert_eq!(stored::into_bson(bson.clone()).unwrap(), bson);
}

#[test]
fn a_narrow_document_is_stored_as_its_bson_alone() {
    let mut d = Document::new();
    for i in 0..MIN_TABLE_FIELDS - 1 {
        d.push(format!("f{i}"), i as i32);
    }
    let bson = d.to_bytes().unwrap();
    assert_eq!(stored::encode(bson.clone()), bson);
}

#[test]
fn a_lookup_through_the_table_finds_what_the_walk_finds() {
    for seed in 0..40 {
        let bson = wide(seed, MIN_TABLE_FIELDS + (seed as usize % 5));
        let kept = stored::encode(bson.clone());
        let s = stored::split(&kept).unwrap();
        let mut all = Vec::new();
        names(s.bson(), s.raw(), &mut all);
        let mut through_table = 0;
        for (at, name) in all {
            let want = walk_pos(s.bson(), at, &name);
            assert_eq!(want.is_some(), walked(doc_at(s.bson(), at), &name).is_some());
            match s.lookup(at, name.as_bytes()) {
                Lookup::NoTable => {}
                Lookup::Missing => assert_eq!(want, None, "{name} at {at}"),
                Lookup::At(pos) => {
                    through_table += 1;
                    assert_eq!(Some(pos), want, "{name} at {at}");
                }
            }
        }
        assert!(through_table > 2 * MIN_TABLE_FIELDS);
    }
}

/// Where the first field named `name` of the document at `at` starts, walked element by element.
fn walk_pos(bson: &[u8], at: usize, name: &str) -> Option<usize> {
    let len = i32::from_le_bytes(bson[at..at + 4].try_into().unwrap()) as usize;
    let mut p = at + 4;
    while p < at + len - 1 {
        let e = stored::element(bson, p).unwrap();
        if &bson[p + 1..e.name_end] == name.as_bytes() {
            return Some(p);
        }
        p = e.end();
    }
    None
}

#[test]
fn elements_walk_a_document_as_its_view_does() {
    for doc in pool(11, 500) {
        let view = RawDoc::from_bytes(&doc).unwrap();
        let mut pos = 4;
        for (name, _) in view.iter() {
            let e = stored::element(&doc, pos).unwrap();
            assert_eq!(&doc[pos + 1..e.name_end], name.as_bytes());
            pos = e.end();
        }
        assert_eq!(pos, doc.len() - 1);
        assert_eq!(raw(&doc).element_type(), doc[4]);
    }
}

#[test]
fn a_damaged_table_is_refused() {
    let kept = stored::encode(wide(3, MIN_TABLE_FIELDS));
    let n = stored::bson(&kept).unwrap().len();
    let last = kept.len() - 1;
    let mut cases: Vec<Vec<u8>> =
        vec![kept[..n + 3].to_vec(), kept[..last].to_vec(), [&kept[..n], &kept[n + 1..]].concat()];
    let mut version = kept.clone();
    version[last - 1] = 9;
    cases.push(version);
    let mut tables = kept.clone();
    tables[last - 3] = 0xFF;
    cases.push(tables);
    let mut past = kept.clone();
    past[n + 3..n + 7].copy_from_slice(&u32::MAX.to_le_bytes());
    cases.push(past);
    for (i, bad) in cases.iter().enumerate() {
        assert!(stored::split(bad).is_err(), "case {i}");
    }
    assert!(stored::split(&kept[..n - 1]).is_err(), "a cut BSON");
}
