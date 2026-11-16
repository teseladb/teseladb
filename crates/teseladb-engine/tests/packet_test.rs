//! Tests of `Packet`: building from many fragments, and headers kept contiguous when
//! prepended to small or many fragments.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::RefCell;

use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::net::packet::{Fragment, Packet};

#[test]
fn test_many_fragments() {
    let expected: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    let append = |p: Packet, c: u8, n: usize| -> Packet {
        let mut tmp = TemporaryBuffer::<u8>::new(n);
        // SAFETY: `tmp` has `n` writable bytes.
        unsafe { std::ptr::write_bytes(tmp.get_write(), c, n) };
        expected.borrow_mut().extend(std::iter::repeat_n(c, n));
        Packet::append_buffer(p, tmp)
    };

    let mut p = Packet::default();
    p = append(p, b'a', 5);
    p = append(p, b'b', 31);
    p = append(p, b'c', 65);
    p = append(p, b'c', 4096);
    p = append(p, b'd', 4096);

    let verify = |p: &Packet| {
        let expected = expected.borrow();
        assert_eq!(p.len() as usize, expected.len());
        let mut expected_it = 0usize;
        for frag in p.fragments().iter() {
            assert!(frag.size <= expected.len() - expected_it);
            // SAFETY: a fragment points at `size` readable bytes of the packet.
            let bytes = unsafe { std::slice::from_raw_parts(frag.base, frag.size) };
            assert!(bytes == &expected[expected_it..expected_it + frag.size]);
            expected_it += frag.size;
        }
    };

    let trim_front = |p: &mut Packet, n: usize| {
        p.trim_front(n);
        expected.borrow_mut().drain(..n);
    };

    verify(&p);

    trim_front(&mut p, 1);
    verify(&p);

    trim_front(&mut p, 6);
    verify(&p);

    trim_front(&mut p, 29);
    verify(&p);

    trim_front(&mut p, 1024);
    verify(&p);

    let mut p2 = Packet::default();
    p2 = append(p2, b'z', 9);
    p2 = append(p2, b'x', 7);

    p.append(p2);
    verify(&p);
}

type TcpHeader = [u8; 20];
type IpHeader = [u8; 20];

#[test]
fn test_headers_are_contiguous() {
    let mut data = [0u8; 1000];
    let f = Fragment { base: data.as_mut_ptr(), size: data.len() };
    let mut p = Packet::from_fragment(f);
    p.prepend_header::<TcpHeader>(0);
    p.prepend_header::<IpHeader>(0);
    assert_eq!(p.nr_frags(), 2u32);
}

#[test]
fn test_headers_are_contiguous_even_with_small_fragment() {
    let mut data = [0u8; 100];
    let f = Fragment { base: data.as_mut_ptr(), size: data.len() };
    let mut p = Packet::from_fragment(f);
    p.prepend_header::<TcpHeader>(0);
    p.prepend_header::<IpHeader>(0);
    assert_eq!(p.nr_frags(), 2u32);
}

#[test]
fn test_headers_are_contiguous_even_with_many_fragments() {
    let mut data = [0u8; 100];
    let f = Fragment { base: data.as_mut_ptr(), size: data.len() };
    let mut p = Packet::from_fragment(f);
    for _ in 0..7 {
        p.append(Packet::from_fragment(f));
    }
    p.prepend_header::<TcpHeader>(0);
    p.prepend_header::<IpHeader>(0);
    assert_eq!(p.nr_frags(), 9u32);
}

// Linearizing an empty packet copies no fragment: the packet is left with one empty fragment,
// as packet::linearize() leaves it (its fragment count drops by `nr_frags - 1` in unsigned
// arithmetic, one more here). With overflow checks the count update panicked instead.
#[test]
fn test_linearize_empty_packet() {
    let mut p = Packet::default();
    p.linearize_all();
    assert_eq!(p.len(), 0u32);
    assert_eq!(p.nr_frags(), 1u32);
    assert_eq!(p.frag(0).size, 0);
}
