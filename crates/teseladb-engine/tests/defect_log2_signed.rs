#![cfg(target_os = "linux")]
//! `log2ceil` and `log2floor` accept every primitive integer. For a positive signed value
//! they must give the same result as for the same unsigned value: `leading_zeros` of a
//! positive signed value counts the sign bit, so the width they subtract from is the full
//! bit width of the type.

use teseladb::engine::core::bitops::{log2ceil, log2floor};

#[test]
fn signed_values_give_the_same_logarithms_as_unsigned_ones() {
    for n in [1u32, 2, 3, 7, 8, 9, 1000, 1 << 20, (1 << 30) + 1] {
        let ceil = log2ceil(n);
        let floor = log2floor(n);
        assert_eq!(log2ceil(n as i32), ceil, "log2ceil({n}i32)");
        assert_eq!(log2floor(n as i32), floor, "log2floor({n}i32)");
        assert_eq!(log2ceil(i64::from(n)), ceil, "log2ceil({n}i64)");
        assert_eq!(log2floor(i64::from(n)), floor, "log2floor({n}i64)");
        assert_eq!(log2ceil(n as isize), ceil, "log2ceil({n}isize)");
        assert_eq!(log2floor(i128::from(n)), floor, "log2floor({n}i128)");
    }
    assert_eq!(log2ceil(8i32), 3);
    assert_eq!(log2floor(8i32), 3);
    assert_eq!(log2ceil(5i8), 3);
    assert_eq!(log2floor(100i8), 6);
    assert_eq!(log2ceil(300i16), 9);
    assert_eq!(log2floor(i64::MAX), 62);
    assert_eq!(log2ceil(i32::MAX), 31);
}
