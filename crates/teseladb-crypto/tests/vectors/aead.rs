//! AES (CAVP KAT), AES-CTR (RFC 3686), AES-CBC, AES-GCM and ChaCha20-Poly1305 (Wycheproof).

use teseladb_crypto::aes::{Aes, InvalidKeyLength};
use teseladb_crypto::chacha20poly1305::{ChaCha20Poly1305, Poly1305};
use teseladb_crypto::modes::{AesGcm, DecryptError, cbc_decrypt, cbc_encrypt, ctr_xor};

use crate::common::{cavp, check, field, hex, wycheproof};

const ENCRYPT: &str = "ENCRYPT";

#[test]
fn aes_ecb_matches_the_cavp_known_answer_tests() {
    let mut n = 0;
    for kind in ["GFSbox", "KeySbox", "VarKey", "VarTxt"] {
        for bits in [128, 192, 256] {
            let file = format!("ECB{kind}{bits}.rsp");
            for (section, rec) in cavp(&file) {
                let aes = Aes::new(&hex(field(&rec, "KEY"))).expect("key");
                let mut pt = [0u8; 16];
                pt.copy_from_slice(&hex(field(&rec, "PLAINTEXT")));
                let mut ct = [0u8; 16];
                ct.copy_from_slice(&hex(field(&rec, "CIPHERTEXT")));
                let mut b = if section == ENCRYPT { pt } else { ct };
                if section == ENCRYPT {
                    aes.encrypt_block(&mut b);
                    assert_eq!(b, ct, "{file}");
                } else {
                    aes.decrypt_block(&mut b);
                    assert_eq!(b, pt, "{file}");
                }
                assert_eq!(aes.key_len() * 8, bits);
                n += 1;
            }
        }
    }
    assert!(n > 1000, "{n}");
}

#[test]
fn aes_rejects_keys_of_other_lengths() {
    for len in [0, 15, 17, 31, 33] {
        assert_eq!(Aes::new(&vec![0; len]).err(), Some(InvalidKeyLength));
    }
    assert!(format!("{:?}", Aes::new(&[0; 16]).expect("key")).contains("10 rounds"));
}

#[test]
fn aes_ctr_matches_rfc_3686_vector_2() {
    let aes = Aes::new(&hex("7e24067817fae0d743d6ce1f32539163")).expect("key");
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&hex("006cb6dbc0543b59da48d90b00000001"));
    let mut data: Vec<u8> = (0u8..32).collect();
    ctr_xor(&aes, &iv, &mut data);
    assert_eq!(data, hex("5104a106168a72d9790d41ee8edad388eb2e1efc46da57c8fce630df9141be28"));
}

#[test]
fn aes_ctr_carries_across_the_whole_128_bit_counter() {
    let aes = Aes::new(&[7; 32]).expect("key");
    let mut data = [0u8; 32];
    ctr_xor(&aes, &[0xff; 16], &mut data);
    let mut first = [0xff; 16];
    aes.encrypt_block(&mut first);
    let mut second = [0u8; 16];
    aes.encrypt_block(&mut second);
    assert_eq!(&data[..16], &first);
    assert_eq!(&data[16..], &second);
}

#[test]
fn aes_cbc_pkcs7_matches_wycheproof() {
    let n = wycheproof("aes_cbc_pkcs5_test.json", |c| {
        let aes = Aes::new(&c.hex("key")).expect("key");
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&c.hex("iv"));
        let (msg, ct) = (c.hex("msg"), c.hex("ct"));
        let dec = cbc_decrypt(&aes, &iv, &ct);
        check(c, dec.as_deref() == Ok(&msg[..]));
        if dec.is_ok() {
            assert_eq!(cbc_encrypt(&aes, &iv, &msg), ct, "tc {}", c.id());
        }
    });
    assert!(n > 100);
    let aes = Aes::new(&[1; 16]).expect("key");
    assert_eq!(cbc_decrypt(&aes, &[0; 16], &[]), Err(DecryptError));
    assert_eq!(cbc_decrypt(&aes, &[0; 16], &[0; 17]), Err(DecryptError));
}

#[test]
fn aes_gcm_matches_wycheproof() {
    let n = wycheproof("aes_gcm_test.json", |c| {
        let Ok(aes) = Aes::new(&c.hex("key")) else {
            check(c, false);
            return;
        };
        let gcm = AesGcm::new(aes);
        let (iv, aad, msg, ct, tag) =
            (c.hex("iv"), c.hex("aad"), c.hex("msg"), c.hex("ct"), c.hex("tag"));
        let mut sealed = ct.clone();
        sealed.extend_from_slice(&tag);
        let opened = gcm.open(&iv, &aad, &sealed);
        check(c, opened.as_deref() == Ok(&msg[..]));
        if opened.is_ok() {
            assert_eq!(gcm.seal(&iv, &aad, &msg), sealed, "tc {}", c.id());
        }
    });
    assert!(n > 200);
}

#[test]
fn aes_gcm_rejects_short_input_and_empty_nonces() {
    let gcm = AesGcm::new(Aes::new(&[3; 16]).expect("key"));
    assert_eq!(gcm.open(&[0; 12], &[], &[0; 15]), Err(DecryptError));
    let mut data = [0u8; 4];
    assert_eq!(gcm.open_in_place(&[], &[], &mut data, &[0; 16]), Err(DecryptError));
}

#[test]
fn chacha20_poly1305_matches_wycheproof() {
    let n = wycheproof("chacha20_poly1305_test.json", |c| {
        let (key, iv) = (c.hex("key"), c.hex("iv"));
        if key.len() != 32 || iv.len() != 12 {
            check(c, false);
            return;
        }
        let cipher = ChaCha20Poly1305::new(key.as_slice().try_into().expect("32"));
        let nonce: [u8; 12] = iv.as_slice().try_into().expect("12");
        let (aad, msg, ct, tag) = (c.hex("aad"), c.hex("msg"), c.hex("ct"), c.hex("tag"));
        let mut sealed = ct.clone();
        sealed.extend_from_slice(&tag);
        let opened = cipher.open(&nonce, &aad, &sealed);
        check(c, opened.as_deref() == Ok(&msg[..]));
        if opened.is_ok() {
            assert_eq!(cipher.seal(&nonce, &aad, &msg), sealed, "tc {}", c.id());
        }
    });
    assert!(n > 200);
    let cipher = ChaCha20Poly1305::new(&[0; 32]);
    assert_eq!(cipher.open(&[0; 12], &[], &[0; 15]), Err(DecryptError));
    assert_eq!(format!("{cipher:?}"), "ChaCha20Poly1305");
}

#[test]
fn poly1305_in_pieces_matches_one_shot_and_rfc_8439_section_2_5_2() {
    let key: [u8; 32] = hex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
        .try_into()
        .expect("32");
    let msg = b"Cryptographic Forum Research Group";
    let mut p = Poly1305::new(&key);
    p.update(&msg[..5]);
    p.update(&msg[5..21]);
    p.update(&msg[21..]);
    assert_eq!(p.finish().to_vec(), hex("a8061dc1305136c6c22b8baf0c0127a9"));
}
