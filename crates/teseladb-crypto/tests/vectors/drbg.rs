//! HMAC_DRBG with SHA-256 against the CAVP vectors without prediction resistance.

use teseladb_crypto::rand::{HmacDrbg, RESEED_INTERVAL, ReseedRequired};

use crate::common::{cavp, field, hex};

#[test]
fn hmac_drbg_sha256_matches_cavp() {
    let mut n = 0;
    for (_, rec) in cavp("HMAC_DRBG_SHA256_pr_false.rsp") {
        let additional: Vec<Vec<u8>> =
            rec.iter().filter(|(k, _)| k == "AdditionalInput").map(|(_, v)| hex(v)).collect();
        let mut d = HmacDrbg::new(
            &hex(field(&rec, "EntropyInput")),
            &hex(field(&rec, "Nonce")),
            &hex(field(&rec, "PersonalizationString")),
        );
        d.reseed(
            &hex(field(&rec, "EntropyInputReseed")),
            &hex(field(&rec, "AdditionalInputReseed")),
        );
        let want = hex(field(&rec, "ReturnedBits"));
        let mut out = vec![0u8; want.len()];
        d.generate(&mut out, &additional[0]).expect("first");
        d.generate(&mut out, &additional[1]).expect("second");
        assert_eq!(out, want, "COUNT {}", field(&rec, "COUNT"));
        n += 1;
    }
    assert_eq!(n, 240);
}

#[test]
fn the_generator_refuses_to_run_past_its_reseed_interval() {
    let mut d = HmacDrbg::new(&[1; 32], &[2; 16], b"").with_reseed_interval(3);
    assert!(format!("{d:?}").contains("requests 1"));
    const { assert!(RESEED_INTERVAL > 1_000_000) };
    let mut out = [0u8; 8];
    for _ in 0..3 {
        d.generate(&mut out, b"").expect("fresh");
    }
    let mut stale = d.clone();
    assert_eq!(stale.generate(&mut out, b""), Err(ReseedRequired));
    stale.reseed(&[3; 32], b"");
    assert_eq!(stale.generate(&mut out, b""), Ok(()));
}
