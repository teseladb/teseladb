//! RSASSA-PKCS1-v1_5 and RSASSA-PSS against Wycheproof.

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::rsa::{RsaError, RsaPrivateKey, RsaPublicKey};

use crate::common::{Case, check, hex, wycheproof};

const SHA1: &str = "SHA-1";
const SHA224: &str = "SHA-224";
const SHA256: &str = "SHA-256";
const SHA384: &str = "SHA-384";
const SHA512: &str = "SHA-512";

fn hash_of(name: &str) -> HashAlgorithm {
    match name {
        SHA1 => HashAlgorithm::Sha1,
        SHA224 => HashAlgorithm::Sha224,
        SHA256 => HashAlgorithm::Sha256,
        SHA384 => HashAlgorithm::Sha384,
        SHA512 => HashAlgorithm::Sha512,
        other => panic!("hash {other}"),
    }
}

fn public_of(c: &Case<'_>) -> Option<RsaPublicKey> {
    let pk = c.group_obj("publicKey");
    let get = |k: &str| hex(pk.get(k).and_then(|v| v.as_str()).expect(k));
    RsaPublicKey::new(&get("modulus"), &get("publicExponent")).ok()
}

#[test]
fn pkcs1v15_verification_matches_wycheproof() {
    let n = wycheproof("rsa_signature_2048_sha256_test.json", |c| {
        let hash = hash_of(c.group_str("sha"));
        let ok =
            public_of(c).is_some_and(|k| k.verify_pkcs1v15(hash, &c.hex("msg"), &c.hex("sig")));
        check(c, ok);
    });
    assert!(n > 200);
}

#[test]
fn pss_verification_matches_wycheproof() {
    let n = wycheproof("rsa_pss_2048_sha256_mgf1_32_test.json", |c| {
        let hash = hash_of(c.group_str("sha"));
        assert_eq!(hash_of(c.group_str("mgfSha")), hash);
        let salt = c.group_int("sLen") as usize;
        let ok =
            public_of(c).is_some_and(|k| k.verify_pss(hash, &c.hex("msg"), salt, &c.hex("sig")));
        check(c, ok);
    });
    assert!(n > 100);
}

#[test]
fn pkcs1v15_signatures_match_wycheproof_generation_vectors() {
    let n = wycheproof("rsa_pkcs1_2048_sig_gen_test.json", |c| {
        let hash = hash_of(c.group_str("sha"));
        let pk = c.group_obj("privateKey");
        let get = |k: &str| hex(pk.get(k).and_then(|v| v.as_str()).expect(k));
        let key =
            RsaPrivateKey::new(&get("modulus"), &get("publicExponent"), &get("privateExponent"))
                .expect("key");
        let sig = key.sign_pkcs1v15(hash, &c.hex("msg"));
        check(c, sig.as_deref() == Ok(&c.hex("sig")[..]));
        let public = key.public_key();
        assert_eq!(
            public.modulus(),
            get("modulus").into_iter().skip_while(|&b| b == 0).collect::<Vec<_>>()
        );
        assert_eq!(
            public.exponent(),
            get("publicExponent").into_iter().skip_while(|&b| b == 0).collect::<Vec<_>>()
        );
        let salt = [0x5a; 32];
        let pss = key.sign_pss(HashAlgorithm::Sha256, b"m", &salt).expect("pss");
        assert!(public.verify_pss(HashAlgorithm::Sha256, b"m", 32, &pss));
        assert!(!public.verify_pss(HashAlgorithm::Sha256, b"m", 31, &pss));
        assert!(!public.verify_pss(HashAlgorithm::Sha256, b"x", 32, &pss));
        assert_eq!(public.modulus_bits(), 2048);
        assert_eq!(public.modulus_len(), 256);
        assert!(format!("{key:?}").contains("2048"));
    });
    assert!(n >= 8);
}

#[test]
fn malformed_keys_and_inputs_are_rejected() {
    let mut n = vec![0xffu8; 128];
    assert_eq!(RsaPublicKey::new(&n, &[1, 0, 1]).err(), None);
    assert_eq!(RsaPublicKey::new(&n[..100], &[1, 0, 1]).err(), Some(RsaError));
    assert_eq!(RsaPublicKey::new(&n, &[2]).err(), Some(RsaError));
    assert_eq!(RsaPublicKey::new(&n, &[1]).err(), Some(RsaError));
    n[127] = 0xfe;
    assert_eq!(RsaPublicKey::new(&n, &[1, 0, 1]).err(), Some(RsaError));
    n[127] = 0xff;
    assert_eq!(RsaPrivateKey::new(&n, &[3], &[0xff; 129]).err(), Some(RsaError));
    let k = RsaPublicKey::new(&n, &[3]).expect("key");
    assert!(!k.verify_pkcs1v15(HashAlgorithm::Sha256, b"m", &[0; 127]));
    assert!(!k.verify_pkcs1v15(HashAlgorithm::Sha256, b"m", &[0xff; 128]));
    assert!(!k.verify_pkcs1v15_digest(HashAlgorithm::Sha256, &[0; 31], &[1; 128]));
    assert!(!k.verify_pss_digest(HashAlgorithm::Sha512, &[0; 64], 64, &[1; 128]));
}
