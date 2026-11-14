//! HMAC, HKDF and PBKDF2 against Wycheproof, and HMAC-SHA-1 PBKDF2 against RFC 6070.

use teseladb_crypto::hkdf::{self, OutputTooLong};
use teseladb_crypto::hmac::Hmac;
use teseladb_crypto::pbkdf2::pbkdf2;
use teseladb_crypto::sha512::{Sha384, Sha512};
use teseladb_crypto::{Digest, HashAlgorithm, Sha1, Sha256};

use crate::common::{check, hex, wycheproof};

fn hmac_file<D: Digest>(file: &str) {
    let n = wycheproof(file, |c| {
        let tag_len = usize::try_from(c.group_int("tagSize") / 8).expect("a tag size");
        let mut m = Hmac::<D>::new(&c.hex("key"));
        m.update(&c.hex("msg"));
        let full = m.clone().finish();
        let tag = c.hex("tag");
        let ok = full.as_ref()[..tag_len] == tag[..] && m.verify(&tag);
        check(c, ok);
    });
    assert!(n > 100, "{file}: {n}");
}

#[test]
fn hmac_matches_wycheproof_for_sha1_sha256_sha384_sha512() {
    hmac_file::<Sha1>("hmac_sha1_test.json");
    hmac_file::<Sha256>("hmac_sha256_test.json");
    hmac_file::<Sha384>("hmac_sha384_test.json");
    hmac_file::<Sha512>("hmac_sha512_test.json");
}

#[test]
fn hmac_rejects_tags_shorter_than_ten_bytes_or_longer_than_the_digest() {
    let m = Hmac::<Sha256>::new(b"k");
    let full = m.clone().finish();
    assert!(!m.clone().verify(&full.as_ref()[..9]));
    let mut long = full.as_ref().to_vec();
    long.push(0);
    assert!(!m.verify(&long));
}

fn hkdf_file<D: Digest>(file: &str) {
    let n = wycheproof(file, |c| {
        let mut out = vec![0u8; usize::try_from(c.int("size")).expect("a size")];
        let prk = hkdf::extract::<D>(&c.hex("salt"), &c.hex("ikm"));
        let res = hkdf::expand::<D>(prk.as_ref(), &c.hex("info"), &mut out);
        check(c, res.is_ok() && out == c.hex("okm"));
        if res.is_err() {
            assert_eq!(res, Err(OutputTooLong));
        }
    });
    assert!(n > 50, "{file}: {n}");
}

/// RFC 5869, test case 1, through the fixed-size expansion.
#[test]
fn hkdf_into_an_array_matches_rfc_5869() {
    let prk = hkdf::extract::<Sha256>(&hex("000102030405060708090a0b0c"), &[0x0b; 22]);
    let okm: [u8; 42] =
        hkdf::expand_array::<Sha256, 42>(prk.as_ref(), &hex("f0f1f2f3f4f5f6f7f8f9"));
    let want =
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865";
    assert_eq!(okm.to_vec(), hex(want));
}

#[test]
fn hkdf_matches_wycheproof_for_sha256_sha384_sha512() {
    hkdf_file::<Sha256>("hkdf_sha256_test.json");
    hkdf_file::<Sha384>("hkdf_sha384_test.json");
    hkdf_file::<Sha512>("hkdf_sha512_test.json");
}

fn pbkdf2_file<D: Digest>(file: &str) {
    let n = wycheproof(file, |c| {
        let mut out = vec![0u8; usize::try_from(c.int("dkLen")).expect("a key length")];
        pbkdf2::<D>(
            &c.hex("password"),
            &c.hex("salt"),
            u32::try_from(c.int("iterationCount")).expect("an iteration count"),
            &mut out,
        );
        check(c, out == c.hex("dk"));
    });
    assert!(n > 20, "{file}: {n}");
}

#[test]
fn pbkdf2_matches_wycheproof_for_sha1_sha256_sha512() {
    pbkdf2_file::<Sha1>("pbkdf2_hmacsha1_test.json");
    pbkdf2_file::<Sha256>("pbkdf2_hmacsha256_test.json");
    pbkdf2_file::<Sha512>("pbkdf2_hmacsha512_test.json");
}

#[test]
fn pbkdf2_sha1_matches_rfc_6070() {
    let cases: [(&[u8], &[u8], u32, &str); 3] = [
        (b"password", b"salt", 1, "0c60c80f961f0e71f3a9b524af6012062fe037a6"),
        (b"password", b"salt", 4096, "4b007901b765489abead49d926f721d065a429c1"),
        (
            b"passwordPASSWORDpassword",
            b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
            4096,
            "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038",
        ),
    ];
    for (p, s, c, dk) in cases {
        let want = hex(dk);
        let mut out = vec![0u8; want.len()];
        pbkdf2::<Sha1>(p, s, c, &mut out);
        assert_eq!(out, want);
    }
}

#[test]
fn hash_algorithm_dispatch_matches_the_typed_digests() {
    for alg in [
        HashAlgorithm::Sha1,
        HashAlgorithm::Sha224,
        HashAlgorithm::Sha256,
        HashAlgorithm::Sha384,
        HashAlgorithm::Sha512,
    ] {
        let one = alg.hash(b"abcdef");
        assert_eq!(one, alg.hash_parts(&[b"abc", b"def"]));
        assert_eq!(one.as_ref().len(), alg.output_len());
        assert_eq!(alg.hmac(b"key", b"abcdef"), alg.hmac_parts(b"key", &[b"ab", b"cdef"]));
        assert!(format!("{one:?}").contains(&alg.output_len().to_string()));
    }
    assert_eq!(<Sha384 as Digest>::ALGORITHM, HashAlgorithm::Sha384);
}
