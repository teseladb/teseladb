//! The portable and accelerated paths give the same result. A single test per binary because
//! `force_portable` is global.
mod common;

use teseladb_hash::{accel, adler32, crc32c, force_portable, portable, sha1, sha256};

#[test]
fn forced_portable_paths_match_accelerated() {
    let a = accel();
    println!("detected acceleration: {a:?}");
    for &(n, crc, _, s1, s256) in common::CASES {
        let d = common::pattern(n);
        for portable in [false, true] {
            force_portable(portable);
            assert_eq!(crc32c(&d), crc);
            assert_eq!(common::hex(&sha1(&d)), s1);
            assert_eq!(common::hex(&sha256(&d)), s256);
        }
    }
    // Adler-32: every length around the 16-byte blocks and the NMAX boundary, all-0xff bytes
    // (largest sums) and random ones, from arbitrary starting values.
    let mut rng = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let random: Vec<u8> = (0..40_000).map(|_| next().to_le_bytes()[0]).collect();
    let ones = vec![0xffu8; 40_000];
    for data in [&random, &ones] {
        for n in (0..70).chain([5_551, 5_552, 5_553, 11_104, 11_120, 39_999, 40_000]) {
            force_portable(false);
            let fast = adler32(&data[..n]);
            force_portable(true);
            assert_eq!(fast, adler32(&data[..n]), "length {n}");
            assert_eq!(portable::adler32(1, &data[..n]), fast);
            force_portable(false);
            // From an arbitrary intermediate state: a random prefix first.
            let cut = usize::try_from(next() % 300).expect("below 300");
            let mut s = teseladb_hash::Adler32::default();
            s.update(&random[..cut]);
            s.update(&data[..n]);
            let want = portable::adler32(portable::adler32(1, &random[..cut]), &data[..n]);
            assert_eq!(s.value(), want, "length {n} after {cut}");
        }
    }
    // SHA-1 and SHA-256: every length around one, two and three 64-byte blocks plus a long input,
    // so that several blocks go through one call of the accelerated compression.
    for n in (0..200).chain([1_000, 4_096, 39_999]) {
        let d = &random[..n];
        force_portable(false);
        let (fast1, fast256) = (sha1(d), sha256(d));
        force_portable(true);
        assert_eq!(fast1, sha1(d), "SHA-1, length {n}");
        assert_eq!(fast256, sha256(d), "SHA-256, length {n}");
    }
    force_portable(false);
    expected_acceleration_is_detected(a);
}

/// With `TESELADB_EXPECT_ACCEL` (names separated by commas), the accelerations the CPU must have:
/// `scripts/test-x86_64.sh` sets it so that a run on a CPU without them fails instead of testing
/// only the portable paths.
fn expected_acceleration_is_detected(a: teseladb_hash::Accel) {
    let Ok(expected) = std::env::var("TESELADB_EXPECT_ACCEL") else {
        return;
    };
    for name in expected.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let detected = match name {
            "crc32c" => a.crc32c,
            "sha1" => a.sha1,
            "sha256" => a.sha256,
            "adler32" => a.adler32,
            other => panic!("unknown acceleration {other:?} in TESELADB_EXPECT_ACCEL"),
        };
        assert!(detected, "{name} is not detected: {a:?}");
    }
}
