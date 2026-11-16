//! The start lines of the HTTP request and response parsers (the original's ragel grammars in
//! http/request_parser.rl and http/response_parser.rl): every state of the start line rejects a
//! byte it does not expect, and a valid line split at every position still parses.
#![cfg(target_os = "linux")]

use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::http::request_parser::HttpRequestParser;
use teseladb::engine::http::response_parser::HttpResponseParser;

/// Whether the request parser accepts `msg` fed in one buffer.
fn request_ok(msg: &[u8]) -> bool {
    let mut p = HttpRequestParser::default();
    p.init();
    let rest = p
        .call(TemporaryBuffer::copy_from(msg))
        .get()
        .expect("parse");
    rest.is_some() && !p.failed() && p.get_parsed_request().is_some()
}

/// Whether the response parser accepts `msg`.
fn response_ok(msg: &[u8]) -> bool {
    let mut p = HttpResponseParser::default();
    p.init();
    let rest = p
        .call(TemporaryBuffer::copy_from(msg))
        .get()
        .expect("parse");
    rest.is_some() && !p.failed() && p.get_parsed_response().is_some()
}

#[test]
fn request_start_lines_reject_bytes_out_of_place() {
    assert!(request_ok(b"GET / HTTP/1.1\r\n\r\n"));
    for bad in [
        &b"get / HTTP/1.1\r\n\r\n"[..], // method start: lower case
        b"GEt / HTTP/1.1\r\n\r\n",      // method: lower case inside
        b"GET  / HTTP/1.1\r\n\r\n",     // uri start: a second space
        b"GET /\r\n HTTP/1.1\r\n\r\n",  // uri: a crlf inside the start line
        b"GET / XTTP/1.1\r\n\r\n",      // H
        b"GET / HXTP/1.1\r\n\r\n",      // first T
        b"GET / HTXP/1.1\r\n\r\n",      // second T
        b"GET / HTTX/1.1\r\n\r\n",      // P
        b"GET / HTTP-1.1\r\n\r\n",      // slash
        b"GET / HTTP/x.1\r\n\r\n",      // major
        b"GET / HTTP/1,1\r\n\r\n",      // dot
        b"GET / HTTP/1.x\r\n\r\n",      // minor
        b"GET / HTTP/1.1x\r\n\r\n",     // cr after the version
        b"GET / HTTP/1.1\rx\r\n",       // lf after the cr
    ] {
        assert!(!request_ok(bad), "{:?}", String::from_utf8_lossy(bad));
    }
    // A carriage return inside the uri is part of it when no lf follows.
    assert!(request_ok(b"GET /a\rb HTTP/1.1\r\n\r\n"));
}

#[test]
fn response_start_lines_reject_bytes_out_of_place() {
    assert!(response_ok(b"HTTP/1.1 200 OK\r\n\r\n"));
    for bad in [
        &b"XTTP/1.1 200 OK\r\n\r\n"[..],
        b"HXTP/1.1 200 OK\r\n\r\n",
        b"HTXP/1.1 200 OK\r\n\r\n",
        b"HTTX/1.1 200 OK\r\n\r\n",
        b"HTTP-1.1 200 OK\r\n\r\n",
        b"HTTP/x.1 200 OK\r\n\r\n",
        b"HTTP/1,1 200 OK\r\n\r\n",
        b"HTTP/1.x 200 OK\r\n\r\n",
        b"HTTP/1.1x200 OK\r\n\r\n",
        b"HTTP/1.1 x00 OK\r\n\r\n",
        b"HTTP/1.1 2x0 OK\r\n\r\n",
        b"HTTP/1.1 20x OK\r\n\r\n",
        b"HTTP/1.1 200xOK\r\n\r\n",
        b"HTTP/1.1 200 O\nK\r\n\r\n",
        b"HTTP/1.1 200 OK\rx\r\n",
    ] {
        assert!(!response_ok(bad), "{:?}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn a_start_line_split_anywhere_still_parses() {
    let msg = b"POST /x HTTP/1.0\r\nA: b\r\n\r\n";
    for cut in 1..msg.len() {
        let mut p = HttpRequestParser::default();
        p.init();
        let first = p
            .call(TemporaryBuffer::copy_from(&msg[..cut]))
            .get()
            .expect("parse");
        // Asking for more data leaves the state the original leaves too (`error` until the
        // message completes): `failed()` only means something once a remainder is returned.
        assert!(first.is_none(), "needs more data at {cut}");
        let rest = p
            .call(TemporaryBuffer::copy_from(&msg[cut..]))
            .get()
            .expect("parse");
        assert!(rest.is_some() && !p.failed(), "cut at {cut}");
        let req = p.get_parsed_request().expect("request");
        assert_eq!(req._method.as_str(), "POST", "cut at {cut}");
        assert_eq!(req._url.as_str(), "/x", "cut at {cut}");
        assert_eq!(req._version.as_str(), "1.0", "cut at {cut}");
    }
}
