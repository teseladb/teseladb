//! The header and chunk grammars of the HTTP parsers beyond chunk_parsers_test and
//! cov2_http_parsers_test (the original's ragel machines in http/request_parser.rl and
//! http/chunk_parsers.rl): every header state rejects a byte it does not expect, a chunk head
//! rejects bytes out of place in its size, names, tokens and quoted strings, and an input
//! that ends before a head or a trailer is reported as end of input, not as a failure.
#![cfg(target_os = "linux")]

use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::http::chunk_parsers::{HttpChunkSizeAndExtParser, HttpChunkTrailerParser};
use teseladb::engine::http::request_parser::HttpRequestParser;

/// Whether the request parser accepts `msg` fed in one buffer.
fn request_ok(msg: &[u8]) -> bool {
    let mut p = HttpRequestParser::default();
    p.init();
    let rest = p.call(TemporaryBuffer::copy_from(msg)).get().expect("parse");
    rest.is_some() && !p.failed() && p.get_parsed_request().is_some()
}

#[test]
fn request_headers_reject_bytes_out_of_place() {
    assert!(request_ok(b"GET / HTTP/1.1\r\nHost: a\r\n\tb\r\n\r\n"));
    for bad in [
        &b"GET / HTTP/1.1\r\n:x\r\n\r\n"[..], // a header that starts with no name
        b"GET / HTTP/1.1\r\nHo st: a\r\n\r\n", // a space inside the name
        b"GET / HTTP/1.1\r\nHost:\x01\r\n\r\n", // a control byte before the value
        b"GET / HTTP/1.1\r\nHost: a\x01\r\n\r\n", // a control byte inside the value
        b"GET / HTTP/1.1\r\nHost: a\rx\r\n",  // no lf after the value's cr
        b"GET / HTTP/1.1\r\nHost: a\r\n@\r\n\r\n", // a line that is neither a header nor a continuation
        b"GET / HTTP/1.1\r\nHost: a\r\n\rx",       // no lf after the final cr
    ] {
        assert!(!request_ok(bad), "{:?}", String::from_utf8_lossy(bad));
    }
}

/// Feeds `msg` to a fresh chunk-head parser: (failed, eof).
fn head(msg: &[u8]) -> (bool, bool) {
    let mut p = HttpChunkSizeAndExtParser::default();
    p.init();
    let _ = p.call(TemporaryBuffer::copy_from(msg)).get().expect("parse");
    (p.failed(), p.eof())
}

#[test]
fn chunk_heads_reject_bytes_out_of_place() {
    for bad in [
        &b"x\r\n"[..],          // a size that does not start with a hex digit
        b"1;a@\r\n",            // a name followed by a byte that is not '=', ';' or cr
        b"1;a=b@\r\n",          // a token value followed by such a byte
        b"1;a=\"\x01\"\r\n",    // a control byte opening a quoted string
        b"1;a=\"x\x01\"\r\n",   // a control byte inside a quoted string
        b"1;a=\"x\\\x01\"\r\n", // a quoted pair of a control byte
        b"1;a=,\r\n",           // a value that is neither a token nor a quoted string
        b"1;a=\"x\"y\r\n",      // a byte after the closing quote
        b"1\rx",                // no lf after the cr
    ] {
        assert_eq!(head(bad), (true, false), "{:?}", String::from_utf8_lossy(bad));
    }
    // An empty input ends before the head: end of input, not a failure.
    assert_eq!(head(b""), (false, true));
}

#[test]
fn a_chunk_head_and_a_trailer_split_over_buffers() {
    let mut p = HttpChunkSizeAndExtParser::default();
    p.init();
    // The first buffer needs more input (meanwhile the state reads as failed, as the
    // original's machine leaves it until the head completes).
    assert!(p.call(TemporaryBuffer::copy_from(b"1f;na")).get().expect("parse").is_none());
    assert!(!p.eof());
    assert!(p.call(TemporaryBuffer::copy_from(b"me=v\r\nrest")).get().expect("parse").is_some());
    assert!(!p.failed());
    assert_eq!(p.get_size(), "1f");
    let ext = p.get_parsed_extensions();
    assert_eq!(ext.len(), 1);

    let mut t = HttpChunkTrailerParser::default();
    t.init();
    assert!(t.call(TemporaryBuffer::copy_from(b"X-A: 1\r\n")).get().expect("parse").is_none());
    assert!(t.call(TemporaryBuffer::copy_from(b"\r\n")).get().expect("parse").is_some());
    assert!(!t.failed() && !t.eof());
    assert_eq!(t.get_parsed_headers().len(), 1);
    // An empty trailer input is end of input; a bad one fails.
    t.init();
    let _ = t.call(TemporaryBuffer::copy_from(b"")).get().expect("parse");
    assert!(t.eof() && !t.failed());
    t.init();
    let _ = t.call(TemporaryBuffer::copy_from(b"\x01\r\n")).get().expect("parse");
    assert!(t.failed() && !t.eof());
}
