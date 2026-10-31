//! Edge cases of the primitives that the published vectors do not reach.

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::chacha20poly1305::Poly1305;
use teseladb_crypto::ct::ct_eq;
use teseladb_crypto::curve25519::{Ed25519KeyPair, ed25519_verify};
use teseladb_crypto::ec::{
    EcPrivateKey, InvalidKey, NamedCurve, ecdsa_sig_from_der, ecdsa_sig_to_der, ecdsa_verify,
};
use teseladb_crypto::rsa::{RsaError, RsaPrivateKey, RsaPublicKey};
use teseladb_crypto::sha512::{Sha512, sha512};

/// A 1025-bit key made for this test (e = 65537): its PSS encoding is one byte shorter than the
/// modulus, so verification checks a leading zero byte.
const N_1025: &str = "00017850f8f0775ce6585c10f190febb94cb03f10e78620c4455d6cc80b9da6dfc7e1d9ff426fc734b4b9d95119c95f4086528a0c384d02fe28bfd9403114870b86d565bc734a5cd101ffe911bc03923af3cf4802c62b1521c063f01f1df808ccfe57c52c6de73a5ff12082261cc3c6a2a102a64789b265203e16a60e0b7d699abfd";
const D_1025: &str = "00014ab0a541f33280fd0ed298e558e49049a30a173434bd20e3e9e146118c6070ac86ee0408aa79caa60531750b8c3ea98df398ea97f0b6d3b73f3bed4227cd5e51f7616f45932162f64b6dc561ef82a56b449e65e295938aca4e6ae415765c18f5333693231efd273e9531695676801243e4b7c55a7954e9d949116824263c02c1";

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

#[test]
fn comparisons_of_different_lengths_are_unequal() {
    assert!(!ct_eq(b"ab", b"abc"));
    assert!(ct_eq(b"", b""));
}

#[test]
fn poly1305_and_sha512_buffer_short_updates() {
    let mut p = Poly1305::new(&[1; 32]);
    p.update(&[2; 3]);
    p.update(&[2; 3]);
    let mut q = Poly1305::new(&[1; 32]);
    q.update(&[2; 6]);
    assert_eq!(p.finish(), q.finish());
    let mut h = Sha512::default();
    h.update(b"ab");
    h.update(b"c");
    assert_eq!(h.finalize(), sha512(b"abc"));
}

#[test]
fn ed25519_rejects_public_keys_that_are_not_points() {
    let kp = Ed25519KeyPair::from_seed(&[4; 32]);
    let sig = kp.sign(b"m");
    assert!(!ed25519_verify(&[0xff; 32], b"m", &sig));
    let mut bad_r = sig;
    bad_r[..32].copy_from_slice(&[0xff; 32]);
    assert!(!ed25519_verify(&kp.public_key(), b"m", &bad_r));
}

#[test]
fn ec_rejects_malformed_points_scalars_and_signatures() {
    let key = EcPrivateKey::from_bytes(NamedCurve::P256, &[7; 32]).expect("key");
    let mut public = key.public_key();
    public[0] = 5;
    assert_eq!(key.agree(&public), Err(InvalidKey));
    let mut wide = vec![1u8];
    wide.extend_from_slice(&[7; 32]);
    assert_eq!(EcPrivateKey::from_bytes(NamedCurve::P256, &wide).err(), Some(InvalidKey));
    assert_eq!(EcPrivateKey::from_random(NamedCurve::P256, &[1; 65]).err(), Some(InvalidKey));
    let public = key.public_key();
    for i in 0..64u8 {
        let msg = [i; 3];
        let sig = key.sign(&msg);
        let der = ecdsa_sig_to_der(&sig);
        assert_eq!(ecdsa_sig_from_der(NamedCurve::P256, &der), Some(sig.clone()));
        assert!(ecdsa_verify(NamedCurve::P256, &public, HashAlgorithm::Sha256, &msg, &sig));
    }
    let sig = key.sign(b"x");
    let mut der = ecdsa_sig_to_der(&sig);
    der.push(0);
    assert_eq!(ecdsa_sig_from_der(NamedCurve::P256, &der), None);
    let der = ecdsa_sig_to_der(&sig);
    let mut extra = der.clone();
    extra[1] += 1;
    extra.push(0);
    assert_eq!(ecdsa_sig_from_der(NamedCurve::P256, &extra), None);
    for bad in [
        &[0x30, 0x81, 0x02, 0x02, 0x00][..],
        &[0x30, 0x02, 0x02, 0x00],
        &[0x31, 0x00],
        &[0x30, 0x04, 0x02, 0x01, 0x80, 0x02],
    ] {
        assert_eq!(ecdsa_sig_from_der(NamedCurve::P256, bad), None);
    }
    let too_long =
        [&[0x30, 0x46, 0x02, 0x21, 0x01][..], &[0x11; 32], &[0x02, 0x21, 0x00], &[0x80; 32]]
            .concat();
    assert_eq!(ecdsa_sig_from_der(NamedCurve::P256, &too_long), None);
}

#[test]
fn rsa_keys_and_paddings_reject_what_does_not_fit() {
    let n = hex(N_1025);
    let key = RsaPrivateKey::new(&n, &[1, 0, 1], &hex(D_1025)).expect("key");
    let public = key.public_key();
    assert_eq!(public.modulus_bits(), 1025);
    for i in 0..8u8 {
        let sig = key.sign_pss(HashAlgorithm::Sha256, &[i], &[i; 20]).expect("sign");
        assert!(public.verify_pss(HashAlgorithm::Sha256, &[i], 20, &sig));
    }
    let mut big = vec![0u8; 129];
    big[0] = 1;
    big[128] = 2;
    for i in 0..8u8 {
        big[64] = i;
        assert!(!public.verify_pss(HashAlgorithm::Sha256, b"m", 20, &big));
        assert!(!public.verify_pss(HashAlgorithm::Sha512, b"m", 64, &big));
    }
    assert_eq!(key.sign_pss_digest(HashAlgorithm::Sha256, &[0; 31], &[]).err(), Some(RsaError));
    assert_eq!(key.sign_pss(HashAlgorithm::Sha512, b"m", &[0; 64]).err(), Some(RsaError));
    assert_eq!(key.sign_pkcs1v15_digest(HashAlgorithm::Sha256, &[0; 31]).err(), Some(RsaError));
    assert!(!public.verify_pss_digest(HashAlgorithm::Sha256, &[0; 31], 20, &big));
    assert_eq!(RsaPrivateKey::new(&n, &[1, 0, 1], &n).err(), Some(RsaError));
    let wrong_d = RsaPrivateKey::new(&n, &[1, 0, 1], &[3; 100]).expect("key");
    assert_eq!(wrong_d.sign_pkcs1v15(HashAlgorithm::Sha256, b"m").err(), Some(RsaError));
    let mut e9 = vec![1u8];
    e9.extend_from_slice(&[0; 7]);
    e9.push(1);
    assert!(RsaPublicKey::new(&n, &e9).is_ok());
    assert_eq!(RsaPublicKey::new(&n, &[1; 140]).err(), Some(RsaError));
    let mut huge = vec![0xffu8; 1100];
    huge[0] = 0x80;
    assert_eq!(RsaPublicKey::new(&huge, &[3]).err(), Some(RsaError));
}
