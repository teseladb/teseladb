//! Tests of the HTTP body sources: a content-length source stops at the given length, and a
//! chunked source decodes chunks, chunk extensions and trailing headers, also when the
//! input arrives one byte at a time.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::collections::HashMap;

use teseladb::engine::core::future::{ExceptionPtr, Future, SystemError, make_ready_future};
use teseladb::engine::core::iostream::{DataSource, DataSourceImpl, InputStream};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::internal::content_source::internal::{
    ChunkTargets, ChunkedSourceImpl, ContentLengthSourceImpl,
};

/// `EPROTO` (`std::errc::protocol_error`).
const EPROTO: i32 = 71;

fn system_error_code(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<SystemError>().map(|err| err.code())
}

fn tb(s: &str) -> TemporaryBuffer<u8> {
    TemporaryBuffer::copy_of(s)
}

struct BufSourceImpl {
    tmp: TemporaryBuffer<u8>,
}

impl BufSourceImpl {
    fn new(str: Sstring) -> Self {
        BufSourceImpl { tmp: tb(str.as_str()) }
    }
}

impl DataSourceImpl for BufSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        if self.tmp.empty() {
            return make_ready_future(TemporaryBuffer::default());
        }
        make_ready_future(std::mem::take(&mut self.tmp))
    }

    fn skip(&mut self, n: u64) -> Future<TemporaryBuffer<u8>> {
        let k = (self.tmp.size() as u64).min(n) as usize;
        self.tmp.trim_front(k);
        make_ready_future(TemporaryBuffer::default())
    }
}

/// A stream of at most `length` bytes read from `inp`.
fn content_length_source(inp: &mut InputStream<u8>, length: usize) -> DataSource {
    // SAFETY: each test keeps `inp` alive and in place for as long as the content stream,
    // and reads it only through that stream meanwhile.
    DataSource::new(Box::new(unsafe { ContentLengthSourceImpl::new(inp, length) }))
}

/// A stream of the decoded chunks of `inp`, filling the chunk extensions and trailing
/// headers maps as it reads: the maps are reached through `targets`, which each test
/// declares before the content stream.
fn chunked_source(
    inp: &mut InputStream<u8>,
    chunk_extensions: &mut HashMap<Sstring, Sstring>,
    trailing_headers: &mut HashMap<Sstring, Sstring>,
    targets: &ChunkTargets,
) -> DataSource {
    // SAFETY: each test keeps the stream, the maps and the targets alive and in place for
    // as long as the content stream, and uses them only through it meanwhile.
    unsafe {
        targets.set(chunk_extensions, trailing_headers);
        DataSource::new(Box::new(ChunkedSourceImpl::new(inp, targets)))
    }
}

fn buf_input(s: &str) -> InputStream<u8> {
    InputStream::new(DataSource::new(Box::new(BufSourceImpl::new(Sstring::from(s)))))
}

teseladb::engine_test_case!(test_incomplete_content, {
    async_(|| -> Result<(), ExceptionPtr> {
        let targets = ChunkTargets::new();
        let mut inp = buf_input("asdfghjkl;");
        let mut content_strm = InputStream::new(content_length_source(&mut inp, 20));

        let mut content1 = content_strm.read().get()?;
        assert!(tb("asdfghjkl;") == content1);
        // The body ends before its declared length: a protocol error, not a clean end.
        let r = content_strm.read().get();
        assert_eq!(
            r.as_ref().err().and_then(system_error_code),
            Some(EPROTO),
            "{:?}",
            r.as_ref().err()
        );
        assert!(inp.eof());

        inp = buf_input("4\r\n132");
        let mut tmp: HashMap<Sstring, Sstring> = HashMap::new();
        let mut tmp2: HashMap<Sstring, Sstring> = HashMap::new();
        content_strm = InputStream::new(chunked_source(&mut inp, &mut tmp, &mut tmp2, &targets));

        content1 = content_strm.read().get()?;
        assert!(tb("132") == content1);
        // The stream ends before the last chunk: a protocol error, not a clean end.
        let r = content_strm.read().get();
        assert_eq!(
            r.as_ref().err().and_then(system_error_code),
            Some(EPROTO),
            "{:?}",
            r.as_ref().err()
        );
        Ok(())
    })
});

teseladb::engine_test_case!(test_complete_content, {
    async_(|| -> Result<(), ExceptionPtr> {
        let targets = ChunkTargets::new();
        let mut inp = buf_input("asdfghjkl;1234567890");
        let mut content_strm = InputStream::new(content_length_source(&mut inp, 20));

        let mut content1 = content_strm.read().get()?;
        assert!(tb("asdfghjkl;1234567890") == content1);
        let mut content2 = content_strm.read().get()?;
        assert!(TemporaryBuffer::<u8>::default() == content2);
        assert!(content_strm.eof());

        inp = buf_input("4\r\n1324\r\n0\r\n\r\n");
        let mut tmp: HashMap<Sstring, Sstring> = HashMap::new();
        let mut tmp2: HashMap<Sstring, Sstring> = HashMap::new();
        content_strm = InputStream::new(chunked_source(&mut inp, &mut tmp, &mut tmp2, &targets));

        content1 = content_strm.read().get()?;
        assert!(tb("1324") == content1);
        content2 = content_strm.read().get()?;
        assert!(TemporaryBuffer::<u8>::default() == content2);
        assert!(content_strm.eof());
        Ok(())
    })
});

teseladb::engine_test_case!(test_more_than_requests_content, {
    async_(|| -> Result<(), ExceptionPtr> {
        let targets = ChunkTargets::new();
        let mut inp = buf_input("asdfghjkl;1234567890xyz");
        let mut content_strm = InputStream::new(content_length_source(&mut inp, 20));

        let mut content1 = content_strm.read().get()?;
        assert!(tb("asdfghjkl;1234567890") == content1);
        let mut content2 = content_strm.read().get()?;
        assert!(TemporaryBuffer::<u8>::default() == content2);
        assert!(content_strm.eof());
        let mut content3 = inp.read().get()?;
        assert!(tb("xyz") == content3);

        inp = buf_input("4\r\n1324\r\n0\r\n\r\nxyz");
        let mut tmp: HashMap<Sstring, Sstring> = HashMap::new();
        let mut tmp2: HashMap<Sstring, Sstring> = HashMap::new();
        content_strm = InputStream::new(chunked_source(&mut inp, &mut tmp, &mut tmp2, &targets));

        content1 = content_strm.read().get()?;
        assert!(tb("1324") == content1);
        content2 = content_strm.read().get()?;
        assert!(TemporaryBuffer::<u8>::default() == content2);
        assert!(content_strm.eof());
        content3 = inp.read().get()?;
        assert!(tb("xyz") == content3);
        Ok(())
    })
});

struct SingleBytesSourceImpl {
    tmp: TemporaryBuffer<u8>,
}

impl SingleBytesSourceImpl {
    fn new(tmp: TemporaryBuffer<u8>) -> Self {
        SingleBytesSourceImpl { tmp }
    }
}

impl DataSourceImpl for SingleBytesSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        if self.tmp.empty() {
            return make_ready_future(TemporaryBuffer::default());
        }
        let byte = self.tmp.share_range(0, 1);
        self.tmp.trim_front(1);
        make_ready_future(byte)
    }

    fn skip(&mut self, n: u64) -> Future<TemporaryBuffer<u8>> {
        let k = (self.tmp.size() as u64).min(n) as usize;
        self.tmp.trim_front(k);
        make_ready_future(TemporaryBuffer::default())
    }
}

teseladb::engine_test_case!(test_single_bytes_source, {
    async_(|| -> Result<(), ExceptionPtr> {
        let input_str = Sstring::from("test input");
        let mut ds = DataSource::new(Box::new(SingleBytesSourceImpl::new(tb(input_str.as_str()))));

        for &ch in input_str.as_bytes() {
            let mut one_letter_buf = TemporaryBuffer::<u8>::new(1);
            // SAFETY: the buffer has one writable byte.
            unsafe { *one_letter_buf.get_write() = ch };
            let get_buf = ds.get().get()?;
            assert!(one_letter_buf == get_buf);
        }
        Ok(())
    })
});

teseladb::engine_test_case!(test_fragmented_chunks, {
    // Test if a message that cannot be parsed as a http request is being replied with a 400 Bad Request response
    {
        async_(|| -> Result<(), ExceptionPtr> {
            let targets = ChunkTargets::new();
            let request_string =
                Sstring::from("a;chunk=ext\r\n1234567890\r\n0\r\ntrailer: part\r\n\r\n");
            let mut inp = InputStream::new(DataSource::new(Box::new(SingleBytesSourceImpl::new(
                tb(request_string.as_str()),
            ))));
            let mut chunk_extensions: HashMap<Sstring, Sstring> = HashMap::new();
            let mut trailing_headers: HashMap<Sstring, Sstring> = HashMap::new();
            let mut content_stream = InputStream::new(chunked_source(
                &mut inp,
                &mut chunk_extensions,
                &mut trailing_headers,
                &targets,
            ));

            for &ch in Sstring::from("1234567890").as_bytes() {
                let mut one_letter_buf = TemporaryBuffer::<u8>::new(1);
                // SAFETY: the buffer has one writable byte.
                unsafe { *one_letter_buf.get_write() = ch };
                let read_buf = content_stream.read().get()?;
                assert!(one_letter_buf == read_buf);
            }
            let read_buf = content_stream.read().get()?;
            assert!(TemporaryBuffer::<u8>::default() == read_buf);
            drop(content_stream);
            assert!(chunk_extensions.entry(Sstring::from("chunk")).or_default().clone() == "ext");
            assert!(
                trailing_headers.entry(Sstring::from("trailer")).or_default().clone() == "part"
            );
            Ok(())
        })
    }
});
