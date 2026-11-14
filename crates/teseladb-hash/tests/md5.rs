//! MD5 (RFC 1321) against the test suite of its appendix A.5, and across block boundaries.

use std::fmt::Write as _;

use teseladb_hash::md5;

fn hex(b: [u8; 16]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

#[test]
fn the_rfc_1321_test_suite() {
    let suite = [
        ("", "d41d8cd98f00b204e9800998ecf8427e"),
        ("a", "0cc175b9c0f1b6a831c399e269772661"),
        ("abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            "d174ab98d277d9f5a5611c2c9f419d9f",
        ),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ];
    for (input, digest) in suite {
        assert_eq!(hex(md5(input.as_bytes())), digest, "{input:?}");
    }
}

#[test]
fn lengths_around_the_padding_boundary() {
    // 55 bytes fit one block with the length; 56 need a second one.
    assert_eq!(hex(md5(&[b'a'; 55])), "ef1772b6dff9a122358552954ad0df65");
    assert_eq!(hex(md5(&[b'a'; 56])), "3b0c8ac703f828b04c6c197006d17218");
    assert_eq!(hex(md5(&[b'a'; 64])), "014842d480b571495a4a0363793f7367");
}
