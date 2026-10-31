//! SHA-1: FIPS 180-4 vectors and accelerated versus portable comparison.
mod common;

use common::hex;
use teseladb_hash::{Sha1, sha1};
use teseladb_types::Rng;

#[test]
fn fips_180_4_vectors() {
    assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    let mut h = Sha1::new();
    for _ in 0..1000 {
        h.update(&[b'a'; 1000]);
    }
    assert_eq!(hex(&h.finalize()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
}

#[test]
fn independent_reference_values() {
    for &(n, _, _, want, _) in common::CASES {
        assert_eq!(hex(&sha1(&common::pattern(n))), want, "length {n}");
    }
}

#[test]
fn streaming_in_arbitrary_pieces() {
    let d = common::pattern(10_000);
    let mut rng = Rng::new(9);
    for _ in 0..100 {
        let mut h = Sha1::default();
        let mut pos = 0;
        while pos < d.len() {
            let take = rng.range(0, 200).min((d.len() - pos) as u64) as usize;
            h.update(&d[pos..pos + take]);
            pos += take;
        }
        assert_eq!(h.finalize(), sha1(&d));
    }
}

#[test]
fn portable_compression_on_one_block() {
    // "abc" padded to one block: the portable compression over the initial state gives the digest.
    let mut block = [0u8; 64];
    block[..3].copy_from_slice(b"abc");
    block[3] = 0x80;
    block[63] = 24;
    let mut st = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    teseladb_hash::portable::sha1_compress(&mut st, &[block]);
    let mut out = Vec::new();
    for w in st {
        out.extend_from_slice(&w.to_be_bytes());
    }
    assert_eq!(hex(&out), "a9993e364706816aba3e25717850c26c9cd0d89d");
}
