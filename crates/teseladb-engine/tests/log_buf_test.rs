//! Tests of `LogBuf`: growing from an external buffer, formatting through its insert
//! iterator, and clearing.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::fmt::Write as _;

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::util::log_impl::internal::LogBuf;
use teseladb::engine_test_case;

engine_test_case!(log_buf_realloc, {
    let mut external_buf = [0u8; 128];
    let external_buf_ptr = external_buf.as_mut_ptr() as usize;

    // SAFETY: `external_buf` holds 128 bytes and outlives `b` (declared before it).
    let mut b = unsafe { LogBuf::with_external(external_buf.as_mut_ptr(), external_buf.len()) };

    assert_eq!(b.data() as usize, external_buf_ptr);

    let mut it = b.back_insert_begin();

    for _ in 0..128 {
        it.post_inc().assign(b'a');
    }

    it.assign(b'a'); // should trigger realloc

    assert_ne!(b.data() as usize, external_buf.as_ptr() as usize);

    let p = b.data();
    for i in 0..129 {
        // SAFETY: the buffer holds the 129 bytes written above.
        assert_eq!(unsafe { *p.add(i) }, b'a');
    }

    make_ready_future(())
});

engine_test_case!(log_buf_insert_iterator_format_to, {
    const SIZE: usize = 128;
    let mut external_buf = vec![0u8; SIZE].into_boxed_slice();
    let external_buf_ptr = external_buf.as_mut_ptr();

    // SAFETY: `external_buf` holds SIZE bytes and outlives `b` (declared before it).
    let mut b = unsafe { LogBuf::with_external(external_buf_ptr, SIZE) };

    assert_eq!(b.data() as usize, external_buf_ptr as usize);

    // The iterator keeps no state of its own (all the work happens on assignment), and
    // here it borrows the buffer exclusively, so each use takes a fresh one, which is the
    // same iterator: this lets the test read the buffer between writes.

    // Format with a runtime string that has no
    // replacement field, so it writes `str` itself.
    let str_a = "a".repeat(SIZE);
    let _ = write!(b.back_insert_begin(), "{}", str_a);

    assert_eq!(b.data() as usize, external_buf_ptr as usize);

    b.back_insert_begin().post_inc().assign(b'\n');

    assert_ne!(b.data() as usize, external_buf_ptr as usize);

    let str_b = "b".repeat(SIZE);
    let _ = write!(b.back_insert_begin(), "{}", str_b);
    b.back_insert_begin().post_inc().assign(b'\n');

    let p = b.data();
    let mut pos = 0usize;
    // SAFETY: the buffer holds the 2 * (SIZE + 1) bytes written above.
    let at = |i: usize| unsafe { *p.add(i) };
    for _ in 0..SIZE {
        assert_eq!(at(pos), b'a');
        pos += 1;
    }
    assert_eq!(at(pos), b'\n');
    pos += 1;
    for _ in 0..SIZE {
        assert_eq!(at(pos), b'b');
        pos += 1;
    }
    assert_eq!(at(pos), b'\n');

    make_ready_future(())
});

engine_test_case!(log_buf_clear, {
    let mut buf = LogBuf::new();
    // A fresh iterator per write, as in log_buf_insert_iterator_format_to.

    let _ = write!(buf.back_insert_begin(), "abcd");
    assert_eq!(buf.view(), "abcd".as_bytes());

    let mut cap_before = buf.capacity();
    buf.clear();
    assert_eq!(cap_before, buf.capacity());
    assert_eq!(0, buf.size());

    let _ = write!(buf.back_insert_begin(), "uuvvwwxxyyzz");
    assert_eq!(buf.view(), "uuvvwwxxyyzz".as_bytes());

    cap_before = buf.capacity();
    buf.clear();
    assert_eq!(cap_before, buf.capacity());
    assert_eq!(0, buf.size());

    make_ready_future(())
});
