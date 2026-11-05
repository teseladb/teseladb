//! Edges of the key formats (003 FR-009, FR-011, FR-014): a decimal whose bytes are not canonical
//! comes back with its 16 bytes, extra type bits are refused, key intervals at the end of the byte
//! space, the two levels of the meta-ranges, and the insertion order of capped collections and
//! the `_id` order of clustered ones (T031).

use teseladb_keys::{Direction, KeyBuilder, KeyParts, KeyRange, TypeBits, decode, encode};
use teseladb_bson::{Decimal128, Document, RawDoc, Value};

#[test]
fn a_decimal_with_a_coefficient_past_34_digits_comes_back_byte_for_byte() {
    // 10^34 written with the normal combination: not canonical, worth 0E+0.
    let bits = (6176u128 << 113) | 10u128.pow(34);
    let odd = Decimal128::from_le_bytes(bits.to_le_bytes());
    assert_eq!(odd.to_string(), "0");
    let doc = Document::new().with("v", Value::Decimal128(odd)).to_bytes().unwrap();
    let v = RawDoc::from_bytes(&doc).unwrap().get("v").unwrap();
    let (key, tb) = encode(&[(v, Direction::Ascending)]);
    // The same key as any zero.
    let zero = Document::new().with("v", 0).to_bytes().unwrap();
    let z = RawDoc::from_bytes(&zero).unwrap().get("v").unwrap();
    assert_eq!(key, encode(&[(z, Direction::Ascending)]).0);
    assert_eq!(decode(&key, &tb, &[Direction::Ascending]).unwrap(), vec![Value::Decimal128(odd)]);
}

#[test]
fn type_bits_with_bytes_left_over_are_refused() {
    let doc = Document::new().with("v", 1).to_bytes().unwrap();
    let v = RawDoc::from_bytes(&doc).unwrap().get("v").unwrap();
    let (key, tb) = encode(&[(v, Direction::Ascending)]);
    let longer = TypeBits::from_bytes([tb.as_bytes(), &[0]].concat());
    assert!(decode(&key, &longer, &[Direction::Ascending]).is_err());
}

#[test]
fn intervals_at_the_end_of_the_byte_space() {
    let all = KeyRange { start: Vec::new(), end: None };
    assert_eq!(all.under(&[0xFF]).end, None, "nothing comes after FF");
    assert_eq!(all.under(&[1, 0xFF]).end, Some(vec![2]));
    assert_eq!(all.under(&[1, 0xFF]).start, vec![1, 0xFF]);
}

#[test]
fn meta_ranges_have_two_levels() {
    let one = KeyBuilder::meta(1, b"x");
    assert_eq!(KeyParts::of(one.as_bytes()).unwrap(), KeyParts::Meta { level: 1, end: b"x" });
    let e = KeyParts::of(&[]).unwrap_err();
    assert_eq!(e.to_string(), "not a key: empty");
}

#[test]
#[should_panic(expected = "meta-ranges have two levels")]
fn a_third_meta_level_is_a_mistake() {
    KeyBuilder::meta(3, b"x");
}

#[test]
fn capped_and_clustered_documents_keep_their_order() {
    // Insertion order across byte boundaries.
    let seqs = [1u64, 2, 255, 256, 1 << 32, u64::MAX];
    let keys: Vec<_> = seqs.iter().map(|s| KeyBuilder::document_capped(4, 9, *s)).collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]));
    // The _id order, numbers of every type mixed, with no bucket in front.
    let ids = [
        Value::Int32(-3),
        Value::Double(-0.5),
        Value::Int64(0),
        Value::Double(2.5),
        Value::Int32(7),
    ];
    let docs: Vec<Vec<u8>> =
        ids.iter().map(|v| Document::new().with("_id", v.clone()).to_bytes().unwrap()).collect();
    let keys: Vec<_> = docs
        .iter()
        .map(|d| {
            KeyBuilder::document_clustered(4, 9, RawDoc::from_bytes(d).unwrap().get("_id").unwrap())
        })
        .collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]));
}

/// Whole numbers take a shorter path than the other numbers; their bytes are the same as those
/// of the same value as a decimal, which goes through the general one.
#[test]
fn whole_numbers_encode_as_their_decimal() {
    use teseladb_keys::{Direction, TypeBits, encode_value};
    use teseladb_bson::{Decimal128, RawValue};
    let enc = |v: RawValue<'_>| {
        let mut out = Vec::new();
        encode_value(&mut out, v, Direction::Ascending, &mut TypeBits::default());
        out
    };
    let mut n: i64 = 1;
    let mut samples = vec![0, 1, -1, 7, 10, -10, 100, 1_000_000, i64::MAX, i64::MIN, i64::MIN + 1];
    for _ in 0..2000 {
        n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        samples.extend([n, n >> 20, n >> 40, (n >> 50) * 1000]);
    }
    for s in samples {
        let d = Decimal128::from_parts(s < 0, u128::from(s.unsigned_abs()), 0).unwrap();
        let (a, b) = (enc(RawValue::Int64(s)), enc(RawValue::Decimal128(d)));
        assert_eq!(a[1..], b[1..], "{s}");
        if let Ok(small) = i32::try_from(s) {
            assert_eq!(enc(RawValue::Int32(small)), a, "{s}");
        }
    }
}
