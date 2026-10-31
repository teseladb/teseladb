//! TDES (SP 800-67) against NIST CAVP: the ECB and CBC known-answer tests (single-key keying
//! option 3) and the multi-block tests with two and three keys (013 D-065, FR-018).

use teseladb_crypto::des::{TripleDes, cbc_decrypt_blocks, cbc_encrypt_blocks};

use crate::common::{cavp, field, hex};

fn key(rec: &[(String, String)]) -> Vec<u8> {
    match rec.iter().find(|(k, _)| k == "KEYs") {
        Some((_, k)) => hex(&k.repeat(3)),
        None => hex(&format!("{}{}{}", field(rec, "KEY1"), field(rec, "KEY2"), field(rec, "KEY3"))),
    }
}

#[test]
fn tdes_ecb_and_cbc_match_cavp() {
    let mut n = 0;
    for mode in ["ECB", "CBC"] {
        let files = ["invperm", "permop", "subtab", "varkey", "vartext", "MMT2", "MMT3"]
            .map(|f| format!("T{mode}{f}.rsp"));
        for file in files {
            for (section, rec) in cavp(&file) {
                let k = TripleDes::new(&key(&rec)).unwrap();
                let (pt, ct) = (hex(field(&rec, "PLAINTEXT")), hex(field(&rec, "CIPHERTEXT")));
                let iv: [u8; 8] =
                    if mode == "CBC" { hex(field(&rec, "IV")).try_into().unwrap() } else { [0; 8] };
                let (input, want) = if section == "ENCRYPT" { (&pt, &ct) } else { (&ct, &pt) };
                let got = match (mode, section.as_str()) {
                    ("CBC", "ENCRYPT") => cbc_encrypt_blocks(&k, &iv, input),
                    ("CBC", _) => cbc_decrypt_blocks(&k, &iv, input),
                    (_, s) => input
                        .chunks(8)
                        .flat_map(|c| {
                            let mut b: [u8; 8] = c.try_into().unwrap();
                            if s == "ENCRYPT" {
                                k.encrypt_block(&mut b)
                            } else {
                                k.decrypt_block(&mut b)
                            }
                            b
                        })
                        .collect(),
                };
                assert_eq!(&got, want, "{file} {section} {:?}", field(&rec, "COUNT"));
                n += 1;
            }
        }
    }
    assert_eq!(n, 1000);
}
