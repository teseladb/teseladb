//! Tests of `Sstring` and `BasicSstring`: construction, comparison, search, substrings,
//! appends and edits, NUL termination, `resize_and_overwrite` and formatting.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::OutOfRange;
use teseladb::engine::core::sstring::{
    BasicSstring, Sstring, make_sstring, to_sstring, uninitialized_string,
};

fn s(x: &str) -> Sstring {
    Sstring::from(x)
}

const NPOS: usize = Sstring::NPOS;

#[test]
fn test_make_sstring() {
    let foo: &str = "foo";
    let bar: String = "bar".to_string();
    let zed = s("zed");
    let baz: &str = "baz";
    assert_eq!(make_sstring(&[foo, &bar, zed.as_str(), baz, "bah"]), s("foobarzedbazbah"));
}

#[test]
fn test_construction() {
    assert_eq!(Sstring::from(String::from("abc").as_str()), s("abc"));
}

#[test]
fn test_equality() {
    assert_eq!(s("aaa"), s("aaa"));
}

#[test]
fn test_to_sstring() {
    assert_eq!(to_sstring(&1234567), s("1234567"));
}

#[test]
fn test_add_literal_to_sstring() {
    assert_eq!(&s("x") + &s("y"), s("xy"));
}

#[test]
fn test_front() {
    let mut st = s("abcde");
    assert_eq!(*st.front(), b'a');
    assert_eq!(st.as_bytes()[0], b'a');
}

#[test]
fn test_find_sstring() {
    assert_eq!(s("abcde").find_byte(b'b', 0), 1usize);
    assert_eq!(s("babcde").find_byte(b'b', 1), 2usize);
}

#[test]
fn test_find_sstring_compatible() {
    let check_find = |s1: &str, s2: &str, pos: usize| {
        let xpos_ss = s(s1).find(s2.as_bytes(), pos);
        let xpos_std = if pos <= s1.len() { s1[pos..].find(s2).map(|p| p + pos) } else { None };

        // verify that `str` really has the same behavior as we just tested for sstring
        if xpos_ss == NPOS {
            // sstring::NPOS stands for "not found", which `str` gives as `None`
            assert_eq!(xpos_std, None);
        } else {
            assert_eq!(Some(xpos_ss), xpos_std);
        }
    };

    check_find("", "", 0);
    check_find("", "", 1);
    check_find("abcde", "", 0);
    check_find("abcde", "", 1);
    check_find("abcde", "", 5);
    check_find("abcde", "", 6);
}

#[test]
fn test_not_find_sstring() {
    assert_eq!(s("abcde").find_byte(b'x', 0), NPOS);
}

#[test]
fn test_str_find_sstring() {
    assert_eq!(s("abcde").find_str("bc", 0), 1usize);
    assert_eq!(s("abcbcde").find_str("bc", 2), 3usize);
    assert_eq!(s("abcde").find_str("abcde", 0), 0usize);
    assert_eq!(s("abcde").find_str("", 5), 5usize);
    assert_eq!(s("ababcbdbef").find_str("bef", 0), 7usize);
    assert_eq!(s("").find_str("", 0), 0usize);
}

#[test]
fn test_str_not_find_sstring() {
    assert_eq!(s("abcde").find_str("x", 0), NPOS);
    assert_eq!(s("abcdefg").find_str("cde", 6), NPOS);
    assert_eq!(s("abcdefg").find_str("cde", 4), NPOS);
    assert_eq!(s("ababcbdbe").find_str("bcd", 0), NPOS);
    assert_eq!(s("").find_str("", 1), NPOS);
    assert_eq!(s("abc").find_str("abcde", 0), NPOS);
}

#[test]
fn test_str_starts_with() {
    assert!(String::from("abcdefg").starts_with("abcdefg"));
    assert!(s("abcdefg").starts_with(b"abcdefg"));
    assert!(s("abcdefg").starts_with(b"ab"));
    assert!(s("abcde").starts_with_byte(b'a'));
    assert!(s("abcde").starts_with(b"ab"));
    assert!(s("abcdefg").starts_with(b""));
    assert!(!s("abcde").starts_with(b"cde"));
    assert!(!s("abcde").starts_with_byte(b'b'));
    assert!(!s("abcdefg").starts_with(b"cde"));
    assert!(!s("abcdefg").starts_with(b"ab\0"));
}

#[test]
fn test_str_ends_with() {
    assert!(String::from("abcdefg").ends_with("abcdefg"));
    assert!(s("abcdefg").ends_with(b"abcdefg"));
    assert!(s("abcdefg").ends_with(b"efg"));
    assert!(s("abcde").ends_with_byte(b'e'));
    assert!(s("abcde").ends_with(b"de"));
    assert!(s("abcdefg").ends_with(b""));
    assert!(!s("abcde").ends_with(b"abc"));
    assert!(!s("abcde").ends_with_byte(b'b'));
    assert!(!s("abcdefg").ends_with(b"abc"));
    assert!(!s("abcdefg").ends_with(b"efg\0"));
}

#[test]
fn test_str_contains() {
    assert!(s("abcde").starts_with(b"ab"));
    assert!(s("abcde").starts_with_byte(b'a'));
    assert!(s("abcde").starts_with(b"ab"));
    assert!(s("abcde").starts_with(b""));
    assert!(s("abcde").contains(b"bc"));
    assert!(s("abcde").contains(b"bc"));
    assert!(s("abcde").contains_byte(b'c'));
    assert!(s("abcde").contains(b"de"));
    assert!(s("abcde").contains(b"de"));
    assert!(!s("abcde").contains(b"bad"));
    assert!(!s("abcde").contains(b"bce"));
    assert!(!s("abcde").contains(b"x"));
    assert!(!s("abcde").contains_byte(b'x'));
    assert!(!s("abcde").contains(b"ab\0"));
    assert!(!s("abcde").contains(b"bc\0"));
    assert!(!s("abcde").contains(b"de\0"));
}

#[test]
fn test_substr_sstring() {
    assert_eq!(s("abcde").substr(1, 2).expect("substr"), "bc");
    assert_eq!(s("abc").substr(1, 2).expect("substr"), "bc");
    assert_eq!(s("abc").substr(1, 3).expect("substr"), "bc");
    assert_eq!(s("abc").substr(0, 2).expect("substr"), "ab");
    assert_eq!(s("abc").substr(3, 2).expect("substr"), "");
    assert_eq!(s("abc").substr(1, NPOS).expect("substr"), "bc");
}

#[test]
fn test_substr_eor_sstring() {
    assert!(matches!(s("abcde").substr(6, 1), Err(ref e) if e.is::<OutOfRange>()));
}

#[test]
fn test_at_sstring() {
    assert_eq!(s("abcde").at(1).expect("at"), b'b');
    assert!(matches!(s("abcde").at(6), Err(ref e) if e.is::<OutOfRange>()));
    let mut st = s("abcde");
    st.as_bytes_mut()[1] = b'd';
    assert_eq!(st, "adcde");
}

#[test]
fn test_find_last_sstring() {
    assert_eq!(s("ababa").find_last_of(b'a', NPOS), 4usize);
    assert_eq!(s("ababa").find_last_of(b'a', 5), 4usize);
    assert_eq!(s("ababa").find_last_of(b'a', 4), 4usize);
    assert_eq!(s("ababa").find_last_of(b'a', 3), 2usize);
    assert_eq!(s("ababa").find_last_of(b'x', NPOS), NPOS);
    assert_eq!(s("").find_last_of(b'a', NPOS), NPOS);
}

#[test]
fn test_append() {
    assert_eq!(*s("aba").append_bytes(&b"1234"[..3]), "aba123");
    assert_eq!(*s("aba").append_bytes(&b"1234"[..4]), "aba1234");
    assert_eq!(*s("aba").append_bytes(&b"1234"[..0]), "aba");
}

#[test]
fn test_replace() {
    assert_eq!(*s("abc").replace(1, 1, &b"xyz"[..1]).expect("replace"), "axc");
    assert_eq!(*s("abc").replace(3, 2, &b"xyz"[..2]).expect("replace"), "abcxy");
    assert_eq!(*s("abc").replace(2, 2, &b"xyz"[..2]).expect("replace"), "abxy");
    assert_eq!(*s("abc").replace(0, 2, &b""[..0]).expect("replace"), "c");
    assert!(matches!(s("abc").replace(4, 1, &b"xyz"[..1]), Err(ref e) if e.is::<OutOfRange>()));
    let sp = b"xyz";
    let mut str = s("abcdef");
    assert_eq!(*str.replace_range(1, 3, &sp[1..3]).expect("replace"), "ayzdef");
    assert!(matches!(s("abc").replace(4, 1, &b"xyz"[..1]), Err(ref e) if e.is::<OutOfRange>()));
}

#[test]
fn test_insert() {
    let mut str = s("abc");
    let sp = b"xyz";
    str.insert(1, &sp[1..2]).expect("insert");
    assert_eq!(str, "aybc");
    str = s("abc");
    assert!(matches!(str.insert(5, &sp[1..2]), Err(ref e) if e.is::<OutOfRange>()));
}

#[test]
fn test_erase() {
    let mut str = s("abcdef");
    let i = str.erase(1, 3).expect("erase");
    assert_eq!(str.as_bytes()[i], b'd');
    assert_eq!(str, "adef");
}

#[test]
fn test_ctor_iterator() {
    let data: std::collections::LinkedList<u8> = b"abc".iter().copied().collect();
    let v: Vec<u8> = data.iter().copied().collect();
    let st = Sstring::from_bytes(&v);
    assert_eq!(st, "abc");
}

fn at_c_str<const MAX: usize, const NUL: bool>(x: &BasicSstring<MAX, NUL>, i: usize) -> u8 {
    // SAFETY: a NUL-terminated sstring has size() + 1 readable bytes at c_str().
    unsafe { *x.c_str().add(i) }
}

fn strncmp_eq(a: *const u8, b: *const u8, n: usize) -> bool {
    // SAFETY: both strings have at least `n` readable bytes.
    unsafe { std::slice::from_raw_parts(a, n) == std::slice::from_raw_parts(b, n) }
}

#[test]
fn test_nul_termination() {
    type Stype = BasicSstring<15, true>;
    let digits = b"01234567890123456789012345678901";
    let mut size = 1usize;
    while size <= 32 {
        let mut s1: Stype = uninitialized_string(size - 1);
        assert_eq!(at_c_str(&s1, size - 1), b'\0');
        let mut s2: Stype = uninitialized_string(size);
        assert_eq!(at_c_str(&s2, size), b'\0');

        s1 = Stype::from_bytes(&digits[..size - 1]);
        assert_eq!(at_c_str(&s1, size - 1), b'\0');
        s2 = Stype::from_bytes(&digits[..size]);
        assert_eq!(at_c_str(&s2, size), b'\0');

        s1 = Stype::filled(size - 1, b' ');
        assert_eq!(at_c_str(&s1, size - 1), b'\0');
        s2 = Stype::filled(size, b' ');
        assert_eq!(at_c_str(&s2, size), b'\0');

        s2 = s1.clone();
        assert_eq!(at_c_str(&s2, s1.size()), b'\0');
        s2.resize(s1.size());
        assert_eq!(at_c_str(&s2, s1.size()), b'\0');
        assert_eq!(s1, s2);

        let mut new_size = size / 2;
        s2 = s1.clone();
        s2.resize(new_size);
        assert_eq!(at_c_str(&s2, new_size), b'\0');
        assert!(strncmp_eq(s1.c_str(), s2.c_str(), new_size.min(s1.size())));

        new_size = size * 2;
        s2 = s1.clone();
        s2.resize(new_size);
        assert_eq!(at_c_str(&s2, new_size), b'\0');
        assert!(strncmp_eq(s1.c_str(), s2.c_str(), s1.size().min(s2.size())));

        s2 = &s1 + &s1;
        assert_eq!(at_c_str(&s2, s2.size()), b'\0');
        assert!(strncmp_eq(s1.c_str(), s2.c_str(), s1.size().min(s2.size())));
        size *= 2;
    }
}

#[test]
fn test_resize_and_overwrite() {
    const NEW_SIZE: usize = 42;
    const PATTERN: u8 = b's';
    {
        // the size of new content is identical to the specified count
        let mut st = Sstring::new();
        st.resize_and_overwrite(NEW_SIZE, |buf, n| {
            // SAFETY: `buf` has `n` writable bytes.
            unsafe { std::ptr::write_bytes(buf, PATTERN, n) };
            n
        });
        assert_eq!(st, Sstring::filled(NEW_SIZE, PATTERN));
    }
    {
        // the size of new content is smaller than the specified count
        const SMALLER_SIZE: usize = NEW_SIZE / 2;
        let mut st = Sstring::new();
        st.resize_and_overwrite(NEW_SIZE, |buf, _n| {
            // SAFETY: `buf` has at least SMALLER_SIZE writable bytes.
            unsafe { std::ptr::write_bytes(buf, PATTERN, SMALLER_SIZE) };
            SMALLER_SIZE
        });
        assert_eq!(st, Sstring::filled(SMALLER_SIZE, PATTERN));
    }
}

#[test]
fn test_compares_left_hand_not_string() {
    // mostly a compile test for non-sstring left-hand-side
    assert!(s("a") == "a");
    // The comparison with a `String` is what this checks.
    #[allow(clippy::cmp_owned)]
    let eq_string = s("a") == String::from("a");
    assert!(eq_string);
    assert!(s("a") == *"a");
    assert!(s("a") < s("b"));
    // Built from a `String`.
    #[allow(clippy::cmp_owned)]
    let lt_string = Sstring::from(String::from("a").as_str()) < s("b");
    assert!(lt_string);
}

#[test]
fn test_fmt() {
    // Debug formatting of a value and of an empty vector of values.
    let _ = format!("{:?}", Some(s("hello")));
    let strings: Vec<Sstring> = Vec::new();
    let _ = format!("{:?}", strings);
}
