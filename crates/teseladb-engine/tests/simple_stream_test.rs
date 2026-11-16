//! Tests of the simple memory streams: contiguous and fragmented output and input cursors,
//! writes and reads of every primitive, skips, and out-of-range errors past the end.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::{ExceptionPtr, OutOfRange};
use teseladb::engine::core::simple_stream::{
    FragmentIterator, FragmentedMemoryInputStream, FragmentedMemoryOutputStream,
    SimpleMemoryInputStream, SimpleMemoryOutputStream,
};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

/// What `write_read_test` needs of an output stream.
trait TestOutput {
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr>;
    fn fill(&mut self, c: u8, size: usize) -> Result<(), ExceptionPtr>;
}

/// What `write_read_test` needs of an input stream.
trait TestInput {
    fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr>;
}

impl TestOutput for SimpleMemoryOutputStream {
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        SimpleMemoryOutputStream::write(self, data)
    }
    fn fill(&mut self, c: u8, size: usize) -> Result<(), ExceptionPtr> {
        SimpleMemoryOutputStream::fill(self, c, size)
    }
}

impl TestInput for SimpleMemoryInputStream {
    fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr> {
        SimpleMemoryInputStream::read(self, out)
    }
}

impl<I: FragmentIterator> TestOutput for FragmentedMemoryOutputStream<I> {
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        FragmentedMemoryOutputStream::write(self, data)
    }
    fn fill(&mut self, c: u8, size: usize) -> Result<(), ExceptionPtr> {
        FragmentedMemoryOutputStream::fill(self, c, size)
    }
}

impl<I: FragmentIterator> TestInput for FragmentedMemoryInputStream<I> {
    fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr> {
        FragmentedMemoryInputStream::read(self, out)
    }
}

/// Asserts that `r` failed with an out-of-range error.
fn check_out_of_range(r: Result<(), ExceptionPtr>) {
    assert!(matches!(r, Err(ref e) if e.is::<OutOfRange>()));
}

fn write_read_test<I: TestInput, O: TestOutput>(mut in_: I, mut out: O) {
    let aa = vec![b'a'; 4];
    let bb = vec![b'b'; 3];
    let cc = vec![b'c'; 2];

    out.write(&aa).expect("write");
    out.fill(b'b', 3).expect("fill");

    check_out_of_range(out.fill(b' ', 3));
    check_out_of_range(out.write(b"   "));

    out.write(&cc).expect("write");

    check_out_of_range(out.fill(b' ', 1));
    check_out_of_range(out.write(b" "));

    let mut actual_aa = vec![0u8; 4];
    in_.read(&mut actual_aa).expect("read");
    assert_eq!(aa, actual_aa);

    let mut actual_bb = vec![0u8; 3];
    in_.read(&mut actual_bb).expect("read");
    assert_eq!(bb, actual_bb);

    actual_aa.resize(1024, 0);
    check_out_of_range(in_.read(&mut actual_aa));

    let mut actual_cc = vec![0u8; 2];
    in_.read(&mut actual_cc).expect("read");
    assert_eq!(cc, actual_cc);

    check_out_of_range(in_.read(&mut actual_aa[..1]));
}

#[test]
fn simple_write_read_test() {
    let mut buf = TemporaryBuffer::<u8>::new(9);

    let size = buf.size();
    let p = buf.get_write();
    // SAFETY: `p` points at the `size` bytes of `buf`, which outlives both cursors; the
    // input cursor reads the bytes only after the output cursor has written
    // them all, on this thread, and nothing else touches them.
    write_read_test(unsafe { SimpleMemoryInputStream::new(p, size) }, unsafe {
        SimpleMemoryOutputStream::new(p, size, 0)
    });

    // SAFETY: `p` points at the `size` writable bytes of `buf`.
    unsafe { std::ptr::write_bytes(p, b'x', size) };
    // SAFETY: as above.
    let out = unsafe { SimpleMemoryOutputStream::new(p, size, 0) };
    // SAFETY: write_read_test does every write through `out` before the first read through
    // the input cursor, and never copies one cursor into the other.
    write_read_test(unsafe { out.to_input_stream() }, out);
}

/// An iterator over the fragments of a vector of buffers.
#[derive(Clone)]
struct BufIter {
    p: *mut TemporaryBuffer<u8>,
}

// SAFETY: a `BufIter` walks the vector of buffers the test keeps alive and unchanged while
// the streams are used, and the streams never go past its `TOTAL_SIZE` bytes; every fragment
// is the readable and writable storage of a `TemporaryBuffer`.
unsafe impl FragmentIterator for BufIter {
    fn fragment(&self) -> (*mut u8, usize) {
        // SAFETY: the iterator never goes past the vector the test keeps alive.
        unsafe { ((*self.p).get_write(), (*self.p).size()) }
    }
    fn advance(&mut self) {
        // SAFETY: as above.
        self.p = unsafe { self.p.add(1) };
    }
}

#[test]
fn fragmented_write_read_test() {
    const TOTAL_SIZE: usize = 9;

    let mut bufs: Vec<TemporaryBuffer<u8>> = Vec::new();

    let test = |bufs: &mut Vec<TemporaryBuffer<u8>>| {
        let begin = BufIter { p: bufs.as_mut_ptr() };
        write_read_test(
            FragmentedMemoryInputStream::new(begin.clone(), TOTAL_SIZE),
            FragmentedMemoryOutputStream::new(begin.clone(), TOTAL_SIZE),
        );
        let out = FragmentedMemoryOutputStream::new(begin, TOTAL_SIZE);
        // SAFETY: as in simple_write_read_test: every write through `out` comes before the
        // first read through the input cursor, and the cursors are never copied into each
        // other.
        write_read_test(unsafe { out.to_input_stream() }, out);
    };

    bufs.push(TemporaryBuffer::new(TOTAL_SIZE));
    test(&mut bufs);

    bufs.clear();

    for _ in 0..TOTAL_SIZE {
        bufs.push(TemporaryBuffer::new(1));
    }
    test(&mut bufs);
}
