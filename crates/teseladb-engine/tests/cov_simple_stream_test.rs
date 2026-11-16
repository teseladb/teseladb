//! Tests of the in-memory serialization cursors beyond plain writes and reads: skipping,
//! sub-streams (contiguous and spanning fragments), fills, empty fragments, the type-erased
//! cursors and their visitors, copies into other sinks, and the overflow and underflow errors
//! of every cursor.
#![cfg(target_os = "linux")]

use std::rc::Rc;

use teseladb::engine::core::future::{ExceptionPtr, OutOfRange};
use teseladb::engine::core::simple_stream::{
    FragmentIterator, FragmentedMemoryInputStream, FragmentedMemoryOutputStream,
    MeasuringOutputStream, MemoryInputStream, MemoryOutputStream, SimpleInputStream,
    SimpleMemoryInputStream, SimpleMemoryOutputStream, SimpleOutputStream, StreamOutput,
    StreamVisitor, with_serialized_stream, with_serialized_stream_direct,
};

/// Fragments of buffers the test owns, walked by index.
#[derive(Clone)]
struct Frags {
    frags: Rc<Vec<(*mut u8, usize)>>,
    i: usize,
}

// SAFETY: every fragment is a live vector the test keeps unchanged while the cursors are used;
// past the last fragment the iterator yields an empty one, which is never dereferenced.
unsafe impl FragmentIterator for Frags {
    fn fragment(&self) -> (*mut u8, usize) {
        self.frags.get(self.i).copied().unwrap_or((std::ptr::null_mut(), 0))
    }

    fn advance(&mut self) {
        self.i += 1;
    }
}

/// Fragments of 3, 0, 4 and 2 bytes (9 in total), zeroed.
struct Storage {
    bufs: Vec<Vec<u8>>,
    it: Frags,
}

impl Storage {
    fn new() -> Self {
        let mut bufs: Vec<Vec<u8>> = [3usize, 0, 4, 2].iter().map(|&n| vec![0u8; n]).collect();
        let frags =
            bufs.iter_mut()
                .map(|b| {
                    if b.is_empty() { (std::ptr::null_mut(), 0) } else { (b.as_mut_ptr(), b.len()) }
                })
                .collect();
        Storage { bufs, it: Frags { frags: Rc::new(frags), i: 0 } }
    }

    fn bytes(&self) -> Vec<u8> {
        self.bufs.concat()
    }
}

const TOTAL: usize = 9;

fn assert_out_of_range(r: Result<(), ExceptionPtr>, what: &str) {
    let e = r.expect_err("past the end");
    assert!(e.downcast_ref::<OutOfRange>().is_some());
    assert!(e.to_string().contains(what), "{e}");
}

#[test]
fn measuring_streams_count_bytes() {
    let mut m = MeasuringOutputStream::default();
    m.write(b"abc");
    StreamOutput::write(&mut m, b"de").unwrap();
    assert_eq!(m.size(), 5);
}

#[test]
fn simple_output_cursors_skip_fill_and_split() {
    let mut buf = [0u8; 12];
    // SAFETY: `buf` outlives the cursors and is read only after they are done.
    let mut out: SimpleOutputStream =
        unsafe { SimpleMemoryOutputStream::new(buf.as_mut_ptr(), 10, 2) };
    assert!(!SimpleMemoryOutputStream::HAS_WITH_STREAM);
    assert_eq!(out.begin(), buf.as_mut_ptr().wrapping_add(2));
    out.write(b"").unwrap();
    out.fill(b'z', 0).unwrap();
    out.fill(b'-', 2).unwrap();
    out.skip(1).unwrap();
    let mut sub = out.write_substream(3).unwrap();
    assert_eq!(sub.size(), 3);
    StreamOutput::write(&mut sub, b"sub").unwrap();
    assert_out_of_range(sub.write(b"x"), "overflow");
    out.write(b"end").unwrap();
    assert_eq!(out.size(), 1);
    assert_out_of_range(out.skip(2), "overflow");
    assert_out_of_range(out.fill(b'!', 2), "overflow");
    assert_out_of_range(out.write(b"xy"), "overflow");
    assert!(out.write_substream(2).is_err());
    assert_eq!(&buf[2..11], b"--\0subend");

    // A default cursor has no room but accepts empty writes.
    let mut empty = SimpleMemoryOutputStream::default();
    empty.write(&[]).unwrap();
    empty.fill(0, 0).unwrap();
    assert_out_of_range(empty.write(b"a"), "overflow");
}

#[test]
fn simple_input_cursors_skip_split_and_copy() {
    let data = *b"0123456789";
    // SAFETY: `data` outlives the cursors and is never written.
    let mut input: SimpleInputStream = unsafe { SimpleMemoryInputStream::new(data.as_ptr(), 10) };
    assert!(!SimpleMemoryInputStream::HAS_WITH_STREAM);
    assert_eq!(input.begin(), data.as_ptr());
    input.skip(1).unwrap();
    let mut sub = input.read_substream(3).unwrap();
    let mut two = [0u8; 2];
    sub.read(&mut two).unwrap();
    assert_eq!(&two, b"12");
    assert_out_of_range(sub.read(&mut two), "underflow");
    sub.read(&mut []).unwrap();
    let mut m = MeasuringOutputStream::default();
    sub.copy_to(&mut m).unwrap();
    assert_eq!(m.size(), 1);
    assert_eq!(input.size(), 6);
    assert!(input.read_substream(7).is_err());
    assert_out_of_range(input.skip(7), "underflow");

    // Copy the rest into a write cursor, and refuse a copy that does not fit.
    let mut out_buf = [0u8; 6];
    // SAFETY: `out_buf` outlives the cursor.
    let mut out = unsafe { SimpleMemoryOutputStream::new(out_buf.as_mut_ptr(), 6, 0) };
    input.copy_to(&mut out).unwrap();
    assert_eq!(&out_buf, b"456789");
    let mut small = [0u8; 2];
    // SAFETY: as above.
    let mut out = unsafe { SimpleMemoryOutputStream::new(small.as_mut_ptr(), 2, 0) };
    assert_out_of_range(input.copy_to(&mut out), "overflow");

    // An empty default cursor copies nothing.
    let empty = SimpleMemoryInputStream::default();
    let mut m = MeasuringOutputStream::default();
    empty.copy_to(&mut m).unwrap();
    assert_eq!(m.size(), 0);
    assert_eq!(empty.size(), 0);
}

#[test]
fn fragmented_output_cursors_cross_fragments() {
    let s = Storage::new();
    let mut out = FragmentedMemoryOutputStream::new(s.it.clone(), TOTAL);
    assert!(!FragmentedMemoryOutputStream::<Frags>::HAS_WITH_STREAM);
    out.fill(b'a', 2).unwrap();
    // One byte left in the first fragment: a two-byte sub-stream spans the empty fragment.
    let mut spanning = out.write_substream(2).unwrap();
    assert!(matches!(spanning, MemoryOutputStream::Fragmented(_)));
    spanning.write(b"bc").unwrap();
    // Three bytes left in the third fragment: a sub-stream inside it is contiguous.
    let mut inside = out.write_substream(2).unwrap();
    assert!(matches!(inside, MemoryOutputStream::Simple(_)));
    inside.fill(b'd', 2).unwrap();
    out.skip(1).unwrap();
    assert_eq!(out.size(), 2);
    assert_out_of_range(out.write(b"xyz"), "overflow");
    assert_out_of_range(out.fill(b'x', 3), "overflow");
    assert_out_of_range(out.skip(3), "overflow");
    assert!(out.write_substream(3).is_err());
    out.write(b"ef").unwrap();
    assert_eq!(out.size(), 0);
    assert_eq!(s.bytes(), b"aabcdd\0ef");
}

#[test]
fn fragmented_input_cursors_cross_fragments() {
    let mut s = Storage::new();
    s.bufs[0].copy_from_slice(b"abc");
    s.bufs[2].copy_from_slice(b"defg");
    s.bufs[3].copy_from_slice(b"hi");
    let mut input = FragmentedMemoryInputStream::new(s.it.clone(), TOTAL);
    assert!(!FragmentedMemoryInputStream::<Frags>::HAS_WITH_STREAM);
    input.skip(1).unwrap();
    assert_eq!(input.first_fragment_size(), 2);
    assert_eq!(input.first_fragment_data(), s.bufs[0][1..].as_ptr());
    assert_eq!(input.fragment_iterator().i, 1);
    let mut sub = input.read_substream(4).unwrap();
    let mut four = [0u8; 4];
    sub.read(&mut four).unwrap();
    assert_eq!(&four, b"bcde");
    assert_out_of_range(sub.read(&mut [0u8; 1]), "underflow");
    assert!(input.read_substream(TOTAL).is_err());
    assert_out_of_range(input.skip(TOTAL), "underflow");
    let mut m = MeasuringOutputStream::default();
    input.clone().copy_to(&mut m).unwrap();
    assert_eq!(m.size(), 4);
    let mut out_buf = [0u8; 3];
    // SAFETY: `out_buf` outlives the cursor.
    let mut out = unsafe { SimpleMemoryOutputStream::new(out_buf.as_mut_ptr(), 3, 0) };
    // Four bytes do not fit in three: the first fragment is copied, the second overflows.
    assert_out_of_range(input.copy_to(&mut out), "overflow");
    assert_eq!(&out_buf, b"fg\0");
}

#[test]
fn output_cursors_convert_to_input_cursors() {
    let s = Storage::new();
    let mut out = FragmentedMemoryOutputStream::new(s.it.clone(), TOTAL);
    out.write(b"xy").unwrap();
    // SAFETY: the bytes the input cursor covers are not written while it reads them.
    let mut rest = unsafe { out.to_input_stream() };
    assert_eq!(rest.size(), 7);
    let mut erased: MemoryOutputStream<Frags> = MemoryOutputStream::Fragmented(out);
    // SAFETY: as above.
    let erased_in = unsafe { erased.to_input_stream() };
    assert!(matches!(erased_in, MemoryInputStream::Fragmented(_)));
    erased.write(b"z").unwrap();
    let mut first = [0u8; 1];
    rest.read(&mut first).unwrap();
    assert_eq!(&first, b"z");

    let mut buf = [0u8; 4];
    // SAFETY: `buf` outlives the cursors.
    let simple = unsafe { SimpleMemoryOutputStream::new(buf.as_mut_ptr(), 4, 0) };
    let erased: MemoryOutputStream<Frags> = MemoryOutputStream::Simple(simple);
    // SAFETY: nothing writes the bytes while the input cursor exists.
    let erased_in = unsafe { erased.to_input_stream() };
    assert!(matches!(erased_in, MemoryInputStream::Simple(_)));
    assert_eq!(erased_in.size(), 4);
}

/// Names the representation a visitor is called with and reports its size.
struct Which;

impl StreamVisitor<SimpleMemoryOutputStream, FragmentedMemoryOutputStream<Frags>> for Which {
    type Output = (&'static str, usize);
    fn simple(self, s: &mut SimpleMemoryOutputStream) -> Self::Output {
        ("simple", s.size())
    }
    fn fragmented(self, f: &mut FragmentedMemoryOutputStream<Frags>) -> Self::Output {
        ("fragmented", f.size())
    }
}

impl StreamVisitor<SimpleMemoryInputStream, FragmentedMemoryInputStream<Frags>> for Which {
    type Output = (&'static str, usize);
    fn simple(self, s: &mut SimpleMemoryInputStream) -> Self::Output {
        ("simple", s.size())
    }
    fn fragmented(self, f: &mut FragmentedMemoryInputStream<Frags>) -> Self::Output {
        ("fragmented", f.size())
    }
}

#[test]
fn type_erased_output_cursors_dispatch_each_call() {
    assert!(MemoryOutputStream::<Frags>::HAS_WITH_STREAM);
    let mut default = MemoryOutputStream::<Frags>::default();
    assert_eq!(default.size(), 0);
    assert_eq!(default.with_stream(Which), ("simple", 0));

    let mut buf = [0u8; 8];
    // SAFETY: `buf` outlives the cursor.
    let mut simple: MemoryOutputStream<Frags> = MemoryOutputStream::Simple(unsafe {
        SimpleMemoryOutputStream::new(buf.as_mut_ptr(), 8, 0)
    });
    simple.write(b"ab").unwrap();
    simple.fill(b'-', 1).unwrap();
    simple.skip(1).unwrap();
    let mut sub = simple.write_substream(2).unwrap();
    sub.write(b"cd").unwrap();
    assert_eq!(simple.with_stream(Which), ("simple", 2));
    assert_out_of_range(simple.write(b"xyz"), "overflow");
    assert!(simple.write_substream(3).is_err());
    assert_eq!(&buf[..6], b"ab-\0cd");

    let s = Storage::new();
    let mut frag: MemoryOutputStream<Frags> =
        MemoryOutputStream::Fragmented(FragmentedMemoryOutputStream::new(s.it.clone(), TOTAL));
    frag.write(b"12").unwrap();
    frag.fill(b'3', 1).unwrap();
    frag.skip(1).unwrap();
    let mut sub = frag.write_substream(3).unwrap();
    sub.write(b"567").unwrap();
    assert_eq!(frag.size(), 2);
    assert_eq!(frag.with_stream(Which), ("fragmented", 2));
    assert_out_of_range(frag.skip(3), "overflow");
    assert_eq!(s.bytes(), b"123\0567\0\0");
}

#[test]
fn type_erased_input_cursors_dispatch_each_call() {
    assert!(MemoryInputStream::<Frags>::HAS_WITH_STREAM);
    let data = *b"abcdefgh";
    // SAFETY: `data` outlives the cursor and is never written.
    let mut simple: MemoryInputStream<Frags> =
        MemoryInputStream::Simple(unsafe { SimpleMemoryInputStream::new(data.as_ptr(), 8) });
    simple.skip(1).unwrap();
    let mut sub = simple.read_substream(2).unwrap();
    assert!(matches!(sub, MemoryInputStream::Simple(_)));
    let mut two = [0u8; 2];
    sub.read(&mut two).unwrap();
    assert_eq!(&two, b"bc");
    simple.read(&mut two).unwrap();
    assert_eq!(&two, b"de");
    let mut m = MeasuringOutputStream::default();
    simple.copy_to(&mut m).unwrap();
    assert_eq!(m.size(), 3);
    assert_eq!(with_serialized_stream(&mut simple, Which), ("simple", 3));
    assert_out_of_range(simple.skip(4), "underflow");
    assert!(simple.read_substream(4).is_err());

    let mut s = Storage::new();
    s.bufs[0].copy_from_slice(b"ABC");
    s.bufs[2].copy_from_slice(b"DEFG");
    s.bufs[3].copy_from_slice(b"HI");
    let mut frag: MemoryInputStream<Frags> =
        MemoryInputStream::Fragmented(FragmentedMemoryInputStream::new(s.it.clone(), TOTAL));
    frag.skip(2).unwrap();
    let mut sub = frag.read_substream(3).unwrap();
    assert!(matches!(sub, MemoryInputStream::Fragmented(_)));
    let mut three = [0u8; 3];
    sub.read(&mut three).unwrap();
    assert_eq!(&three, b"CDE");
    frag.read(&mut two).unwrap();
    assert_eq!(&two, b"FG");
    assert_eq!(frag.size(), 2);
    assert_eq!(frag.with_stream(Which), ("fragmented", 2));
    let mut m = MeasuringOutputStream::default();
    frag.copy_to(&mut m).unwrap();
    assert_eq!(m.size(), 2);
    assert!(frag.read_substream(1).is_err());

    let mut direct = 41u32;
    assert_eq!(with_serialized_stream_direct(&mut direct, |d| *d + 1), 42);
}
