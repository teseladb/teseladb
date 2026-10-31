//! `decode(encode(v)) = v` with its exact type (003 FR-009, T014): every value of a random pool,
//! ascending, descending and in compound keys, comes back to the same BSON bytes (a number's type,
//! a decimal's scale, a zero's sign, a NaN's bits, a symbol); a damaged key is refused.

mod common;

use common::{pool, raw};
use teseladb_bson::{Document, Value};
use teseladb_keys::{Direction, TypeBits, decode, encode};

fn bytes_of(v: &Value) -> Vec<u8> {
    Document::new().with("v", v.clone()).to_bytes().unwrap()
}

#[test]
fn every_value_comes_back_exactly() {
    for doc in pool(3, 3_000) {
        let v = raw(&doc);
        for dir in [Direction::Ascending, Direction::Descending] {
            let (key, bits) = encode(&[(v, dir)]);
            let back = decode(&key, &bits, &[dir]).unwrap_or_else(|e| panic!("{v:?}: {e}"));
            assert_eq!(back.len(), 1);
            assert_eq!(bytes_of(&back[0]), doc, "{v:?} {dir:?}");
        }
    }
}

#[test]
fn compound_keys_come_back_field_by_field() {
    let docs = pool(4, 900);
    let dirs = [Direction::Descending, Direction::Ascending, Direction::Descending];
    for i in 0..docs.len() / 3 {
        let fields: Vec<_> = (0..3).map(|j| (raw(&docs[3 * i + j]), dirs[j])).collect();
        let (key, bits) = encode(&fields);
        let back = decode(&key, &bits, &dirs).unwrap();
        for j in 0..3 {
            assert_eq!(bytes_of(&back[j]), docs[3 * i + j]);
        }
        // Bits stored apart and read back.
        let stored = TypeBits::from_bytes(bits.as_bytes().to_vec());
        assert_eq!(decode(&key, &stored, &dirs).unwrap().len(), 3);
    }
}

#[test]
fn a_damaged_key_or_bits_are_refused() {
    let docs = pool(5, 300);
    let mut refused = 0;
    for doc in &docs {
        let (key, bits) = encode(&[(raw(doc), Direction::Ascending)]);
        // Cut short, with a byte too many, with bits missing or for another field count.
        for cut in 0..key.len() {
            refused += usize::from(decode(&key[..cut], &bits, &[Direction::Ascending]).is_err());
        }
        let mut longer = key.clone();
        longer.push(0x10);
        assert!(decode(&longer, &bits, &[Direction::Ascending]).is_err());
        if !bits.as_bytes().is_empty() {
            let fewer = TypeBits::from_bytes(bits.as_bytes()[..bits.as_bytes().len() - 1].to_vec());
            assert!(decode(&key, &fewer, &[Direction::Ascending]).is_err());
        }
        assert!(decode(&key, &bits, &[]).is_err());
    }
    assert!(refused > 0);
    // Bytes that are not a key at all, among them an old binary shorter than its inner length.
    let junk: [&[u8]; 7] = [
        &[0x00],
        &[0x0F],
        &[0x40],
        &[0x13, 0x09],
        &[0x14, 0x00, 0x01],
        &[0x13, 0x05, 0x80, 0x01, 0xFF],
        &[0x17, 0, 0, 0, 3, 2],
    ];
    for j in junk {
        assert!(decode(j, &TypeBits::default(), &[Direction::Ascending]).is_err(), "{j:?}");
    }
    assert!(
        decode(&[0x14, 0, 0], &TypeBits::from_bytes(vec![9]), &[Direction::Ascending]).is_err()
    );
    let e = decode(&[0x40], &TypeBits::default(), &[Direction::Ascending]).unwrap_err();
    assert_eq!(e.to_string(), "the key does not decode: not a class byte");
}

#[test]
fn values_len_finds_where_the_values_of_a_key_end() {
    use teseladb_keys::values_len;
    let docs = pool(5, 1_500);
    let dirs = [Direction::Ascending, Direction::Descending, Direction::Ascending];
    for i in 0..docs.len() / 3 {
        let fields: Vec<_> = (0..3).map(|j| (raw(&docs[3 * i + j]), dirs[j])).collect();
        let (key, _) = encode(&fields);
        let mut entry = key.clone();
        // A primary key after the values, as in an index entry.
        let (pk, _) = encode(&[(raw(&docs[i]), Direction::Ascending)]);
        entry.extend(pk);
        assert_eq!(values_len(&entry, &dirs), Ok(key.len()), "{fields:?}");
        assert!(values_len(&key[..key.len() - 1], &dirs).is_err(), "cut short");
    }
    assert!(values_len(&[0x01], &[Direction::Ascending]).is_err(), "not a class byte");
    assert!(values_len(&[0x40], &[Direction::Ascending]).is_err(), "past the last class");
    assert!(values_len(&[0x13, 0x07], &[Direction::Ascending]).is_err(), "not a number");
    assert!(values_len(&[0x14, b'a', 0, 7], &[Direction::Ascending]).is_err(), "a lone 00");
}

#[test]
fn values_len_measures_strings_in_a_collations_order() {
    use teseladb_bson::{Collator, RawDoc};
    use teseladb_collation::Collation;
    use teseladb_keys::{TypeBits as Bits, encode_value_with, values_len};
    let c = Collation::for_locale("de@collation=phonebook").unwrap();
    let v = Value::Array(vec![Value::String("Straße".into()), Value::Symbol("Äpfel".into())]);
    let doc = Document::new().with("v", v).to_bytes().unwrap();
    let raw = RawDoc::from_bytes(&doc).unwrap().get("v").unwrap();
    for dir in [Direction::Ascending, Direction::Descending] {
        let mut key = Vec::new();
        encode_value_with(&mut key, raw, dir, &mut Bits::default(), Some(&c as &dyn Collator));
        let len = key.len();
        key.extend([0x10, 0x10]);
        assert_eq!(values_len(&key, &[dir]), Ok(len));
    }
}
