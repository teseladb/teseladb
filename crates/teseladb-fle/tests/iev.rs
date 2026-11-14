//! The indexed values the collection keeps (017 FR-001) and the value helpers: every layout reads
//! back what it wrote, a block gives back its count and contention, and every malformed shape is
//! refused instead of read past its end.

use std::cmp::Ordering;

use teseladb_bson::{Decimal128, Value};
use teseladb_fle::cipher;
use teseladb_fle::iev::{BLOCK_LEN, Block, IndexedValue, Kind, Malformed};
use teseladb_fle::tokens::{self, Token};
use teseladb_fle::values;

fn server() -> Token {
    Token([9u8; 32])
}

fn block(count: u64) -> Block {
    let tag = tokens::tag(&Token([1u8; 32]), count);
    let iv = [u8::try_from(count % 256).expect("under 256"); 16];
    Block::new(&server(), (count, 3), tag, (iv, [0x55; 16]))
}

fn value(kind: Kind, blocks: u64) -> IndexedValue {
    let encryption = Token([4u8; 32]);
    let mut plain = vec![0xaa; 16];
    plain.extend_from_slice(b"client ciphertext");
    IndexedValue {
        kind,
        key_id: [7u8; 16],
        bson_type: 0x10,
        server_value: cipher::encrypt(&encryption, [2u8; 16], &plain),
        blocks: (1..=blocks).map(block).collect(),
    }
}

#[test]
fn every_kind_reads_back_what_it_writes() {
    for (kind, blocks) in [
        (Kind::Equality, 1),
        (Kind::Range, 1),
        (Kind::Range, 65),
        (Kind::Text { substrings: 0, suffixes: 0 }, 1),
        (Kind::Text { substrings: 3, suffixes: 2 }, 9),
    ] {
        let v = value(kind, blocks);
        let bytes = v.to_bytes();
        let back = IndexedValue::parse(&bytes).expect("written by to_bytes");
        assert_eq!(back, v, "{kind:?}");
        assert_eq!(back.tags().count(), usize::try_from(blocks).expect("small"));
    }
}

#[test]
fn a_block_gives_back_its_count_and_contention_and_its_tag() {
    let b = block(12);
    assert_eq!(b.count_and_contention(&server()), (12, 3));
    assert_eq!(b.tag, tokens::tag(&Token([1u8; 32]), 12));
    // With another value's token the numbers are noise, never a panic.
    let _ = b.count_and_contention(&Token([8u8; 32]));
}

#[test]
fn the_client_value_comes_back_with_the_server_encryption_token() {
    let v = value(Kind::Equality, 1);
    let plain = v.client_value(&Token([4u8; 32])).expect("long enough");
    assert_eq!(&plain[..16], &[0xaa; 16]);
    assert_eq!(&plain[16..], b"client ciphertext");
}

#[test]
fn malformed_values_are_refused() {
    let good = value(Kind::Equality, 1).to_bytes();
    // No bytes, an unknown subtype, a subtype that is no indexed value.
    assert_eq!(IndexedValue::parse(&[]), Err(Malformed));
    assert_eq!(IndexedValue::parse(&[200]), Err(Malformed));
    let mut unindexed = good.clone();
    unindexed[0] = 16;
    assert_eq!(IndexedValue::parse(&unindexed), Err(Malformed));
    // Cut in the key id, at the type byte, and so short that the blocks leave the server value
    // under an IV, a key id and one byte (the format has no length for that value: it is what the
    // blocks leave, so a cut of one byte only shortens it).
    assert!(IndexedValue::parse(&good[..good.len() - 1]).is_ok());
    for cut in [5, 17, good.len() - 17] {
        assert_eq!(IndexedValue::parse(&good[..cut]), Err(Malformed), "cut at {cut}");
    }
    // A server value shorter than an IV, a key id and one byte.
    let mut short = value(Kind::Equality, 1);
    short.server_value.truncate(32);
    assert_eq!(IndexedValue::parse(&short.to_bytes()), Err(Malformed));
    // A range value with no edges, or without its edge count.
    let mut no_edges = value(Kind::Range, 1);
    no_edges.blocks.clear();
    assert_eq!(IndexedValue::parse(&no_edges.to_bytes()), Err(Malformed));
    assert_eq!(IndexedValue::parse(&value(Kind::Range, 1).to_bytes()[..18]), Err(Malformed));
    // A text value whose edges do not cover its exact, substring and suffix blocks.
    let mut text = value(Kind::Text { substrings: 4, suffixes: 4 }, 9).to_bytes();
    text[18..22].copy_from_slice(&8u32.to_le_bytes());
    assert_eq!(IndexedValue::parse(&text), Err(Malformed));
    // A text value cut in each of its three counts.
    let text = value(Kind::Text { substrings: 1, suffixes: 1 }, 3).to_bytes();
    for cut in [20, 24, 28] {
        assert_eq!(IndexedValue::parse(&text[..cut]), Err(Malformed), "text cut at {cut}");
    }
    // Blocks claimed past the end.
    let mut many = value(Kind::Range, 2).to_bytes();
    many[18] = 255;
    assert_eq!(IndexedValue::parse(&many), Err(Malformed));
    assert_eq!(BLOCK_LEN, 96);
}

fn dec(negative: bool, coefficient: u128, exponent: i32) -> Decimal128 {
    Decimal128::from_parts(negative, coefficient, exponent).expect("in range")
}

#[test]
fn integral_decimals_become_integers_and_the_rest_do_not() {
    assert_eq!(values::decimal_to_i64(dec(false, 120, -1)), Some(12));
    assert_eq!(values::decimal_to_i64(dec(true, 5, 2)), Some(-500));
    assert_eq!(values::decimal_to_i64(dec(false, 15, -1)), None);
    assert_eq!(values::decimal_to_i64(dec(false, 1, 19)), None);
    assert_eq!(values::decimal_to_i64(dec(false, 1, 6000)), None);
    assert_eq!(values::decimal_to_i64(Decimal128::nan()), None);
    assert_eq!(values::decimal_to_i64(Decimal128::infinity(true)), None);
}

#[test]
fn decimals_become_doubles_with_their_sign() {
    assert_eq!(values::decimal_to_f64(dec(false, 25, -1)), Some(2.5));
    assert_eq!(values::decimal_to_f64(dec(true, 3, 2)), Some(-300.0));
    assert_eq!(values::decimal_to_f64(Decimal128::nan()), None);
}

#[test]
fn compare_follows_the_server_order_across_numeric_types_and_strings() {
    assert_eq!(values::compare(&Value::Int32(3), &Value::Double(3.0)), Ordering::Equal);
    assert_eq!(values::compare(&Value::Int64(2), &Value::Int32(3)), Ordering::Less);
    assert_eq!(
        values::compare(&Value::String("b".into()), &Value::String("a".into())),
        Ordering::Greater
    );
    // Numbers sort before strings.
    assert_eq!(values::compare(&Value::Int32(9), &Value::String("0".into())), Ordering::Less);
}
