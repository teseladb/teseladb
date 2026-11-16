//! [`MockReadOnlyFile`], a file implementation that serves reads of a given size without
//! storage and checks the size and number of the reads it receives.

#![allow(dead_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

use teseladb::engine::core::file::{DirectoryEntry, FileImpl, FileImplBase};
use teseladb::engine::core::future::{
    BadFunctionCall, Future, make_exception_future, make_exception_ptr, make_ready_future,
};
use teseladb::engine::core::io_intent::IoIntent;
use teseladb::engine::core::posix::Stat;
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::stream::{NextFn, Subscription};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

/// A read-only file of a fixed size whose reads are checked by a verifier. The file
/// `&self`, so the members they change sit in cells.
pub struct MockReadOnlyFile {
    base: FileImplBase,
    closed: Cell<bool>,
    total_file_size: u64,
    allowed_read_requests: Cell<usize>,
    verify_length: RefCell<Box<dyn FnMut(usize)>>,
}

impl MockReadOnlyFile {
    fn verify_read(&self, position: u64, length: usize) -> usize {
        assert!(!self.closed.get());
        assert!(position <= self.total_file_size);
        assert!(position + length as u64 <= self.total_file_size);
        if position + length as u64 != self.total_file_size {
            (self.verify_length.borrow_mut())(length);
        }
        assert!(self.allowed_read_requests.get() != 0);
        self.allowed_read_requests.set(self.allowed_read_requests.get() - 1);
        length
    }

    /// A file of `file_size` bytes that accepts reads of any size.
    pub fn new(file_size: u64) -> Self {
        MockReadOnlyFile {
            base: FileImplBase::default(),
            closed: Cell::new(false),
            total_file_size: file_size,
            allowed_read_requests: Cell::new(0),
            verify_length: RefCell::new(Box::new(|_| {})),
        }
    }

    pub fn set_read_size_verifier<F: FnMut(usize) + 'static>(&self, f: F) {
        *self.verify_length.borrow_mut() = Box::new(f);
    }

    pub fn set_expected_read_size(&self, expected: usize) {
        *self.verify_length.borrow_mut() = Box::new(move |length: usize| {
            assert_eq!(length, expected);
        });
    }

    pub fn set_allowed_read_requests(&self, requests: usize) {
        self.allowed_read_requests.set(requests);
    }
}

impl FileImpl for MockReadOnlyFile {
    fn file_impl_base(&self) -> &FileImplBase {
        &self.base
    }

    unsafe fn write_dma(
        &self,
        _pos: u64,
        _buffer: *const u8,
        _len: usize,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    unsafe fn write_dma_iov(
        &self,
        _pos: u64,
        _iov: Vec<Iovec>,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    unsafe fn read_dma(
        &self,
        pos: u64,
        _buffer: *mut u8,
        len: usize,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        make_ready_future(self.verify_read(pos, len))
    }

    unsafe fn read_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        let length = iov.iter().map(|iov| iov.iov_len).sum::<usize>();
        make_ready_future(self.verify_read(pos, length))
    }

    fn flush(&self) -> Future<()> {
        make_ready_future(())
    }

    fn stat(&self) -> Future<Stat> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn truncate(&self, _length: u64) -> Future<()> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn discard(&self, _offset: u64, _length: u64) -> Future<()> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn allocate(&self, _position: u64, _length: u64) -> Future<()> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn size(&self) -> Future<u64> {
        make_ready_future(self.total_file_size)
    }

    fn close(&self) -> Future<()> {
        assert!(!self.closed.get());
        self.closed.set(true);
        make_ready_future(())
    }

    fn list_directory(&self, _next: NextFn<DirectoryEntry>) -> Subscription<DirectoryEntry> {
        // A failure out of a method that returns no future is a panic carrying a
        // `BadFunctionCall`.
        std::panic::panic_any(make_exception_ptr(BadFunctionCall))
    }

    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        _intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        let length = self.verify_read(offset, range_size);
        make_ready_future(TemporaryBuffer::<u8>::new(length))
    }
}
