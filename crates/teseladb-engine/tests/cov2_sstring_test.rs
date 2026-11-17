#![allow(unsafe_code)]
//! String behaviour not reached by sstring_test: the representation without a terminating NUL
//! (the original's `basic_sstring<char, uint32_t, 15, false>`), shrinking an external string
//! back inline, releasing external and inline strings into buffers, the comparisons, the
//! clamped ranges, the overflow error, invalid UTF-8 in the views and printing, word reading
//! across buffer refills, and `format_sstring` with a `Display` that fails or changes.
#![cfg(target_os = "linux")]

use std::fmt;
use std::io::{BufReader, Cursor};

use teseladb::engine::core::future::OverflowError;
use teseladb::engine::core::sstring::{
    BasicSstring, InitializedLater, Sstring, format_sstring, read_word, swap, to_sstring,
};

type Plain = BasicSstring<15, false>;

#[test]
fn strings_without_a_terminating_nul() {
    // 15 bytes fit inline when no byte is kept for the NUL; 16 go to the heap.
    let inline = Plain::from_bytes(b"0123456789abcde");
    assert_eq!(inline.size(), 15);
    assert_eq!(Plain::padding(), 0);
    let mut ext = Plain::from_bytes(b"0123456789abcdefghij");
    assert_eq!(ext.as_bytes(), b"0123456789abcdefghij");
    let copy = ext.clone();
    // Shrinking an external string keeps it external while it does not fit inline...
    ext.resize(17);
    assert_eq!(ext.as_bytes(), b"0123456789abcdefg");
    // ...and brings it back inline when it does.
    ext.resize(4);
    assert_eq!(ext.as_bytes(), b"0123");
    assert_eq!(copy.size(), 20, "the clone owns its own block");
    let mut small = Plain::from_bytes(b"hello");
    small.resize(2);
    assert_eq!(small.as_bytes(), b"he");
    assert_eq!(&small + &copy, Plain::from_bytes(b"he0123456789abcdefghij"));
    let buf = copy.release();
    assert_eq!(buf.as_slice(), b"0123456789abcdefghij");
}

#[test]
fn nul_terminated_strings_keep_the_nul() {
    // 14 bytes plus the NUL fit inline; 15 do not.
    let mut s = Sstring::from_bytes(b"0123456789abcd");
    // SAFETY: c_str points to size() + 1 bytes for a NUL-terminated string.
    assert_eq!(unsafe { *s.c_str().add(14) }, 0);
    s.resize(20);
    assert_eq!(s.size(), 20);
    // SAFETY: as above.
    assert_eq!(unsafe { *s.c_str().add(20) }, 0);
    s.resize(18);
    // SAFETY: as above.
    assert_eq!(unsafe { *s.c_str().add(18) }, 0);
}

#[test]
fn release_hands_over_external_blocks_and_copies_inline_ones() {
    let ext = Sstring::from("a string that does not fit inline");
    let p = ext.as_bytes().as_ptr();
    let buf = ext.release();
    assert_eq!(buf.as_slice(), b"a string that does not fit inline");
    assert_eq!(buf.get(), p, "the external block moves into the buffer without a copy");
    let buf = Sstring::from("short").release();
    assert_eq!(buf.as_slice(), b"short");
}

#[test]
fn a_size_beyond_32_bits_overflows() {
    let e = Sstring::initialized_later(InitializedLater, (u32::MAX as usize) + 1)
        .expect_err("overflow");
    assert!(e.downcast_ref::<OverflowError>().is_some(), "{e}");
    assert!(e.to_string().contains("sstring overflow"), "{e}");
    let s = Sstring::initialized_later(InitializedLater, 3).unwrap();
    assert_eq!(s.size(), 3);
}

#[test]
fn comparisons_three_way_and_by_range() {
    let s = Sstring::from("hello world");
    assert_eq!(s.compare(b"hello world"), 0);
    assert!(s.compare(b"hello") > 0);
    assert!(s.compare(b"zebra") < 0);
    assert_eq!(s.compare_range(6, 5, b"world").unwrap(), 0);
    // The length is clamped to the end of the string.
    assert_eq!(s.compare_range(6, 100, b"world").unwrap(), 0);
    assert!(s.compare_range(0, 5, b"help").unwrap() < 0);
    assert!(s.compare_range(0, 5, b"hellp").unwrap() < 0);
    assert!(s.compare_range(0, 5, b"hell").unwrap() > 0);
    assert_eq!(s.compare_range(11, 3, b"").unwrap(), 0);
    assert!(s.compare_range(12, 1, b"").is_err(), "a position past the end is out of range");
}

#[test]
fn searches_at_the_edges() {
    let s = Sstring::from("xaa");
    assert_eq!(s.find_byte(b'a', 3), Sstring::NPOS);
    assert_eq!(s.find_byte(b'a', 2), 2);
    // A candidate that runs past the end once the search moves on.
    assert_eq!(s.find(b"ab", 0), Sstring::NPOS);
    assert_eq!(s.find(b"aa", 0), 1);
    assert_eq!(s.find(b"", 3), 3);
    assert_eq!(s.find(b"a", 4), Sstring::NPOS);
}

#[test]
fn replacements_clamp_and_keep_lengths() {
    let mut s = Sstring::from("abcdef");
    s.replace_range(4, 100, b"XY").unwrap();
    assert_eq!(s, "abcdXY");
    s.replace_range(4, 100, b"Z").unwrap();
    assert_eq!(s, "abcdZ");
    s.replace(5, 0, b"").unwrap();
    assert_eq!(s, "abcdZ", "an empty in-place replacement changes nothing");
    let mut t = Sstring::from("abc");
    t.resize_and_overwrite(2, |p, n| {
        // SAFETY: `p` has `n` writable bytes.
        unsafe { *p = b'z' };
        n
    });
    assert_eq!(t, "zb", "growing nothing, then shrinking to what op wrote");
}

#[test]
fn indexing_swapping_conversions_and_reset() {
    let mut a = Sstring::from("abc");
    let mut b = Sstring::from("a long string on the heap");
    assert_eq!(a[1], b'b');
    a[1] = b'X';
    assert_eq!(a, "aXc");
    // SAFETY: the positions are within the strings.
    unsafe {
        assert_eq!(*a.get_unchecked(2), b'c');
        *a.get_unchecked_mut(0) = b'Y';
    }
    assert_eq!(a, "YXc");
    a.swap(&mut b);
    assert_eq!(a, "a long string on the heap");
    assert_eq!(b, "YXc");
    swap(&mut a, &mut b);
    assert_eq!(a, "YXc");
    let from_slice = Sstring::from(&b"bytes"[..]);
    let owned = String::from("owned");
    assert_eq!(from_slice, "bytes");
    assert_eq!(Sstring::from(&owned), owned);
    #[allow(deprecated)]
    a.reset();
    assert!(a.empty());
}

#[test]
fn invalid_utf8_in_views_conversions_and_printing() {
    let s = Sstring::from_bytes(b"ok\xffrest");
    assert_eq!(s.as_str(), "ok", "the longest valid prefix");
    assert_eq!(String::from(&s), "ok\u{fffd}rest");
    assert_eq!(format!("{s}"), "ok\u{fffd}rest");
    assert_eq!(format!("[{s:>9}]"), "[  ok\u{fffd}rest]", "padded to the width");
    assert_eq!(format!("{s:?}"), "\"ok\u{fffd}rest\"");
}

#[test]
fn read_word_skips_whitespace_and_joins_refills() {
    let mut r = BufReader::with_capacity(3, Cursor::new(&b"  \t hello   world"[..]));
    let mut s = Sstring::new();
    read_word(&mut r, &mut s).unwrap();
    assert_eq!(s, "hello");
    read_word(&mut r, &mut s).unwrap();
    assert_eq!(s, "world", "a word that ends at the end of input");
    read_word(&mut r, &mut s).unwrap();
    assert!(s.empty(), "nothing left");
    let mut plain = Plain::new();
    read_word(&mut Cursor::new(&b"one two"[..]), &mut plain).unwrap();
    assert_eq!(plain.as_bytes(), b"one");
}

struct Failing;
impl fmt::Display for Failing {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        Err(fmt::Error)
    }
}

/// Writes a longer text every time it is formatted.
struct Growing(std::cell::Cell<usize>);
impl fmt::Display for Growing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0.get() + 1;
        self.0.set(n);
        f.write_str(&"x".repeat(n))
    }
}

/// Writes a shorter text the second time.
struct Shrinking(std::cell::Cell<bool>);
impl fmt::Display for Shrinking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let second = self.0.replace(true);
        f.write_str(if second { "ab" } else { "abcd" })
    }
}

#[test]
fn format_sstring_with_literals_and_unstable_displays() {
    assert_eq!(format_sstring(format_args!("literal")), "literal");
    assert_eq!(format_sstring(format_args!("{}-{}", 1, "a")), "1-a");
    assert!(to_sstring(&Failing).empty(), "a failing Display formats nothing");
    // A Display that writes more the second time: what fitted is kept.
    let g = Growing(std::cell::Cell::new(0));
    assert_eq!(format_sstring(format_args!("{g}")), "");
    let s = Shrinking(std::cell::Cell::new(false));
    assert_eq!(format_sstring(format_args!("{s}")), "ab");
}
