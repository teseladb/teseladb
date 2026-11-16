//! Tests of reading a whole input stream (`read_all`) and skipping it (`skip_all`) over
//! sources with many buffer sizes.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{DataSource, DataSourceImpl, InputStream};
use teseladb::engine::core::sstring::{Sstring, to_sstring_buffer};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::util::short_streams::util::{
    read_entire_stream, read_entire_stream_contiguous, skip_entire_stream,
};
use teseladb::engine_test_case;

struct TestSourceImpl {
    current_letter: u16, // a-z corresponds to 0-25
    buffer_size: usize,
    remaining_size: usize,
}

impl TestSourceImpl {
    fn new(buffer_size: usize, total_size: usize) -> Self {
        TestSourceImpl { current_letter: 0, buffer_size, remaining_size: total_size }
    }
}

impl DataSourceImpl for TestSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        let len = self.buffer_size.min(self.remaining_size);
        let mut tmp = TemporaryBuffer::<u8>::new(len);
        for i in 0..len {
            // SAFETY: `tmp` has `len` writable bytes.
            unsafe { *tmp.get_write().add(i) = b'a' + self.current_letter as u8 };
            self.current_letter += 1;
            self.current_letter %= 26;
        }
        self.remaining_size -= len;
        make_ready_future(tmp)
    }

    fn skip(&mut self, mut n: u64) -> Future<TemporaryBuffer<u8>> {
        self.remaining_size -= (self.remaining_size as u64).min(n) as usize;
        n %= 26;
        self.current_letter += n as u16;
        make_ready_future(TemporaryBuffer::default())
    }
}

fn source(buffer_size: usize, total_size: usize) -> InputStream<u8> {
    InputStream::new(DataSource::new(Box::new(TestSourceImpl::new(buffer_size, total_size))))
}

engine_test_case!(test_read_all, {
    async_(|| -> Result<(), ExceptionPtr> {
        let check_read_all = |strm: &mut InputStream<u8>, test: &str| -> Result<(), ExceptionPtr> {
            // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
            let all = unsafe { read_entire_stream(strm) }.get()?;
            let mut s = Sstring::new();
            for buf in &all {
                s += &to_sstring_buffer(buf);
            }
            assert_eq!(s, test);
            Ok(())
        };
        let mut inp = source(5, 15);
        check_read_all(&mut inp, "abcdefghijklmno")?;
        assert!(inp.eof());

        let mut inp2 = source(5, 16);
        check_read_all(&mut inp2, "abcdefghijklmnop")?;
        assert!(inp2.eof());

        let mut empty_inp = source(5, 0);
        check_read_all(&mut empty_inp, "")?;
        assert!(empty_inp.eof());

        let mut inp_cont = source(5, 15);
        assert_eq!(
            // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
            unsafe { read_entire_stream_contiguous(&mut inp_cont) }.get()?,
            "abcdefghijklmno"
        );
        assert!(inp_cont.eof());

        let mut inp_cont2 = source(5, 16);
        assert_eq!(
            // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
            unsafe { read_entire_stream_contiguous(&mut inp_cont2) }.get()?,
            "abcdefghijklmnop"
        );
        assert!(inp_cont2.eof());

        let mut empty_inp_cont = source(5, 0);
        // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
        assert_eq!(unsafe { read_entire_stream_contiguous(&mut empty_inp_cont) }.get()?, "");
        assert!(empty_inp_cont.eof());
        Ok(())
    })
});

engine_test_case!(test_skip_all, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut inp = source(5, 15);
        // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
        unsafe { skip_entire_stream(&mut inp) }.get()?;
        assert!(inp.eof());
        assert!(to_sstring_buffer(&inp.read().get()?).empty());

        let mut inp2 = source(5, 16);
        // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
        unsafe { skip_entire_stream(&mut inp2) }.get()?;
        assert!(inp2.eof());
        assert!(to_sstring_buffer(&inp2.read().get()?).empty());

        let mut empty_inp = source(5, 0);
        // SAFETY: the stream stays in place and is used by nothing else until get() returns, right here.
        unsafe { skip_entire_stream(&mut empty_inp) }.get()?;
        assert!(empty_inp.eof());
        assert!(to_sstring_buffer(&empty_inp.read().get()?).empty());
        Ok(())
    })
});
