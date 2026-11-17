#![cfg(target_os = "linux")]
//! The output stream of an HTTP body with a declared Content-Length fails a write past that
//! length, also when the body is already exactly full: before, bytes written to a full body
//! were silently dropped.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::iostream::{DataSink, OutputStream, OutputStreamOptions};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::vector_data_sink::VectorDataSink;
use teseladb::engine::http::common::http::internal::make_http_content_length_output_stream;
use teseladb::engine::net::packet::Packet;
use teseladb::engine_test_case;

engine_test_case!(test_content_length_body_overflow_when_full, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let mut vec: Vec<Packet> = Vec::new();
        // SAFETY: `vec` outlives the streams, which are closed below, and is not touched
        // until then.
        let sink = unsafe { VectorDataSink::new(&mut vec) };
        let mut out =
            OutputStream::<u8>::with_options(DataSink::new(Box::new(sink)), 64, OutputStreamOptions::default());
        let mut written = 0usize;
        // SAFETY: `out` and `written` outlive the body stream, closed below.
        let mut body = unsafe { make_http_content_length_output_stream(&mut out, 4, &mut written) };

        body.write(b"abcd").get()?;
        body.flush().get()?;
        // The body is full: one more byte is an overflow.
        body.write(b"e").get()?;
        let r = body.flush().get();
        assert!(
            matches!(r, Err(ref e) if e.is::<RuntimeError>()),
            "a write past the full body was accepted: {r:?}"
        );
        drop(body);
        out.close().get()?;
        Ok(())
    })
});
