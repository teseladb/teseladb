//! A chunked body that arrives in pieces cut right after a chunk's CR and right after its LF
//! (the parser asks for more data at each cut), decoded with no extension or trailer targets
//! (they are discarded), then the content source is closed; and the default chunk targets.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::collections::VecDeque;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{DataSource, DataSourceImpl, InputStream};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::internal::content_source::internal::{ChunkTargets, ChunkedSourceImpl};

/// A source that hands out its pieces one per read.
struct PiecesImpl {
    pieces: VecDeque<TemporaryBuffer<u8>>,
}

impl DataSourceImpl for PiecesImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        make_ready_future(self.pieces.pop_front().unwrap_or_default())
    }
}

fn pieces_input(pieces: &[&[u8]]) -> InputStream<u8> {
    let pieces = pieces.iter().map(|p| TemporaryBuffer::copy_from(p)).collect();
    InputStream::new(DataSource::new(Box::new(PiecesImpl { pieces })))
}

teseladb::engine_test_case!(chunks_split_at_cr_and_lf_without_targets, {
    async_(|| -> Result<(), ExceptionPtr> {
        let pieces: [&[u8]; 7] = [
            b"5;name=v\r\nhello\r",
            b"\n",
            b"6\r\n world\r",
            b"\n3\r\n!!!",
            b"\r\n",
            b"0\r\nX-Trailer: t\r\n",
            b"\r\n",
        ];
        let mut inp = pieces_input(&pieces);
        let targets = ChunkTargets::default();
        // SAFETY: the stream and the targets outlive the content stream, closed and dropped
        // at the end of this block, and are used only through it meanwhile.
        let mut content: InputStream<u8> = unsafe {
            InputStream::new(DataSource::new(Box::new(ChunkedSourceImpl::new(&mut inp, &targets))))
        };
        let mut out = Vec::new();
        loop {
            let b = content.read().get()?;
            if b.empty() {
                break;
            }
            out.extend_from_slice(b.as_slice());
        }
        content.close().get()?;
        drop(content);
        assert_eq!(out, b"hello world!!!");
        Ok(())
    })
});
