//! Chunked and content-length body sources beyond content_source_test (the original's
//! http/internal/content_source.hh): a chunk size of more digits than allowed, sizes in
//! upper-case hex, an extension repeated over chunks (its values joined with a comma), a
//! chunk longer than its size says (no cr, or a cr without its lf), a size line that does
//! not parse, a trailer that does not parse, and skipping over a content-length body (with
//! and without a byte counter).
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::collections::HashMap;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{DataSource, DataSourceImpl, InputStream};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::internal::content_source::internal::{
    ChunkTargets, ChunkedSourceImpl, ContentLengthSourceImpl,
};

/// A source that hands its buffer out at once.
struct BufSourceImpl {
    tmp: TemporaryBuffer<u8>,
}

impl DataSourceImpl for BufSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        make_ready_future(std::mem::take(&mut self.tmp))
    }

    fn skip(&mut self, n: u64) -> Future<TemporaryBuffer<u8>> {
        let k = (self.tmp.size() as u64).min(n) as usize;
        self.tmp.trim_front(k);
        make_ready_future(TemporaryBuffer::default())
    }
}

fn buf_input(s: &[u8]) -> InputStream<u8> {
    InputStream::new(DataSource::new(Box::new(BufSourceImpl {
        tmp: TemporaryBuffer::copy_from(s),
    })))
}

/// Reads the chunked body `body` to its end: the decoded bytes, the extensions and the
/// trailing headers, or the error.
fn decode(
    body: &[u8],
) -> Result<(Vec<u8>, HashMap<Sstring, Sstring>, HashMap<Sstring, Sstring>), ExceptionPtr> {
    let targets = ChunkTargets::new();
    let mut inp = buf_input(body);
    let mut ext: HashMap<Sstring, Sstring> = HashMap::new();
    let mut trailers: HashMap<Sstring, Sstring> = HashMap::new();
    // SAFETY: the stream, the maps and the targets outlive the content stream, dropped at
    // the end of this block, and are used only through it meanwhile.
    let mut content = unsafe {
        targets.set(&mut ext, &mut trailers);
        InputStream::new(DataSource::new(Box::new(ChunkedSourceImpl::new(&mut inp, &targets))))
    };
    let mut out = Vec::new();
    let r = loop {
        match content.read().get() {
            Ok(b) if b.empty() => break Ok(()),
            Ok(b) => out.extend_from_slice(b.as_slice()),
            Err(e) => break Err(e),
        }
    };
    drop(content);
    targets.detach();
    r.map(|()| (out, ext, trailers))
}

fn what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

teseladb::engine_test_case!(chunked_bodies_with_edge_cases, {
    async_(|| -> Result<(), ExceptionPtr> {
        // Upper-case hex sizes and an extension on two chunks.
        let (out, ext, trailers) =
            decode(b"A;e=1\r\n0123456789\r\nB;e=2\r\nabcdefghijk\r\n0\r\nT: v\r\n\r\n")?;
        assert_eq!(out, b"0123456789abcdefghijk");
        assert_eq!(ext[&Sstring::from("e")], "1,2");
        assert_eq!(trailers[&Sstring::from("T")], "v");

        // Seventeen hex digits are more than a chunk size may have.
        let e = decode(b"00000000000000001\r\nx\r\n0\r\n\r\n").expect_err("too long");
        assert!(what(&e).contains("Chunk length too big"), "{}", what(&e));
        // A chunk longer than its size: no cr after it, or no lf after the cr.
        let e = decode(b"2\r\nabc\r\n0\r\n\r\n").expect_err("longer");
        assert!(what(&e).contains("exceeds the specified length"), "{}", what(&e));
        let e = decode(b"2\r\nab\rc\n0\r\n\r\n").expect_err("longer");
        assert!(what(&e).contains("exceeds the specified length"), "{}", what(&e));
        // A size line and a trailer that do not parse.
        let e = decode(b"zz\r\n").expect_err("bad size");
        assert!(what(&e).contains("chunk size"), "{}", what(&e));
        let e = decode(b"0\r\n\x01bad\r\n\r\n").expect_err("bad trailer");
        assert!(what(&e).contains("trailer"), "{}", what(&e));
        Ok(())
    })
});

teseladb::engine_test_case!(content_length_bodies_can_be_skipped, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut inp = buf_input(b"0123456789rest");
        let mut consumed = 0usize;
        // SAFETY: `inp` and `consumed` outlive the source, dropped at the end of the block.
        let mut src = unsafe { ContentLengthSourceImpl::with_counter(&mut inp, 10, &mut consumed) };
        assert!(src.skip(4).get()?.empty());
        let b = src.get().get()?;
        assert_eq!(b.as_slice(), b"456789");
        // Nothing is left of the body: a skip past it skips nothing.
        assert!(src.skip(100).get()?.empty());
        src.close().get()?;
        drop(src);
        assert_eq!(consumed, 10);
        let mut inp2 = buf_input(b"abc");
        // SAFETY: as above.
        let mut plain = unsafe { ContentLengthSourceImpl::new(&mut inp2, 3) };
        assert!(plain.skip(2).get()?.empty());
        assert_eq!(plain.get().get()?.as_slice(), b"c");
        Ok(())
    })
});
