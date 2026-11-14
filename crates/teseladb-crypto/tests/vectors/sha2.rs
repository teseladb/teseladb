//! SHA-384 and SHA-512 against the CAVP byte-oriented vectors (FIPS 180-4).

use teseladb_crypto::sha512::{Sha384, Sha512, sha384, sha512};

use crate::common::{cavp, field, hex};

fn run(file: &str, f: impl Fn(&[u8]) -> Vec<u8>) -> usize {
    let mut n = 0;
    for (_, rec) in cavp(file) {
        let len: usize = field(&rec, "Len").parse().expect("Len");
        let msg = hex(field(&rec, "Msg"));
        let msg = &msg[..len / 8];
        assert_eq!(f(msg), hex(field(&rec, "MD")), "{file} Len={len}");
        n += 1;
    }
    n
}

#[test]
fn sha512_matches_cavp_short_and_long_messages() {
    let n = run("SHA512ShortMsg.rsp", |m| sha512(m).to_vec())
        + run("SHA512LongMsg.rsp", |m| sha512(m).to_vec());
    assert!(n > 200);
}

#[test]
fn sha384_matches_cavp_short_and_long_messages() {
    let n = run("SHA384ShortMsg.rsp", |m| sha384(m).to_vec())
        + run("SHA384LongMsg.rsp", |m| sha384(m).to_vec());
    assert!(n > 200);
}

#[test]
fn incremental_updates_in_odd_pieces_give_the_one_shot_digest() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 7).to_le_bytes()[0]).collect();
    for split in [0, 1, 111, 112, 127, 128, 129, 500] {
        let mut h = Sha512::new();
        h.update(&data[..split]);
        h.update(&data[split..]);
        assert_eq!(h.finalize(), sha512(&data));
        let mut h = Sha384::default();
        h.update(&data[..split]);
        h.update(&data[split..]);
        assert_eq!(h.finalize(), sha384(&data));
    }
}

#[test]
fn sha224_matches_cavp_short_and_long_messages() {
    use teseladb_crypto::sha224::{Sha224, sha224};
    let n = run("SHA224ShortMsg.rsp", |m| sha224(m).to_vec())
        + run("SHA224LongMsg.rsp", |m| sha224(m).to_vec());
    assert!(n > 100);
    let mut h = Sha224::default();
    h.update(&[1; 70]);
    h.update(&[1; 70]);
    assert_eq!(h.finalize(), sha224(&[1; 140]));
}
