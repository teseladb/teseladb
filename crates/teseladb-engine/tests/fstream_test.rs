//! Tests of file streams: writes and reads with padding and truncation, consumers that skip
//! bytes or read to the end, large and unaligned files, the adaptive read-ahead buffer, and
//! streams over a mock file.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/closers.rs"]
mod closers;
#[path = "common/minstd.rs"]
mod minstd;
#[path = "common/mock_file.rs"]
mod mock_file;

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use closers::CloseRef;
use minstd::UniformIntDistribution;
use mock_file::MockReadOnlyFile;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::file::{File, FileImpl, OpenFlags, open_file_dma};
use teseladb::engine::core::fstream::{
    FileInputStreamHistory, FileInputStreamOptions, FileOutputStreamOptions,
    make_file_input_stream, make_file_input_stream_range, make_file_output_stream,
};
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{
    ConsumptionResult, InputStream, InputStreamConsumer, ObsoleteConsumer, OutputStream,
    UnconsumedRemainder, continue_consuming, skip_bytes, stop_consuming,
};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::shared_ptr::{make_lw_shared, make_shared, static_pointer_cast};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::testing::random::testing::LOCAL_RANDOM_ENGINE;
use teseladb::engine::util::closeable::DeferredClose;
use teseladb::engine::util::defer::defer;
use teseladb::engine::util::tmp_file::TmpDir;
use teseladb::engine_test_case;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
}

/// Read-write, created if missing, truncated.
fn rw_create_truncate() -> OpenFlags {
    OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE
}

/// An output stream over a file.
struct Writer {
    out: OutputStream<u8>,
}

impl Writer {
    fn make(f: File) -> Future<Writer> {
        make_file_output_stream(f, FileOutputStreamOptions::default())
            .then(|os: OutputStream<u8>| make_ready_future(Writer { out: os }))
    }
}

/// An input stream over a file.
struct Reader {
    in_: InputStream<u8>,
}

impl Reader {
    fn new(f: File) -> Self {
        Reader { in_: make_file_input_stream(f, FileInputStreamOptions::default()) }
    }

    fn with_options(f: File, options: FileInputStreamOptions) -> Self {
        Reader { in_: make_file_input_stream(f, options) }
    }
}

fn testfile(t: &TmpDir) -> String {
    t.get_path().join("testfile.tmp").to_string_lossy().into_owned()
}

/// `n` zeroed bytes from `malloc`, freed by the caller.
fn zeroed_malloc(n: usize) -> *mut u8 {
    // SAFETY: malloc has no preconditions; the block is zeroed before use.
    let buf = unsafe { malloc(n) } as *mut u8;
    // SAFETY: `buf` is a live block of `n` bytes.
    unsafe { std::ptr::write_bytes(buf, 0, n) };
    buf
}

engine_test_case!(test_fstream, {
    TmpDir::do_with_default(|t: &mut TmpDir| {
        let filename = testfile(t);
        make_coroutine(async move {
            let f = open_file_dma(&filename, rw_create_truncate()).await?;
            let mut w = Writer::make(f).await?;
            let buf = zeroed_malloc(4096);
            // SAFETY: `buf` is a live block of 4096 bytes.
            unsafe {
                *buf = b'[';
                *buf.add(1) = b'A';
                *buf.add(4095) = b']';
            }
            // SAFETY: `buf` is a live, initialized block of 4096 bytes, freed only after the write.
            w.out.write(unsafe { std::slice::from_raw_parts(buf, 4096) }).await?;
            // SAFETY: `buf` is live and no longer used.
            unsafe { free(buf as *mut c_void) };

            let buf = zeroed_malloc(8192);
            // SAFETY: `buf` is a live block of 8192 bytes.
            unsafe {
                *buf = b'[';
                *buf.add(1) = b'B';
                *buf.add(8191) = b']';
            }
            // SAFETY: `buf` is a live, initialized block of 8192 bytes, freed only after the write.
            w.out.write(unsafe { std::slice::from_raw_parts(buf, 8192) }).await?;
            // SAFETY: as above.
            unsafe { free(buf as *mut c_void) };
            w.out.close().await?;

            let f = open_file_dma(&filename, OpenFlags::RO).await?;
            /*  file content after running the above:
             * 00000000  5b 41 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |[A..............|
             * 00000010  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
             * *
             * 00000ff0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 5d  |...............]|
             * 00001000  5b 42 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |[B..............|
             * 00001010  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
             * *
             * 00002ff0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 5d  |...............]|
             * 00003000
             */
            let mut r = Reader::new(f);
            let res = r.in_.read_exactly(4096 + 8192).await;
            let res = match res {
                Ok(buf) => {
                    let p = buf.as_slice();
                    assert!(p[0] == b'[' && p[1] == b'A' && p[4095] == b']');
                    assert!(p[4096] == b'[' && p[4096 + 1] == b'B' && p[4096 + 8191] == b']');
                    r.in_.close().await
                }
                Err(e) => Err(e),
            };
            drop(r);
            res
        })
    })
});

/// The consumer of `test_consume_skip_bytes`.
#[derive(Default)]
struct SkipConsumer {
    count: u64,
}

impl InputStreamConsumer<u8> for SkipConsumer {
    /*
     * Consumer reads the file as follows:
     *  - first 8000 bytes are read in 512-byte chunks and checked
     *  - next 2000 bytes are skipped (jumping over both read buffer size and DMA block)
     *  - the remaining 6384 bytes are read and checked
     */
    fn call(&mut self, mut buf: TemporaryBuffer<u8>) -> Future<ConsumptionResult<u8>> {
        if self.count < 8000 {
            let delta = std::cmp::min(buf.size() as u64, 8000 - self.count) as usize;
            for &c in buf.share_range(0, delta).as_slice() {
                assert_eq!(c, b'a');
            }
            buf.trim_front(delta);
            self.count += delta as u64;

            if self.count == 8000 {
                make_ready_future(skip_bytes(2000 - buf.size() as u64))
            } else {
                assert!(buf.empty());
                make_ready_future(continue_consuming())
            }
        } else {
            for &c in buf.as_slice() {
                assert_eq!(c, b'b');
            }
            self.count += buf.size() as u64;
            if self.count < 14384 {
                return make_ready_future(continue_consuming());
            } else if self.count > 14384 {
                panic!("Read more than expected");
            }
            make_ready_future(stop_consuming(TemporaryBuffer::empty_buffer()))
        }
    }
}

engine_test_case!(test_consume_skip_bytes, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t);
        let f = open_file_dma(&filename, rw_create_truncate()).get()?;
        let mut w = Writer::make(f).get()?;
        let mut write_block = |c: u8, size: usize| -> Result<(), ExceptionPtr> {
            let vec: Vec<u8> = vec![c; size];
            w.out.write(&vec).get()
        };
        write_block(b'a', 8192)?;
        write_block(b'b', 8192)?;
        w.out.close().get()?;
        /*  file content after running the above:
         * 00000000  61 61 61 61 61 61 61 61  61 61 61 61 61 61 61 61  |aaaaaaaaaaaaaaaa|
         * *
         * 00002000  62 62 62 62 62 62 62 62  62 62 62 62 62 62 62 62  |bbbbbbbbbbbbbbbb|
         * *
         * 00004000
         */
        let f = open_file_dma(&filename, OpenFlags::RO).get()?;
        let r = make_lw_shared(Reader::with_options(
            f,
            FileInputStreamOptions { buffer_size: 512, ..Default::default() },
        ));
        // SAFETY: the reader was just created and is used from this thread only, through
        // this reference.
        let rr = unsafe { r.get_mut_unchecked() };
        let rinp: *mut InputStream<u8> = &mut rr.in_;
        // SAFETY: the reader outlives the deferred close, declared after it.
        let mut rinp_ref = unsafe { CloseRef::new(rinp) };
        let _close_r_in = DeferredClose::new(&mut rinp_ref);
        // SAFETY: as above.
        unsafe { (*rinp).consume(SkipConsumer::default()) }.get()?;
        Ok(())
    })
});

engine_test_case!(test_fstream_unaligned, {
    TmpDir::do_with_default(|t: &mut TmpDir| {
        let filename = testfile(t);
        make_coroutine(async move {
            let f = open_file_dma(&filename, rw_create_truncate()).await?;
            let mut w = Writer::make(f).await?;
            let buf = zeroed_malloc(40);
            // SAFETY: `buf` is a live block of 40 bytes.
            unsafe {
                *buf = b'[';
                *buf.add(1) = b'A';
                *buf.add(39) = b']';
            }
            // SAFETY: `buf` is a live, initialized block of 40 bytes, freed only after the write.
            w.out.write(unsafe { std::slice::from_raw_parts(buf, 40) }).await?;
            // SAFETY: `buf` is live and no longer used.
            unsafe { free(buf as *mut c_void) };
            w.out.close().await?;

            let f = open_file_dma(&filename, OpenFlags::RO).await?;
            let size = f.size().await?;
            // assert that file was indeed truncated to the amount of bytes written.
            assert!(size == 40);
            drop(f);

            let f = open_file_dma(&filename, OpenFlags::RO).await?;
            let mut r = Reader::new(f);
            let res = match r.in_.read_exactly(40).await {
                Ok(buf) => {
                    let p = buf.as_slice();
                    assert!(p[0] == b'[' && p[1] == b'A' && p[39] == b']');
                    r.in_.close().await
                }
                Err(e) => Err(e),
            };
            drop(r);
            res
        })
    })
});

fn test_consume_until_end(size: u64) -> Future<()> {
    TmpDir::do_with_default(move |t: &mut TmpDir| {
        let filename = testfile(t);
        make_coroutine(async move {
            let mut f = open_file_dma(&filename, rw_create_truncate()).await?;
            let body = async {
                let mut out =
                    make_file_output_stream(f.clone(), FileOutputStreamOptions::default()).await?;
                // Fill the buffer with 0, 1, 2, ...: the values wrap at 256.
                let buf: Vec<u8> = (0..size).map(|i| i as u8).collect();
                out.write(&buf).await?;
                out.flush().await?;
                drop(out);

                let real_size = f.size().await?;
                assert_eq!(size, real_size);

                let mut offset: u64 = 0;
                let consumer = move |buf: TemporaryBuffer<u8>| -> Future<UnconsumedRemainder<u8>> {
                    if buf.empty() {
                        return make_ready_future(Some(TemporaryBuffer::empty_buffer()));
                    }
                    assert!(offset + buf.size() as u64 <= size);
                    let expected: Vec<u8> =
                        (0..buf.size() as u64).map(|i| (offset + i) as u8).collect();
                    offset += buf.size() as u64;
                    assert!(buf.as_slice() == expected.as_slice());
                    make_ready_future(None)
                };
                let mut in_ = make_file_input_stream(f.clone(), FileInputStreamOptions::default());
                let mut consumer = ObsoleteConsumer(consumer);
                // SAFETY: `consumer` lives in this async block until the awaited consume resolves,
                // and nothing else uses it meanwhile.
                unsafe { in_.consume_ref(&mut consumer) }.await?;
                in_.close().await
            }
            .await;
            // Close the file whatever the outcome.
            let closed = f.close().await;
            body.and(closed)
        })
    })
}

engine_test_case!(test_consume_aligned_file, { test_consume_until_end(4096) });

engine_test_case!(test_consume_empty_file, { test_consume_until_end(0) });

engine_test_case!(test_consume_unaligned_file, { test_consume_until_end(1) });

engine_test_case!(test_consume_unaligned_file_large, { test_consume_until_end((1 << 20) + 1) });

engine_test_case!(test_input_stream_esp_around_eof, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let flen: u64 = 5341;
        // Values in [0, 127].
        let rdist = UniformIntDistribution::new(0, i8::MAX as u64);
        // A copy of the local random engine.
        let mut reng = LOCAL_RANDOM_ENGINE.with(|e| e.get());
        let data: Vec<u8> =
            (0..flen).map(|_| rdist.sample_fn(&mut || u64::from(reng.next())) as u8).collect();
        let filename = testfile(t);
        let mut f = open_file_dma(&filename, rw_create_truncate()).get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        let mut out =
            make_file_output_stream(f.clone(), FileOutputStreamOptions::default()).get()?;
        out.write(&data).get()?;
        out.flush().get()?;
        //out.close().get();  // FIXME: closes underlying stream:?!

        /// A byte range of the file.
        struct Range {
            start: u64,
            end: u64,
        }
        let ranges: Vec<Range> = vec![
            Range { start: 0, end: flen },
            Range { start: 0, end: flen * 2 },
            Range { start: 0, end: flen + 1 },
            Range { start: 0, end: flen - 1 },
            Range { start: 0, end: 1 },
            Range { start: 1, end: 2 },
            Range { start: flen - 1, end: flen },
            Range { start: flen - 1, end: flen + 1 },
            Range { start: flen, end: flen + 1 },
            Range { start: flen + 1, end: flen + 2 },
            Range { start: 1023, end: flen - 1 },
            Range { start: 1023, end: flen },
            Range { start: 1023, end: flen + 2 },
            Range { start: 8193, end: 8194 },
            Range { start: 1023, end: 1025 },
            Range { start: 1023, end: 1024 },
            Range { start: 1024, end: 1025 },
            Range { start: 1023, end: 4097 },
        ];
        let opt = FileInputStreamOptions { buffer_size: 512, ..FileInputStreamOptions::default() };
        for r in &ranges {
            let start = r.start;
            let end = r.end;
            let len = end - start;
            let mut in_ = make_file_input_stream_range(f.clone(), start, len, opt.clone());
            let mut readback: Vec<u8> = Vec::new();
            let mut more = true;
            while more {
                let rdata = in_.read().get()?;
                for i in 0..rdata.size() {
                    readback.push(rdata.as_slice()[i]);
                }
                more = !rdata.empty();
            }
            //in.close().get();
            let xlen = std::cmp::min(end, flen) - std::cmp::min(flen, start);
            if xlen != readback.len() as u64 {
                panic!(
                    "Expected {} bytes but got {}, start={}, end={}",
                    xlen,
                    readback.len(),
                    start,
                    end
                );
            }
            let from = std::cmp::min(start, flen) as usize;
            assert!(readback.as_slice() == &data[from..from + readback.len()]);
        }
        drop(out);
        Ok(())
    })
});

engine_test_case!(without_api_prefix, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t);
        let f = open_file_dma(&filename, rw_create_truncate()).get()?;
        let mut out: OutputStream<u8> =
            make_file_output_stream(f, FileOutputStreamOptions::default()).get()?;
        out.close().get()
    })
});

engine_test_case!(file_handle_test, {
    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let filename = testfile(t);
        let mut f =
            open_file_dma(&filename, OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::RW)
                .get()?;
        let fp: *mut File = &mut f;
        // SAFETY: `f` outlives the deferred close, declared after it.
        let mut fp_ref = unsafe { CloseRef::new(fp) };
        let _close_f = DeferredClose::new(&mut fp_ref);
        // SAFETY: 4096 is a power of two and a multiple of itself.
        let buf = unsafe { aligned_alloc(4096, 4096) } as *mut u8;
        // SAFETY: `buf` is live until this deferred action frees it.
        let _del = defer(move || unsafe { free(buf as *mut c_void) });
        for i in 0..4096usize {
            // SAFETY: `i` is within the 4096-byte block.
            unsafe { *buf.add(i) = i as u8 };
        }
        // SAFETY: the buffer holds 4096 readable bytes and stays alive and untouched until get() returns, right here; no intent.
        unsafe { f.dma_write(0, buf, 4096, std::ptr::null_mut()) }.get()?;

        // A vector of bools would do as well; bytes keep it simple
        let bad: Arc<Vec<AtomicU32>> =
            Arc::new((0..Smp::count()).map(|_| AtomicU32::new(0)).collect());
        let fh = f.dup()?;
        let bad1 = bad.clone();
        Smp::invoke_on_all(move || {
            let fh = fh.clone();
            let bad = bad1.clone();
            async_(move || -> Result<(), ExceptionPtr> {
                let mut f = fh.to_file();
                // SAFETY: as above.
                let buf = unsafe { aligned_alloc(4096, 4096) } as *mut u8;
                // SAFETY: `buf` is live until this deferred action frees it.
                let _del = defer(move || unsafe { free(buf as *mut c_void) });
                // SAFETY: the buffer holds 4096 writable bytes and stays alive and untouched until
                // get() returns, right here; no intent.
                unsafe { f.dma_read(0, buf, 4096, std::ptr::null_mut()) }.get()?;
                for i in 0..4096usize {
                    // SAFETY: `i` is within the block just read.
                    let differs = unsafe { *buf.add(i) } != i as u8;
                    bad[this_shard_id() as usize].fetch_or(differs as u32, Ordering::Relaxed);
                }
                Ok(())
            })
        })
        .get()?;
        assert!(!bad.iter().any(|b| b.load(Ordering::Relaxed) == 1));
        Ok(())
    })
});

engine_test_case!(test_fstream_slow_start, {
    async_(|| -> Result<(), ExceptionPtr> {
        const FILE_SIZE: usize = 128 * 1024 * 1024;
        const BUFFER_SIZE: usize = 260 * 1024;
        const READ_AHEAD: usize = 1;

        let mock = make_shared(MockReadOnlyFile::new(FILE_SIZE as u64));
        // The setters take `&self` (the mock keeps its state in cells).
        let mf = || &*mock;

        let history = make_lw_shared(FileInputStreamHistory::default());
        let options = FileInputStreamOptions {
            buffer_size: BUFFER_SIZE,
            read_ahead: READ_AHEAD as u32,
            dynamic_adjustments: Some(history),
        };

        const REQUESTS_AT_SLOW_START: usize = 2; // 1 request + 1 read-ahead
        const REQUESTS_AT_FULL_SPEED: usize = READ_AHEAD + 1; // 1 request + read_ahead

        let initial_read_size: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));

        /// A wrapper around an input stream, for the read-ahead buffer checks.
        struct FstreamWrapper {
            s: InputStream<u8>,
        }
        impl FstreamWrapper {
            fn new(s: InputStream<u8>) -> Self {
                FstreamWrapper { s }
            }
            fn read(&mut self) -> Future<TemporaryBuffer<u8>> {
                self.s.read()
            }
            fn skip(&mut self, n: u64) -> Future<()> {
                self.s.skip(n)
            }
        }
        impl Drop for FstreamWrapper {
            fn drop(&mut self) {
                self.s.close().get().expect("close");
            }
        }

        let read_whole_file_with_slow_start =
            |mut fstr: FstreamWrapper| -> Result<(), ExceptionPtr> {
                let mut total_read: u64 = 0;
                let previous_buffer_length: Rc<Cell<usize>> = Rc::new(Cell::new(0));

                // We don't want to assume too much about fstream internals, but with
                // no history we should start with a buffer sizes somewhere in
                // (0, buffer_size) range.
                {
                    let irs = initial_read_size.clone();
                    let pbl = previous_buffer_length.clone();
                    mf().set_read_size_verifier(move |length: usize| {
                        assert!(length <= irs.get().unwrap_or(BUFFER_SIZE - 1));
                        assert!(length >= irs.get().unwrap_or(1));
                        pbl.set(length);
                        if irs.get().is_none() {
                            irs.set(Some(length));
                        }
                    });
                }

                // Slow start phase
                loop {
                    // We should leave slow start before reading the whole file.
                    assert!(total_read < FILE_SIZE as u64);

                    mf().set_allowed_read_requests(REQUESTS_AT_SLOW_START);
                    let buf = fstr.read().get()?;
                    assert!(buf.size() > 0usize);

                    let pbl = previous_buffer_length.clone();
                    mf().set_read_size_verifier(move |length: usize| {
                        // There is no reason to reduce buffer size.
                        assert!(length <= std::cmp::min(pbl.get() * 2, BUFFER_SIZE));
                        assert!(length >= pbl.get());
                        pbl.set(length);
                    });

                    println!("Size {}", buf.size());
                    total_read += buf.size() as u64;
                    if buf.size() == BUFFER_SIZE {
                        println!("Leaving slow start phase.");
                        break;
                    }
                }

                // Reading at full speed now
                mf().set_expected_read_size(BUFFER_SIZE);
                while total_read != FILE_SIZE as u64 {
                    mf().set_allowed_read_requests(REQUESTS_AT_FULL_SPEED);
                    let buf = fstr.read().get()?;
                    total_read += buf.size() as u64;
                }

                mf().set_allowed_read_requests(REQUESTS_AT_FULL_SPEED);
                let buf = fstr.read().get()?;
                assert_eq!(buf.size(), 0usize);
                assert!(buf.size() == 0);
                Ok(())
            };

        let read_while_file_at_full_speed = |mut fstr: FstreamWrapper| -> Result<(), ExceptionPtr> {
            let mut total_read: u64 = 0;

            mf().set_expected_read_size(BUFFER_SIZE);
            while total_read != FILE_SIZE as u64 {
                mf().set_allowed_read_requests(REQUESTS_AT_FULL_SPEED);
                let buf = fstr.read().get()?;
                total_read += buf.size() as u64;
            }

            mf().set_allowed_read_requests(REQUESTS_AT_FULL_SPEED);
            let buf = fstr.read().get()?;
            assert_eq!(buf.size(), 0usize);
            Ok(())
        };

        let read_and_skip_a_lot = |mut fstr: FstreamWrapper| -> Result<(), ExceptionPtr> {
            let mut total_read: u64 = 0;
            let previous_buffer_size: Rc<Cell<usize>> = Rc::new(Cell::new(BUFFER_SIZE));

            mf().set_allowed_read_requests(usize::MAX);
            {
                let irs = initial_read_size.clone();
                let pbs = previous_buffer_size.clone();
                mf().set_read_size_verifier(move |length: usize| {
                    // There is no reason to reduce buffer size.
                    assert!(length <= pbs.get());
                    assert!(length >= irs.get().unwrap_or(1));
                    pbs.set(length);
                });
            }

            while total_read != FILE_SIZE as u64 {
                let buf = fstr.read().get()?;
                total_read += buf.size() as u64;

                let buf = fstr.read().get()?;
                total_read += buf.size() as u64;

                let skip_by =
                    std::cmp::min(FILE_SIZE as u64 - total_read, (BUFFER_SIZE * 2) as u64);
                fstr.skip(skip_by).get()?;
                total_read += skip_by;
            }

            // We should be back at slow start at this stage.
            assert!(previous_buffer_size.get() < BUFFER_SIZE);
            if let Some(irs) = initial_read_size.get() {
                assert_eq!(previous_buffer_size.get(), irs);
            }

            mf().set_allowed_read_requests(REQUESTS_AT_FULL_SPEED);
            let buf = fstr.read().get()?;
            assert_eq!(buf.size(), 0usize);
            Ok(())
        };

        let make_fstream = || {
            FstreamWrapper::new(make_file_input_stream_range(
                File::from_impl({
                    // SAFETY: the cast keeps the pointer into the same object (an
                    // upcast to the `FileImpl` trait object).
                    unsafe {
                        static_pointer_cast(&mock, |x: *mut MockReadOnlyFile| {
                            x as *mut dyn FileImpl
                        })
                    }
                }),
                0,
                FILE_SIZE as u64,
                options.clone(),
            ))
        };

        println!("Reading file, no history, expectiong a slow start");
        read_whole_file_with_slow_start(make_fstream())?;
        println!("Reading file again, everything good so far, read at full speed");
        read_while_file_at_full_speed(make_fstream())?;
        println!("Reading and skipping a lot");
        read_and_skip_a_lot(make_fstream())?;
        println!("Reading file, bad history, we are back at slow start...");
        read_whole_file_with_slow_start(make_fstream())?;
        println!("Reading file yet again, should've recovered by now");
        read_while_file_at_full_speed(make_fstream())?;
        Ok(())
    })
});

#[cfg(feature = "alloc_failure_injection")]
engine_test_case!(test_close_error, {
    use teseladb::engine::core::future::BadAlloc;
    use teseladb::engine::util::alloc_failure_injector::memory::local_failure_injector;
    use teseladb::engine::util::file::remove_file;

    TmpDir::do_with_thread(|t: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let mut done = false;
        let mut i: usize = 0;
        while !done {
            let mut got_close_error = false;
            let filename =
                t.get_path().join(format!("testfile-{}.tmp", i)).to_string_lossy().into_owned();
            let f = open_file_dma(&filename, rw_create_truncate()).get()?;
            let opts = FileOutputStreamOptions { write_behind: 16, ..Default::default() };
            let mut out: Option<Box<OutputStream<u8>>> =
                Some(Box::new(make_file_output_stream(f, opts).get()?));
            let size: usize = 4096;
            let buf: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let file_length: usize = 1024 * 1024;
            let outp: *mut OutputStream<u8> =
                out.as_deref_mut().map_or(std::ptr::null_mut(), |o| o as *mut _);
            let bufp = buf.as_ptr();
            let mut fut = make_ready_future(());
            let mut len = 0;
            while len < file_length {
                // SAFETY: the stream and the buffer outlive the chain, waited for below.
                fut = fut.finally(move || unsafe {
                    (*outp).write(std::slice::from_raw_parts(bufp, size))
                });
                len += size;
            }
            fut.get()?;
            local_failure_injector().fail_after(i as u64);
            // SAFETY: as above.
            match unsafe { (*outp).close() }.get() {
                Ok(()) => {
                    done = true;
                    local_failure_injector().cancel();
                }
                Err(e) if e.is::<BadAlloc>() => {
                    got_close_error = true;
                }
                Err(e) => return Err(e),
            }
            assert!(got_close_error || done);
            out = None;
            assert!(out.is_none());
            remove_file(&filename).get()?;
            i += 1;
        }
        Ok(())
    })
});
