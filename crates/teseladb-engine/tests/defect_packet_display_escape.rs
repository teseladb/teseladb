#![cfg(target_os = "linux")]
//! A packet fragment printed as text writes a control byte other than `\r`, `\n` and `\t`
//! as `\x` and its two hexadecimal digits.

use teseladb::engine::net::packet::Packet;

#[test]
fn control_bytes_are_escaped_in_hexadecimal() {
    let p = Packet::from_static_data(&[0x41, 0x0b, 0x1f, 0x7f]);
    assert_eq!(format!("{p}"), "packet{\"A\\x0b\\x1f\\x7f\"}");
}
