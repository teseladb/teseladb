//! The tables of version 2 (003 FR-022, T036): their tags are the names' numbers in the
//! collection's dictionary. The BSON comes back byte for byte, a lookup by number finds what the
//! walk finds, also for the names that had no number when the document was written and for a
//! reader of another dictionary, a dictionary only grows, and the tables of version 1 and the
//! values without a table read as before.

mod common;

use common::pool;
use teseladb_keys::fields::{FieldNumbers, MAX_FIELD_NAME_LEN, MAX_FIELD_NAMES, UNNUMBERED};
use teseladb_keys::stored::{self, FieldKey, Lookup, MIN_TABLE_FIELDS, Stored};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

const DICTIONARY: u32 = 0x5EED_0001;

/// A document of `n` fields of every type, with an embedded document of `n` fields, a second
/// field named `f0` and a document in an array.
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
    doc.push("list", vec![Value::from(inner.clone())]);
    doc.push("f0", "a second field named f0");
    doc.to_bytes().unwrap()
}

/// Every name of each document of `d` (through documents), with the offset of its document, and
/// one absent name per document.
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

/// Where the first field named `name` of the document at `at` starts, walked.
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

/// A dictionary of the names of `bson` taken one in `every`, in the order of the document.
fn dictionary_of(bson: &[u8], every: usize) -> FieldNumbers {
    let mut all = Vec::new();
    names(bson, RawDoc::from_bytes(bson).unwrap(), &mut all);
    let mut d = FieldNumbers::new(DICTIONARY);
    for (i, (_, name)) in all.iter().enumerate() {
        if i % every == 0 && name != "absent" {
            d.add(name.as_bytes());
        }
    }
    d
}

/// Each name of `s` looked up with the key `numbers` gives finds the walk's field, and the count
/// of lookups the table answered.
fn lookups_agree(s: &Stored<'_>, numbers: &FieldNumbers) -> usize {
    let mut all = Vec::new();
    names(s.bson(), s.raw(), &mut all);
    let mut answered = 0;
    for (at, name) in all {
        let want = walk_pos(s.bson(), at, &name);
        let key = FieldKey::in_dictionary(name.as_bytes(), numbers);
        match s.lookup_key(at, name.as_bytes(), key) {
            Lookup::NoTable => {}
            Lookup::Missing => assert_eq!(want, None, "{name} at {at}"),
            Lookup::At(pos) => {
                answered += 1;
                assert_eq!(Some(pos), want, "{name} at {at}");
            }
        }
        assert_eq!(s.lookup(at, name.as_bytes()), s.lookup_key(at, name.as_bytes(), key));
    }
    answered
}

#[test]
fn a_numbered_table_keeps_the_bson_and_finds_what_the_walk_finds() {
    for seed in 0..40 {
        let bson = wide(seed, MIN_TABLE_FIELDS + (seed as usize % 7));
        // Every name, one in two and one in three: the others are unnumbered.
        for every in [1, 2, 3] {
            let numbers = dictionary_of(&bson, every);
            let kept = stored::encode_numbered(bson.clone(), &numbers, None);
            let s = stored::split(&kept).unwrap();
            assert_eq!(s.bson(), &bson[..]);
            assert_eq!(s.table().and_then(|t| t.dictionary()), Some(DICTIONARY));
            assert_eq!(stored::into_bson(kept.clone()).unwrap(), bson);
            assert!(lookups_agree(&s, &numbers) > 2 * MIN_TABLE_FIELDS);
            // A reader without numbers, or with those of another dictionary, finds the same.
            assert!(lookups_agree(&s, &FieldNumbers::default()) > 2 * MIN_TABLE_FIELDS);
            let mut other = FieldNumbers::new(DICTIONARY + 1);
            for (name, n) in numbers.by_number().into_iter().rev() {
                other.insert(name, numbers.len() as u16 - 1 - n);
            }
            assert!(lookups_agree(&s, &other) > 2 * MIN_TABLE_FIELDS);
        }
    }
}

#[test]
fn a_name_numbered_after_the_write_is_found_by_its_name() {
    let bson = wide(5, MIN_TABLE_FIELDS);
    let before = FieldNumbers::from_names(DICTIONARY, [&b"_id"[..], b"f1"]);
    let kept = stored::encode_numbered(bson.clone(), &before, None);
    let s = stored::split(&kept).unwrap();
    // The dictionary grew: `f3` and `nested` got the next numbers, the old ones stay.
    let mut after = before.clone();
    after.add(b"f3");
    after.add(b"nested");
    assert_eq!(after.get(b"f1"), before.get(b"f1"));
    assert!(lookups_agree(&s, &after) > MIN_TABLE_FIELDS);
    let key = FieldKey::in_dictionary(b"f3", &after);
    assert_eq!(s.lookup_key(0, b"f3", key), Lookup::At(walk_pos(&bson, 0, "f3").unwrap()));
}

#[test]
fn a_new_version_keeps_the_numbers_of_the_one_it_replaces() {
    let bson = wide(9, MIN_TABLE_FIELDS);
    let numbers = dictionary_of(&bson, 1);
    let old = stored::encode_numbered(bson.clone(), &numbers, None);
    let old = stored::split(&old).unwrap();
    let back = stored::numbers_of(&old).unwrap();
    assert_eq!(back.dictionary(), DICTIONARY);
    for (name, n) in back.by_number() {
        assert_eq!(numbers.get(name), Some(n));
    }
    // An update whose write carries none of them numbers the new version as the old one.
    let carried = FieldNumbers::new(DICTIONARY);
    let new = stored::encode_numbered(bson.clone(), &carried, Some(&old));
    assert_eq!(new, old_bytes(&old));
    // An old version of another dictionary lends nothing: the write's numbers alone.
    let other = FieldNumbers::from_names(DICTIONARY + 1, [&b"f1"[..]]);
    let new = stored::encode_numbered(bson.clone(), &other, Some(&old));
    let s = stored::split(&new).unwrap();
    assert_eq!(s.table().and_then(|t| t.dictionary()), Some(DICTIONARY + 1));
    assert_eq!(stored::numbers_of(&s).unwrap().len(), 1);
    assert!(lookups_agree(&s, &other) > MIN_TABLE_FIELDS);
}

/// The stored value of `s` again.
fn old_bytes(s: &Stored<'_>) -> Vec<u8> {
    stored::encode_numbered(s.bson().to_vec(), &stored::numbers_of(s).unwrap(), None)
}

#[test]
fn a_collection_without_a_dictionary_keeps_the_hashed_tags() {
    let bson = wide(2, MIN_TABLE_FIELDS);
    let kept = stored::encode_numbered(bson.clone(), &FieldNumbers::default(), None);
    assert_eq!(kept, stored::encode(bson.clone()));
    let s = stored::split(&kept).unwrap();
    assert_eq!(s.table().and_then(|t| t.dictionary()), None);
    assert!(stored::numbers_of(&s).is_none());
    assert!(lookups_agree(&s, &dictionary_of(&bson, 1)) > MIN_TABLE_FIELDS);
    // A plain value reads as before.
    let plain = stored::split(&bson).unwrap();
    assert_eq!(plain.lookup_key(0, b"f1", FieldKey::of(b"f1")), Lookup::NoTable);
}

#[test]
fn a_damaged_numbered_table_is_refused() {
    let bson = wide(3, MIN_TABLE_FIELDS);
    let kept = stored::encode_numbered(bson.clone(), &dictionary_of(&bson, 1), None);
    let n = bson.len();
    let last = kept.len() - 1;
    let mut version = kept.clone();
    version[last - 1] = 3;
    let mut tables = kept.clone();
    tables[last - 3] = 0xFF;
    let cases = [kept[..n + 3].to_vec(), kept[..last].to_vec(), version, tables];
    for (i, bad) in cases.iter().enumerate() {
        assert!(stored::split(bad).is_err(), "case {i}");
    }
}

#[test]
fn a_dictionary_only_grows_and_never_gives_a_number_twice() {
    let mut d = FieldNumbers::new(DICTIONARY);
    assert_eq!(d.add(b"a"), Some(0));
    assert_eq!(d.add(b"b"), Some(1));
    assert_eq!(d.add(b"a"), Some(0));
    assert_eq!(d.add(b""), None);
    assert_eq!(d.add(&[b'x'; MAX_FIELD_NAME_LEN + 1]), None);
    assert_eq!(d.add(&[b'x'; MAX_FIELD_NAME_LEN]), Some(2));
    for i in d.len()..MAX_FIELD_NAMES {
        assert_eq!(d.add(format!("n{i}").as_bytes()), Some(i as u16));
    }
    assert!(d.is_full());
    assert_eq!(d.add(b"late"), None);
    assert_eq!(d.get(b"b"), Some(1));
    let numbers: Vec<u16> = d.by_number().into_iter().map(|(_, n)| n).collect();
    assert_eq!(numbers, (0..MAX_FIELD_NAMES as u16).collect::<Vec<_>>());
    // A part of it keeps the identifier and the numbers; a number that cannot be one is left out.
    let mut part = d.subset([&b"b"[..], b"absent"]);
    assert_eq!((part.dictionary(), part.len(), part.get(b"b")), (DICTIONARY, 1, Some(1)));
    part.insert(b"u", UNNUMBERED);
    part.insert(b"b", 7);
    assert_eq!((part.len(), part.get(b"b")), (1, Some(1)));
    let back = FieldNumbers::from_names(DICTIONARY, d.by_number().into_iter().map(|(n, _)| n));
    assert_eq!(back, d);
    // As a write or a filter carries it.
    assert_eq!(FieldNumbers::from_bytes(&part.to_bytes()), Some(part.clone()));
    assert_eq!(FieldNumbers::from_bytes(&d.to_bytes()), Some(d.clone()));
    let bytes = part.to_bytes();
    assert_eq!(FieldNumbers::from_bytes(&bytes[..bytes.len() - 1]), None);
    assert_eq!(FieldNumbers::from_bytes(&bytes[..3]), None);
}
