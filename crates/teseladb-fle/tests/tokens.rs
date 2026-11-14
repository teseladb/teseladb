//! The token chains of Queryable Encryption (017 FR-009) against HMAC-SHA-256 values computed by
//! an independent implementation (Python's `hmac` module) from a root key of bytes 0 to 31, and
//! the subtype byte of encrypted binaries.

use teseladb_fle::subtype::BlobSubtype;
use teseladb_fle::tokens::{self, Collection, TextKind, Token};

fn hex(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex digit");
    }
    out
}

fn root() -> Token {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::try_from(i).expect("index under 32");
    }
    Token(bytes)
}

#[test]
fn the_level_one_tokens_match_an_independent_hmac() {
    let r = root();
    assert_eq!(
        tokens::collections_level1(&r).0,
        hex("3b345d4e3f7a9922d8942f7c4f9c46a36307684beb1c02f98dba9327be8e1617")
    );
    assert_eq!(
        tokens::server_token_derivation_level1(&r).0,
        hex("be563a677f0334d6c3b6b698db0ba3baa59ee0f41841cc64677e64e16c27ddfb")
    );
    assert_eq!(
        tokens::server_data_encryption_level1(&r).0,
        hex("9bc14433322a55626371fcef5ec5c06d6dcc19118012622f7e0ec044949f6fa7")
    );
}

#[test]
fn each_collection_token_is_its_number_under_the_level_one_token() {
    let l1 = tokens::collections_level1(&root());
    let expected = [
        (Collection::Edc, "9f48337cb39d16cfe85611c01668ef557c85595b0d39247d09a4af22e9e742ff"),
        (Collection::Esc, "5256db04287b3318101386d6163a01f82fb054c0aa8832eb4894a0daea88f5a4"),
        (Collection::Ecc, "ce6424c488ea7ec34f8a4510d2ab481635215bea9177fb4f719544960afbc6a1"),
        (Collection::Ecoc, "fa1d52d4e14bf68f7a8afd60037d9aa527d8237c3fdb16e933eaf20fc3e48eda"),
    ];
    for (c, want) in expected {
        assert_eq!(tokens::collection_token(&l1, c).0, hex(want), "{c:?}");
    }
}

#[test]
fn a_pair_is_two_little_endian_numbers_and_padding_is_seventeen_zeros() {
    let r = root();
    assert_eq!(
        r.derive_pair(7, 9).0,
        hex("e44ca5b4a923ad21e7b18405182988c91e6ca813dd4269d07a749b2e9f2c3488")
    );
    let esc = tokens::collection_token(&tokens::collections_level1(&r), Collection::Esc);
    assert_eq!(
        tokens::anchor_padding_root(&esc).0,
        hex("b9e7f56f8714eab09364e1403b893bc3064a0df1ec4e3d5de002df98f95d2198")
    );
}

#[test]
fn tags_and_text_tokens_are_numbers_under_their_base() {
    let edc = tokens::collection_token(&tokens::collections_level1(&root()), Collection::Edc);
    assert_eq!(
        tokens::tag(&edc, 5).0,
        hex("c97a9f3ccd80fb3bbebf42118f5e40bc0767ba6488a566e1d525ba70bfdf5efd")
    );
    assert_eq!(
        tokens::text_token(&edc, TextKind::Prefix).0,
        hex("d6f0475cefc3f094e3a6f2e57a35f658bc9dd678b681ffc1cebd444a7d51a4c6")
    );
}

#[test]
fn the_one_and_two_derivations_are_the_numbered_children() {
    let t = root();
    let one = t.derive_u64(1);
    let two = t.derive_u64(2);
    assert_eq!(tokens::edc_twice_derived(&t), one);
    assert_eq!(tokens::esc_twice_derived_tag(&t), one);
    assert_eq!(tokens::esc_twice_derived_value(&t), two);
    assert_eq!(tokens::server_count_and_contention(&t), one);
    assert_eq!(tokens::server_zeros(&t), two);
    assert_eq!(tokens::anchor_padding_key(&t), one);
    assert_eq!(tokens::anchor_padding_value(&t), two);
    assert_ne!(one, two);
}

#[test]
fn a_token_takes_exactly_thirty_two_bytes_and_never_prints_them() {
    let bytes = [7u8; 32];
    let t = Token::from_slice(&bytes).expect("32 bytes");
    assert_eq!(t.as_bytes(), &bytes);
    assert!(Token::from_slice(&[7u8; 31]).is_none());
    assert!(Token::from_slice(&[7u8; 33]).is_none());
    assert_eq!(format!("{t:?}"), "Token(..)");
}

#[test]
fn every_subtype_byte_round_trips_and_the_gaps_are_none() {
    let known: [(u8, BlobSubtype); 18] = [
        (0, BlobSubtype::Fle1Placeholder),
        (1, BlobSubtype::Fle1Deterministic),
        (2, BlobSubtype::Fle1Random),
        (3, BlobSubtype::Fle2Placeholder),
        (4, BlobSubtype::Fle2InsertUpdateV1),
        (5, BlobSubtype::Fle2FindEqualityV1),
        (6, BlobSubtype::Fle2UnindexedV1),
        (7, BlobSubtype::Fle2EqualityIndexedV1),
        (9, BlobSubtype::Fle2RangeIndexedV1),
        (10, BlobSubtype::Fle2FindRangeV1),
        (11, BlobSubtype::Fle2InsertUpdate),
        (12, BlobSubtype::Fle2FindEquality),
        (13, BlobSubtype::Fle2FindRange),
        (14, BlobSubtype::Fle2EqualityIndexed),
        (15, BlobSubtype::Fle2RangeIndexed),
        (16, BlobSubtype::Fle2Unindexed),
        (17, BlobSubtype::Fle2TextIndexed),
        (18, BlobSubtype::Fle2FindText),
    ];
    for (byte, kind) in known {
        assert_eq!(BlobSubtype::from_byte(byte), Some(kind));
        assert_eq!(kind as u8, byte);
        assert_eq!(BlobSubtype::of(&[byte, 0xff]), Some(kind));
    }
    assert_eq!(BlobSubtype::from_byte(8), None);
    assert_eq!(BlobSubtype::from_byte(19), None);
    assert_eq!(BlobSubtype::of(&[]), None);
}

#[test]
fn only_the_four_stored_kinds_are_stored_and_only_three_are_find_payloads() {
    let all = (0u8..=255).filter_map(BlobSubtype::from_byte).collect::<Vec<_>>();
    let stored = all.iter().filter(|k| k.is_stored_fle2()).count();
    let finds = all.iter().filter(|k| k.is_find_payload()).count();
    assert_eq!(stored, 4);
    assert_eq!(finds, 3);
    assert!(BlobSubtype::Fle2TextIndexed.is_stored_fle2());
    assert!(BlobSubtype::Fle2FindText.is_find_payload());
    assert!(!BlobSubtype::Fle2InsertUpdate.is_stored_fle2());
    assert!(!BlobSubtype::Fle2InsertUpdate.is_find_payload());
}
