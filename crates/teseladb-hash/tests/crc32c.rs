//! CRC32C: RFC 3720 vectors and accelerated versus portable comparison.
mod common;

use teseladb_hash::{Crc32c, crc32c, crc32c_append, crc32c_portable};
use teseladb_types::Rng;

#[test]
fn rfc3720_vectors() {
    assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
    assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
    let asc: Vec<u8> = (0..32).collect();
    assert_eq!(crc32c(&asc), 0x46dd_794e);
    let desc: Vec<u8> = (0..32).rev().collect();
    assert_eq!(crc32c(&desc), 0x113f_db5c);
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
}

#[test]
fn independent_reference_values() {
    for &(n, crc, ..) in common::CASES {
        let d = common::pattern(n);
        assert_eq!(crc32c(&d), crc, "length {n}");
        assert_eq!(crc32c_portable(&d), crc, "portable, length {n}");
    }
}

#[test]
fn accelerated_matches_portable_on_random_inputs() {
    let mut rng = Rng::new(0xc5c);
    let mut buf = vec![0u8; 4096];
    for _ in 0..100_000 {
        let n = rng.range(0, buf.len() as u64 + 1) as usize;
        let off = rng.range(0, 8) as usize;
        let n = n.saturating_sub(off);
        rng.fill(&mut buf[off..off + n]);
        let s = &buf[off..off + n];
        assert_eq!(crc32c(s), crc32c_portable(s));
    }
}

#[test]
fn append_and_streaming_equal_one_shot() {
    let d = common::pattern(10_000);
    let mut rng = Rng::new(3);
    for _ in 0..200 {
        let cut = rng.range(0, d.len() as u64 + 1) as usize;
        assert_eq!(crc32c_append(crc32c(&d[..cut]), &d[cut..]), crc32c(&d));
        let mut s = Crc32c::new();
        s.update(&d[..cut]);
        s.update(&d[cut..]);
        assert_eq!(s.value(), crc32c(&d));
    }
}
