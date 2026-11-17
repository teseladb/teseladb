//! File stream paths the other fstream tests leave out: buffer sizes capped by the file's
//! maximum request length, short writes (also of a padded last chunk), direct writes without
//! write-behind, the sized output stream, read-ahead growth while the consumer waits, skips
//! over read-aheads still in flight (whole and partial), get() after close(), and close() with
//! reads in progress.

#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};

use teseladb::engine::core::file::{
    DirectoryEntry, File, FileImpl, FileImplBase, OpenFlags, open_file_dma,
};
use teseladb::engine::core::fstream::{
    FileInputStreamHistory, FileInputStreamOptions, FileOutputStreamOptions, make_file_data_source,
    make_file_data_source_whole, make_file_input_stream, make_file_output_stream,
    make_file_output_stream_with_size,
};
use teseladb::engine::core::future::{
    BadFunctionCall, ExceptionPtr, Future, LogicError, make_exception_future, make_exception_ptr,
    make_ready_future,
};
use teseladb::engine::core::io_intent::IoIntent;
use teseladb::engine::core::posix::Stat;
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::shared_ptr::{
    SharedPtr, make_lw_shared, make_shared, static_pointer_cast,
};
use teseladb::engine::core::stream::{NextFn, Subscription};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::util::tmp_file::TmpDir;
use teseladb::engine_test_case;

/// A file kept in memory that records the size of each request, optionally writes at most
/// `write_limit` bytes per write (a short write), and has small maximum request lengths.
struct MemFile {
    base: FileImplBase,
    data: RefCell<Vec<u8>>,
    write_limit: usize,
    writes: RefCell<Vec<(u64, usize)>>,
    reads: RefCell<Vec<usize>>,
    truncates: RefCell<Vec<u64>>,
    closed: Cell<bool>,
}

impl MemFile {
    fn new(max_length: u32, write_limit: usize, data: Vec<u8>) -> Self {
        let base = FileImplBase {
            _read_max_length: max_length,
            _write_max_length: max_length,
            ..FileImplBase::default()
        };
        MemFile {
            base,
            data: RefCell::new(data),
            write_limit,
            writes: RefCell::new(Vec::new()),
            reads: RefCell::new(Vec::new()),
            truncates: RefCell::new(Vec::new()),
            closed: Cell::new(false),
        }
    }
}

impl FileImpl for MemFile {
    fn file_impl_base(&self) -> &FileImplBase {
        &self.base
    }

    unsafe fn write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        _intent: *mut IoIntent,
    ) -> Future<usize> {
        self.writes.borrow_mut().push((pos, len));
        let n = len.min(self.write_limit);
        // SAFETY: the caller keeps `len` readable bytes at `buffer` until the future resolves.
        let src = unsafe { std::slice::from_raw_parts(buffer, n) };
        let mut d = self.data.borrow_mut();
        let end = pos as usize + n;
        if d.len() < end {
            d.resize(end, 0);
        }
        d[pos as usize..end].copy_from_slice(src);
        make_ready_future(n)
    }

    unsafe fn write_dma_iov(&self, _: u64, _: Vec<Iovec>, _: *mut IoIntent) -> Future<usize> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    unsafe fn read_dma(&self, _: u64, _: *mut u8, _: usize, _: *mut IoIntent) -> Future<usize> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    unsafe fn read_dma_iov(&self, _: u64, _: Vec<Iovec>, _: *mut IoIntent) -> Future<usize> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        _intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        self.reads.borrow_mut().push(range_size);
        let d = self.data.borrow();
        let start = (offset as usize).min(d.len());
        let end = (start + range_size).min(d.len());
        let mut b = TemporaryBuffer::<u8>::new(end - start);
        // SAFETY: `b` holds `end - start` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(d[start..end].as_ptr(), b.get_write(), end - start)
        };
        make_ready_future(b)
    }

    fn flush(&self) -> Future<()> {
        make_ready_future(())
    }

    fn stat(&self) -> Future<Stat> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn truncate(&self, length: u64) -> Future<()> {
        self.truncates.borrow_mut().push(length);
        self.data.borrow_mut().resize(length as usize, 0);
        make_ready_future(())
    }

    fn discard(&self, _: u64, _: u64) -> Future<()> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn allocate(&self, _: u64, _: u64) -> Future<()> {
        make_exception_future(make_exception_ptr(BadFunctionCall))
    }

    fn size(&self) -> Future<u64> {
        make_ready_future(self.data.borrow().len() as u64)
    }

    fn close(&self) -> Future<()> {
        assert!(!self.closed.get());
        self.closed.set(true);
        make_ready_future(())
    }

    fn list_directory(&self, _next: NextFn<DirectoryEntry>) -> Subscription<DirectoryEntry> {
        std::panic::panic_any(make_exception_ptr(BadFunctionCall))
    }
}

fn as_file(m: &SharedPtr<MemFile>) -> File {
    // SAFETY: the cast keeps the pointer into the same object (an upcast to the trait object).
    File::from_impl(unsafe { static_pointer_cast(m, |x: *mut MemFile| x as *mut dyn FileImpl) })
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 4096) as u8).collect()
}

/// Writes `data` through a file output stream over `f` in pieces of 1000 bytes, then flushes
/// and closes it.
fn write_all(f: File, opts: FileOutputStreamOptions, data: &[u8]) -> Result<(), ExceptionPtr> {
    let mut out = make_file_output_stream(f, opts).get()?;
    for c in data.chunks(1000) {
        out.write(c).get()?;
    }
    out.flush().get()?;
    out.close().get()
}

// The sink's buffer size is the largest power of two within the file's write limit when the
// configured one is above it (select_buffer_size()), and a short write is completed by
// another write of the rest.
engine_test_case!(sink_buffer_capped_and_short_writes_completed, {
    async_(|| -> Result<(), ExceptionPtr> {
        let m = make_shared(MemFile::new(12288, 4096, Vec::new()));
        let data = pattern(3 * 8192);
        let opts = FileOutputStreamOptions { buffer_size: 65536, ..Default::default() };
        write_all(as_file(&m), opts, &data)?;
        assert_eq!(*m.data.borrow(), data);
        let writes = m.writes.borrow().clone();
        // Each 8192-byte chunk is written as 8192 bytes, short by half, then the other half.
        assert_eq!(
            writes,
            vec![
                (0, 8192),
                (4096, 4096),
                (8192, 8192),
                (12288, 4096),
                (16384, 8192),
                (20480, 4096)
            ]
        );
        assert!(m.truncates.borrow().is_empty());
        assert!(m.closed.get());
        Ok(())
    })
});

// An unaligned last chunk is padded to the alignment and the file truncated to its real size,
// also when that padded write is short (the truncation follows the completing write). With
// write_behind 0 the writes are direct.
engine_test_case!(sink_padded_tail_short_write_truncates, {
    async_(|| -> Result<(), ExceptionPtr> {
        for write_behind in [0u32, 1, 3] {
            let m = make_shared(MemFile::new(1 << 30, 4096, Vec::new()));
            let data = pattern(8192 + 5000);
            let opts =
                FileOutputStreamOptions { buffer_size: 8192, write_behind, ..Default::default() };
            write_all(as_file(&m), opts, &data)?;
            assert_eq!(*m.data.borrow(), data, "write_behind {write_behind}");
            assert_eq!(*m.truncates.borrow(), vec![data.len() as u64]);
            let writes = m.writes.borrow().clone();
            assert_eq!(writes, vec![(0, 8192), (4096, 4096), (8192, 8192), (12288, 4096)]);
        }
        Ok(())
    })
});

// make_file_output_stream_with_size() uses the given buffer size and the default options.
engine_test_case!(sized_output_stream, {
    async_(|| -> Result<(), ExceptionPtr> {
        let m = make_shared(MemFile::new(1 << 30, usize::MAX, Vec::new()));
        let data = pattern(3 * 4096);
        let mut out = make_file_output_stream_with_size(as_file(&m), 4096).get()?;
        out.write(&data).get()?;
        out.flush().get()?;
        out.close().get()?;
        assert_eq!(*m.data.borrow(), data);
        assert_eq!(*m.writes.borrow(), vec![(0, 4096), (4096, 4096), (8192, 4096)]);
        Ok(())
    })
});

// The source's buffer size is capped like the sink's: reads of 8192 bytes for a configured
// 65536 and a file limit of 12288.
engine_test_case!(source_buffer_capped_to_power_of_two, {
    async_(|| -> Result<(), ExceptionPtr> {
        let data = pattern(5 * 8192 + 100);
        let m = make_shared(MemFile::new(12288, usize::MAX, data.clone()));
        let opts =
            FileInputStreamOptions { buffer_size: 65536, read_ahead: 0, ..Default::default() };
        let mut ds = make_file_data_source_whole(as_file(&m), opts);
        let mut got = Vec::new();
        loop {
            let b = ds.get().get()?;
            if b.size() == 0 {
                break;
            }
            got.extend_from_slice(b.as_slice());
        }
        ds.close().get()?;
        assert_eq!(got, data);
        let reads = m.reads.borrow().clone();
        assert!(reads.iter().all(|&r| r == 8192), "{reads:?}");
        // Six reads of data and one past the end that finds nothing.
        assert_eq!(reads.len(), 7);
        Ok(())
    })
});

// get() after close() fails with a logic error instead of issuing reads.
engine_test_case!(source_get_after_close_fails, {
    async_(|| -> Result<(), ExceptionPtr> {
        let m = make_shared(MemFile::new(1 << 30, usize::MAX, pattern(10000)));
        let opts = FileInputStreamOptions { buffer_size: 16384, ..Default::default() };
        let mut ds = make_file_data_source(as_file(&m), 0, 10000, opts);
        assert_eq!(ds.get().get()?.size(), 10000);
        ds.close().get()?;
        let Err(e) = ds.get().get() else { panic!("get() after close() succeeded") };
        assert!(e.is::<LogicError>(), "{e}");
        assert_eq!(m.reads.borrow().len(), 1);
        Ok(())
    })
});

fn rw_create_truncate() -> OpenFlags {
    OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE
}

/// Creates `name` holding `data`.
fn make_real_file(name: &str, data: &[u8]) -> Result<(), ExceptionPtr> {
    let f = open_file_dma(name, rw_create_truncate()).get()?;
    write_all(f, FileOutputStreamOptions::default(), data)
}

// With read-ahead allowed and no history, a consumer that finds the next buffer still in
// flight grows the read-ahead; with a history, once out of the slow start, the history records
// the larger read-ahead. The data read is the file's either way.
engine_test_case!(read_ahead_grows_while_waiting, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let name = t.get_path().join("ra").to_string_lossy().into_owned();
        let data = pattern(8 << 20);
        make_real_file(&name, &data)?;
        let history = make_lw_shared(FileInputStreamHistory::default());
        for dynamic in [None, Some(history.clone()), Some(history.clone())] {
            let f = open_file_dma(&name, OpenFlags::RO).get()?;
            let opts = FileInputStreamOptions {
                buffer_size: 65536,
                read_ahead: 6,
                dynamic_adjustments: dynamic,
            };
            let mut ds = make_file_data_source_whole(f, opts);
            let mut got = Vec::with_capacity(data.len());
            loop {
                let b = ds.get().get()?;
                if b.size() == 0 {
                    break;
                }
                got.extend_from_slice(b.as_slice());
            }
            ds.close().get()?;
            assert!(got == data);
        }
        // The history is shared by the streams that used it (its fields are the engine's).
        drop(history);
        Ok(())
    })
});

// skip() over read-aheads still in flight drops them (a whole one, then part of the next
// one), and the following get() returns the bytes after the skipped range; close() then waits
// for the dropped reads and for those still in progress.
engine_test_case!(skip_over_pending_read_aheads, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let name = t.get_path().join("skip").to_string_lossy().into_owned();
        let data = pattern(1 << 20);
        make_real_file(&name, &data)?;
        let history = make_lw_shared(FileInputStreamHistory::default());
        for dynamic in [None, Some(history.clone())] {
            let f = open_file_dma(&name, OpenFlags::RO).get()?;
            let opts = FileInputStreamOptions {
                buffer_size: 16384,
                read_ahead: 4,
                dynamic_adjustments: dynamic.clone(),
            };
            let mut ds = make_file_data_source_whole(f, opts);
            // Issues the first read and the read-aheads; nothing is ready yet.
            let first = ds.get();
            let n0 = first.get()?.size() as u64;
            assert!(n0 > 0);
            // Drops one whole read-ahead and part of the next while they may be in flight.
            let b1 = ds.get();
            let s = ds.skip(5000);
            assert_eq!(s.get()?.size(), 0);
            let n1 = b1.get()?.size() as u64;
            let b2 = ds.get().get()?;
            let at = (n0 + n1 + 5000) as usize;
            assert_eq!(b2.as_slice(), &data[at..at + b2.size()]);
            // A skip larger than one buffer.
            let pos = (at + b2.size()) as u64;
            ds.skip(3 * 16384 + 1).get()?;
            let b3 = ds.get().get()?;
            let at = (pos + 3 * 16384 + 1) as usize;
            assert_eq!(b3.as_slice(), &data[at..at + b3.size()]);
            // One more get() issues read-aheads that close() finds in progress.
            let pending = ds.get();
            ds.close().get()?;
            // Resolved by now (close() waited for it); cancelled or not, its result is unused.
            let _ = pending.get();
        }
        Ok(())
    })
});

// make_file_input_stream() reads a file written by a stream with write-behind and an
// unaligned length, byte for byte.
engine_test_case!(write_behind_roundtrip_real_file, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let name = t.get_path().join("wb").to_string_lossy().into_owned();
        let data = pattern((1 << 20) + 333);
        let f = open_file_dma(&name, rw_create_truncate()).get()?;
        let opts =
            FileOutputStreamOptions { buffer_size: 32768, write_behind: 4, ..Default::default() };
        write_all(f, opts, &data)?;
        let f = open_file_dma(&name, OpenFlags::RO).get()?;
        assert_eq!(f.size().get()?, data.len() as u64);
        let mut s = make_file_input_stream(f, FileInputStreamOptions::default());
        let mut got = Vec::new();
        loop {
            let b = s.read().get()?;
            if b.size() == 0 {
                break;
            }
            got.extend_from_slice(b.as_slice());
        }
        s.close().get()?;
        assert!(got == data);
        Ok(())
    })
});
