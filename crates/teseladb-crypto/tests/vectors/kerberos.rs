//! Kerberos AES: n-fold (RFC 3961, A.1), CBC-CTS and string-to-key (RFC 3962, appendix B) and
//! the RFC 8009 key derivation and encryption examples (appendix A).

use teseladb_crypto::aes::Aes;
use teseladb_crypto::kerberos::{EncType, n_fold};
use teseladb_crypto::modes::{DecryptError, cts_decrypt, cts_encrypt};

use crate::common::hex;

#[test]
fn n_fold_matches_rfc_3961() {
    let cases: [(&[u8], usize, &str); 6] = [
        (b"012345", 8, "be072631276b1955"),
        (b"password", 7, "78a07b6caf85fa"),
        (b"Rough Consensus, and Running Code", 8, "bb6ed30870b7f0e0"),
        (b"password", 21, "59e4a8ca7c0385c3c37b3f6d2000247cb6e6bd5b3e"),
        (
            b"MASSACHVSETTS INSTITVTE OF TECHNOLOGY",
            24,
            "db3b0d8f0b061e603282b308a50841229ad798fab9540c1b",
        ),
        (b"Q", 21, "518a54a215a8452a518a54a215a8452a518a54a215"),
    ];
    for (input, len, want) in cases {
        assert_eq!(n_fold(input, len), hex(want));
    }
}

#[test]
fn cbc_cts_matches_rfc_3962_appendix_b() {
    let aes = Aes::new(b"chicken teriyaki").expect("key");
    let cases = [
        ("4920776f756c64206c696b652074686520", "c6353568f2bf8cb4d8a580362da7ff7f97"),
        (
            "4920776f756c64206c696b65207468652047656e6572616c20476175277320",
            "fc00783e0efdb2c1d445d4c8eff7ed2297687268d6ecccc0c07b25e25ecfe5",
        ),
        (
            "4920776f756c64206c696b65207468652047656e6572616c2047617527732043",
            "39312523a78662d5be7fcbcc98ebf5a897687268d6ecccc0c07b25e25ecfe584",
        ),
        (
            "4920776f756c64206c696b65207468652047656e6572616c20476175277320436869636b656e2c20706c656173652c",
            "97687268d6ecccc0c07b25e25ecfe584b3fffd940c16a18c1b5549d2f838029e39312523a78662d5be7fcbcc98ebf5",
        ),
        (
            "4920776f756c64206c696b65207468652047656e6572616c20476175277320436869636b656e2c20706c656173652c20",
            "97687268d6ecccc0c07b25e25ecfe5849dad8bbb96c4cdc03bc103e1a194bbd839312523a78662d5be7fcbcc98ebf5a8",
        ),
        (
            "4920776f756c64206c696b65207468652047656e6572616c20476175277320436869636b656e2c20706c656173652c20616e6420776f6e746f6e20736f75702e",
            "97687268d6ecccc0c07b25e25ecfe58439312523a78662d5be7fcbcc98ebf5a84807efe836ee89a526730dbc2f7bc8409dad8bbb96c4cdc03bc103e1a194bbd8",
        ),
    ];
    for (input, output) in cases {
        let (p, c) = (hex(input), hex(output));
        assert_eq!(cts_encrypt(&aes, &[0; 16], &p), Ok(c.clone()));
        assert_eq!(cts_decrypt(&aes, &[0; 16], &c), Ok(p));
    }
    let one = cts_encrypt(&aes, &[0; 16], &[7; 16]).expect("one block");
    assert_eq!(cts_decrypt(&aes, &[0; 16], &one), Ok(vec![7; 16]));
    assert_eq!(cts_encrypt(&aes, &[0; 16], &[0; 15]), Err(DecryptError));
    assert_eq!(cts_decrypt(&aes, &[0; 16], &[0; 15]), Err(DecryptError));
}

#[test]
fn string_to_key_matches_rfc_3962_and_rfc_8009() {
    let salt = b"ATHENA.MIT.EDUraeburn";
    assert_eq!(
        EncType::Aes128CtsHmacSha1.string_to_key(b"password", salt, 1),
        hex("42263c6e89f4fc28b8df68ee09799f15")
    );
    assert_eq!(
        EncType::Aes256CtsHmacSha1.string_to_key(b"password", salt, 1),
        hex("fe697b52bc0d3ce14432ba036a92e65bbb52280990a2fa27883998d72af30161")
    );
    assert_eq!(
        EncType::Aes128CtsHmacSha1.string_to_key(b"password", salt, 2),
        hex("c651bf29e2300ac27fa469d693bdda13")
    );
    let mut salt8009 = hex("10df9dd783e5bc8acea1730e74355f61");
    salt8009.extend_from_slice(salt);
    let iters = EncType::Aes128CtsHmacSha256.default_iterations();
    assert_eq!(iters, 32_768);
    assert_eq!(
        EncType::Aes128CtsHmacSha256.string_to_key(b"password", &salt8009, iters),
        hex("089bca48b105ea6ea77ca5d2f39dc5e7")
    );
    assert_eq!(
        EncType::Aes256CtsHmacSha384.string_to_key(b"password", &salt8009, iters),
        hex("45bd806dbf6a833a9cffc1c94589a222367a79bc21c413718906e9f578a78467")
    );
    assert_eq!(EncType::Aes256CtsHmacSha1.default_iterations(), 4096);
}

#[test]
fn rfc_8009_key_derivation_and_encryption_examples() {
    let base128 = hex("3705d96080c17728a0e800eab6e0d23c");
    let e = EncType::Aes128CtsHmacSha256;
    assert_eq!(e.checksum_key(&base128, 2).expect("key"), hex("b31a018a48f54776f403e9a396325dc3"));
    assert_eq!(
        e.encryption_key(&base128, 2).expect("key"),
        hex("9b197dd1e8c5609d6e67c3e37c62c72e")
    );
    assert_eq!(e.integrity_key(&base128, 2).expect("key"), hex("9fda0e56ab2d85e1569a688696c26a6c"));
    let base256 = hex("6d404d37faf79f9df0d33568d320669800eb4836472ea8a026d16b7182460c52");
    let e256 = EncType::Aes256CtsHmacSha384;
    assert_eq!(
        e256.checksum_key(&base256, 2).expect("key"),
        hex("ef5718be86cc84963d8bbb5031e9f5c4ba41f28faf69e73d")
    );
    assert_eq!(
        e256.encryption_key(&base256, 2).expect("key"),
        hex("56ab22bee63d82d7bc5227f6773f8ea7a5eb1c825160c38312980c442e5c7e49")
    );
    assert_eq!(
        e256.integrity_key(&base256, 2).expect("key"),
        hex("69b16514e3cd8e56b82010d5c73012b622c4d00ffc23ed1f")
    );
    let cases = [
        (
            "",
            "7e5895eaf2672435bad817f545a37148",
            "ef85fb890bb8472f4dab20394dca781dad877eda39d50c870c0d5a0a8e48c718",
        ),
        (
            "000102030405",
            "7bca285e2fd4130fb55b1a5c83bc5b24",
            "84d7f30754ed987bab0bf3506beb09cfb55402cef7e6877ce99e247e52d16ed4421dfdf8976c",
        ),
        (
            "000102030405060708090a0b0c0d0e0f",
            "56ab21713ff62c0a1457200f6fa9948f",
            "3517d640f50ddc8ad3628722b3569d2ae07493fa8263254080ea65c1008e8fc295fb4852e7d83e1e7c48c37eebe6b0d3",
        ),
    ];
    for (plain, conf, cipher) in cases {
        let conf: [u8; 16] = hex(conf).try_into().expect("16");
        let c = e.encrypt(&base128, 2, &conf, &hex(plain)).expect("encrypt");
        assert_eq!(c, hex(cipher));
        assert_eq!(e.decrypt(&base128, 2, &c), Ok(hex(plain)));
        let mut bad = c.clone();
        bad[0] ^= 1;
        assert_eq!(e.decrypt(&base128, 2, &bad), Err(DecryptError));
    }
}

#[test]
fn rfc_3962_profiles_round_trip_and_reject_tampering() {
    for e in [EncType::Aes128CtsHmacSha1, EncType::Aes256CtsHmacSha1, EncType::Aes256CtsHmacSha384]
    {
        let base = vec![0x42; e.key_len()];
        let c = e.encrypt(&base, 7, &[9; 16], b"gssapi token").expect("encrypt");
        assert_eq!(c.len(), 16 + 12 + e.mac_len());
        assert_eq!(e.decrypt(&base, 7, &c), Ok(b"gssapi token".to_vec()));
        let mut bad = c.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert_eq!(e.decrypt(&base, 7, &bad), Err(DecryptError));
        assert_eq!(e.decrypt(&base, 7, &c[..20]), Err(DecryptError));
        assert_eq!(e.decrypt(&[1; 5], 7, &c), Err(DecryptError));
        assert!(e.encrypt(&[1; 5], 7, &[0; 16], b"x").is_err());
        assert!(e.checksum_key(&[1; 5], 7).is_err());
        assert!((17..=20).contains(&e.number()));
    }
    assert_eq!(EncType::Aes128CtsHmacSha256.number(), 19);
}
