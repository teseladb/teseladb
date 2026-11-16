#![cfg(target_os = "linux")]
//! `convert_decimal_safe(buf, bufsz, n)` writes the decimal digits of `n` at the start of
//! `buf`, using at most `bufsz` bytes, and returns their number. The digits do not depend on
//! `bufsz`, as long as they fit in it.

use teseladb::engine::util::print_safe::convert_decimal_safe;

#[test]
fn the_digits_do_not_depend_on_the_room_given() {
    let mut buf = [0xffu8; 64];
    for bufsz in [2usize, 3, 10, 16, 20, 24, 30, 48, 64] {
        let n = convert_decimal_safe(&mut buf, bufsz, 12u64);
        assert_eq!(&buf[..n], b"12", "bufsz {bufsz}");
    }
    for bufsz in [7usize, 12, 40] {
        let n = convert_decimal_safe(&mut buf, bufsz, 1234567u32);
        assert_eq!(&buf[..n], b"1234567", "bufsz {bufsz}");
    }
    let n = convert_decimal_safe(&mut buf, 20, u64::MAX);
    assert_eq!(&buf[..n], u64::MAX.to_string().as_bytes());
    let n = convert_decimal_safe(&mut buf, 1, 0u8);
    assert_eq!(&buf[..n], b"0");
}
