//! ECDH and ECDSA on P-256, P-384 and P-521 against Wycheproof and RFC 6979.

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::ec::{
    EcPrivateKey, InvalidKey, NamedCurve, P256_RANDOM_LEN, ecdsa_sig_from_der, ecdsa_sig_to_der,
    ecdsa_verify, ecdsa_verify_digest,
};

use crate::common::{check, hex, wycheproof};

fn ecdh(file: &str, curve: NamedCurve) {
    let n = wycheproof(file, |c| {
        let res = EcPrivateKey::from_bytes(curve, &c.hex("private"))
            .and_then(|k| k.agree(&c.hex("public")));
        check(c, res.as_deref() == Ok(&c.hex("shared")[..]));
    });
    assert!(n > 200, "{file}");
}

#[test]
fn ecdh_matches_wycheproof_on_p256_p384_and_p521() {
    ecdh("ecdh_secp256r1_ecpoint_test.json", NamedCurve::P256);
    ecdh("ecdh_secp384r1_ecpoint_test.json", NamedCurve::P384);
    ecdh("ecdh_secp521r1_ecpoint_test.json", NamedCurve::P521);
}

fn ecdsa(file: &str, curve: NamedCurve, hash: HashAlgorithm) {
    let n = wycheproof(file, |c| {
        let pk = c.group_obj("publicKey");
        let public = hex(pk.get("uncompressed").and_then(|v| v.as_str()).expect("uncompressed"));
        let ok = ecdsa_sig_from_der(curve, &c.hex("sig"))
            .is_some_and(|sig| ecdsa_verify(curve, &public, hash, &c.hex("msg"), &sig));
        check(c, ok);
    });
    assert!(n > 200, "{file}");
}

#[test]
fn ecdsa_verification_matches_wycheproof_on_p256_p384_and_p521() {
    ecdsa("ecdsa_secp256r1_sha256_test.json", NamedCurve::P256, HashAlgorithm::Sha256);
    ecdsa("ecdsa_secp384r1_sha384_test.json", NamedCurve::P384, HashAlgorithm::Sha384);
    ecdsa("ecdsa_secp521r1_sha512_test.json", NamedCurve::P521, HashAlgorithm::Sha512);
}

/// RFC 6979, A.2.5 (P-256), A.2.6 (P-384) and A.2.7 (P-521), message "sample".
#[test]
fn ecdsa_signatures_match_rfc_6979_deterministic_nonces() {
    let cases = [
        (
            NamedCurve::P256,
            HashAlgorithm::Sha256,
            "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721",
            "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8",
        ),
        (
            NamedCurve::P384,
            HashAlgorithm::Sha384,
            "6b9d3dad2e1b8c1c05b19875b6659f4de23c3b667bf297ba9aa47740787137d896d5724e4c70a825f872c9ea60d2edf5",
            "94edbb92a5ecb8aad4736e56c691916b3f88140666ce9fa73d64c4ea95ad133c81a648152e44acf96e36dd1e80fabe4699ef4aeb15f178cea1fe40db2603138f130e740a19624526203b6351d0a3a94fa329c145786e679e7b82c71a38628ac8",
        ),
        (
            NamedCurve::P521,
            HashAlgorithm::Sha512,
            "00fad06daa62ba3b25d2fb40133da757205de67f5bb0018fee8c86e1b68c7e75caa896eb32f1f47c70855836a6d16fcc1466f6d8fbec67db89ec0c08b0e996b83538",
            "00c328fafcbd79dd77850370c46325d987cb525569fb63c5d3bc53950e6d4c5f174e25a1ee9017b5d450606add152b534931d7d4e8455cc91f9b15bf05ec36e377fa00617cce7cf5064806c467f678d3b4080d6f1cc50af26ca209417308281b68af282623eaa63e5b5c0723d8b8c37ff0777b1a20f8ccb1dccc43997f1ee0e44da4a67a",
        ),
    ];
    for (curve, hash, d, sig) in cases {
        let key = EcPrivateKey::from_bytes(curve, &hex(d)).expect("key");
        let got = key.sign_digest(hash, hash.hash(b"sample").as_ref());
        assert_eq!(got, hex(sig), "{curve:?}");
        assert_eq!(key.sign(b"sample"), got);
        let public = key.public_key();
        assert!(ecdsa_verify(curve, &public, hash, b"sample", &got));
        assert!(!ecdsa_verify(curve, &public, hash, b"other", &got));
        assert_eq!(ecdsa_sig_from_der(curve, &ecdsa_sig_to_der(&got)), Some(got.clone()));
        assert_eq!(key.public_key().len(), 1 + 2 * curve.scalar_len());
        assert_eq!(key.to_bytes(), hex(d));
        assert_eq!(key.curve(), curve);
        assert!(format!("{key:?}").contains('P'));
        assert!(!ecdsa_verify_digest(curve, &public, &[0; 32], &got[1..]));
    }
}

#[test]
fn private_keys_outside_one_to_n_minus_one_and_short_seeds_are_rejected() {
    assert_eq!(EcPrivateKey::from_bytes(NamedCurve::P256, &[0; 32]).err(), Some(InvalidKey));
    assert_eq!(EcPrivateKey::from_bytes(NamedCurve::P256, &[0xff; 32]).err(), Some(InvalidKey));
    assert_eq!(EcPrivateKey::from_random(NamedCurve::P256, &[1; 39]).err(), Some(InvalidKey));
    // The fixed-length P-256 derivation gives the same key as the checked one.
    let fixed = EcPrivateKey::p256_from_random(&[3; P256_RANDOM_LEN]);
    let checked = EcPrivateKey::from_random(NamedCurve::P256, &[3; P256_RANDOM_LEN]).expect("key");
    assert_eq!(fixed.public_key(), checked.public_key());
    let k = EcPrivateKey::from_random(NamedCurve::P384, &[0xa5; 56]).expect("key");
    let peer = EcPrivateKey::from_random(NamedCurve::P384, &[0x5a; 56]).expect("key");
    assert_eq!(k.agree(&peer.public_key()), peer.agree(&k.public_key()));
    assert_eq!(NamedCurve::P384.scalar_len(), 48);
    let k = EcPrivateKey::from_random(NamedCurve::P521, &[0xa5; 74]).expect("key");
    let peer = EcPrivateKey::from_random(NamedCurve::P521, &[0x5a; 132]).expect("key");
    assert_eq!(k.agree(&peer.public_key()), peer.agree(&k.public_key()));
    assert_eq!(NamedCurve::P521.scalar_len(), 66);
    assert_eq!(NamedCurve::P521.hash(), HashAlgorithm::Sha512);
    assert_eq!(EcPrivateKey::from_random(NamedCurve::P521, &[1; 73]).err(), Some(InvalidKey));
    // A scalar of 66 bytes whose top byte has bits above the 521st is not below n.
    assert_eq!(EcPrivateKey::from_bytes(NamedCurve::P521, &[0x02; 66]).err(), Some(InvalidKey));
}
