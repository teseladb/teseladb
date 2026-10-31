//! Adler-32: reference values (RFC 1950) and incremental mode.
mod common;

use teseladb_hash::{Adler32, adler32, portable};

#[test]
fn reference_values() {
    assert_eq!(adler32(b""), 1);
    assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    for &(n, _, want, ..) in common::CASES {
        assert_eq!(adler32(&common::pattern(n)), want, "length {n}");
    }
    // Worst case for overflow: all 0xff over several NMAX blocks.
    let ones = vec![0xffu8; 100_000];
    let mut a: u64 = 1;
    let mut b: u64 = 0;
    for &x in &ones {
        a = (a + u64::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    assert_eq!(adler32(&ones), ((b << 16) | a) as u32);
}

#[test]
fn incremental_equals_one_shot() {
    let d = common::pattern(20_000);
    for cut in [0, 1, 7, 5552, 5553, 19_999, 20_000] {
        let mut s = Adler32::default();
        s.update(&d[..cut]);
        s.update(&d[cut..]);
        assert_eq!(s.value(), adler32(&d));
        assert_eq!(portable::adler32(portable::adler32(1, &d[..cut]), &d[cut..]), adler32(&d));
    }
    assert_eq!(Adler32::new().value(), 1);
}
