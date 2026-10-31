//! SHA-256: FIPS 180-4 vectors.
mod common;

use common::hex;
use teseladb_hash::{Sha256, sha256};
use teseladb_types::Rng;

#[test]
fn fips_180_4_vectors() {
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    let mut h = Sha256::new();
    for _ in 0..1000 {
        h.update(&[b'a'; 1000]);
    }
    assert_eq!(
        hex(&h.finalize()),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn independent_reference_values() {
    for &(n, _, _, _, want) in common::CASES {
        assert_eq!(hex(&sha256(&common::pattern(n))), want, "length {n}");
    }
}

#[test]
fn streaming_in_arbitrary_pieces() {
    let d = common::pattern(10_000);
    let mut rng = Rng::new(10);
    for _ in 0..100 {
        let mut h = Sha256::default();
        let mut pos = 0;
        while pos < d.len() {
            let take = rng.range(0, 200).min((d.len() - pos) as u64) as usize;
            h.update(&d[pos..pos + take]);
            pos += take;
        }
        assert_eq!(h.finalize(), sha256(&d));
    }
}

#[test]
fn portable_compression_on_one_block() {
    let mut block = [0u8; 64];
    block[..3].copy_from_slice(b"abc");
    block[3] = 0x80;
    block[63] = 24;
    let mut st = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    teseladb_hash::portable::sha256_compress(&mut st, &[block]);
    let mut out = Vec::new();
    for w in st {
        out.extend_from_slice(&w.to_be_bytes());
    }
    assert_eq!(hex(&out), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}
