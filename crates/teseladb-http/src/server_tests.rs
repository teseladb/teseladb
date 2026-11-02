#![cfg_attr(coverage, coverage(off))]

use super::*;
use crate::hpack::{Encoder, Field};
use crate::http1::ResponseReader;
use crate::http2::{PREFACE, flag, kind};

fn h1() -> Conn {
    Conn::new(Some(Protocol::Http1), Settings::default(), 0)
}

fn ok(body: &str) -> Response {
    Response { status: 200, headers: Vec::new(), body: body.as_bytes().to_vec() }
}

fn ids(found: &[Incoming]) -> Vec<u32> {
    found
        .iter()
        .map(|i| match i {
            Incoming::Request { id, .. } | Incoming::Cancelled { id } => *id,
        })
        .collect()
}

/// The first answer in `bytes`.
fn first(bytes: &[u8]) -> crate::http1::Response {
    let mut r = ResponseReader::new(crate::http1::Limits::default());
    r.feed(bytes).expect("a response").expect("whole")
}

#[test]
fn http1_answers_go_out_in_the_order_of_their_requests() {
    let mut c = h1();
    let found = c.feed(b"GET /a HTTP/1.1\r\nHost: h\r\n\r\nGET /b HTTP/1.1\r\nHost: h\r\n\r\n", 5);
    assert_eq!(ids(&found), [1, 2]);
    c.respond(2, ok("b"));
    assert!(c.take_output().is_empty());
    c.respond(1, ok("a"));
    let out = String::from_utf8(c.take_output()).unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na"), "{out}");
    assert!(out.ends_with("\r\n\r\nb"), "{out}");
    assert_eq!(out.matches("HTTP/1.1").count(), 2);
    assert!(!c.is_closed());
    // An id it no longer has is ignored.
    c.respond(9, ok("x"));
    assert!(c.take_output().is_empty());
}

#[test]
fn http1_connections_close_when_asked_or_refused() {
    let mut c = h1();
    let found =
        c.feed(b"GET / HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\nGET /no HTTP/1.1\r\n", 0);
    assert_eq!(ids(&found), [1]);
    assert!(!c.is_closed());
    c.respond(1, ok(""));
    let out = String::from_utf8(c.take_output()).unwrap();
    assert!(out.contains("Connection: close"), "{out}");
    assert!(c.is_closed());
    assert_eq!(c.feed(b"more", 1), []);
    // A request without Host: 400 with its problem document, then the connection closes.
    let mut c = h1();
    assert_eq!(c.feed(b"GET / HTTP/1.1\r\n\r\n", 0), []);
    let out = c.take_output();
    let r = first(&out);
    assert_eq!(r.status, 400);
    assert_eq!(r.header("content-type"), Some(PROBLEM_JSON));
    assert_eq!(r.body, b"{\"type\":\"about:blank\",\"title\":\"Bad Request\",\"status\":400}");
    assert!(c.is_closed());
}

#[test]
fn http1_streams_go_in_chunks() {
    let mut c = h1();
    c.feed(b"GET /s HTTP/1.1\r\nHost: h\r\n\r\n", 0);
    c.start_stream(1, 200, &[("Content-Type".into(), "text/event-stream".into())]);
    c.stream(1, b"data: 1\n\n");
    c.stream(1, b"");
    c.end_stream(1);
    let out = c.take_output();
    let r = first(&out);
    assert_eq!(r.body, b"data: 1\n\n");
    // A stream not started takes no data.
    c.stream(7, b"x");
    c.end_stream(7);
    c.start_stream(7, 200, &[]);
    assert!(c.take_output().is_empty());
}

#[test]
fn deadlines_follow_partial_fields_and_idle_time() {
    let mut c = h1();
    assert_eq!(c.deadline_ms(), limits::IDLE_TIMEOUT_MS);
    c.feed(b"GET / HTTP/1.1\r\n", 1_000);
    assert_eq!(c.deadline_ms(), 1_000 + limits::HEADER_TIMEOUT_MS);
    c.feed(b"Host: h", 3_000);
    assert_eq!(c.deadline_ms(), 1_000 + limits::HEADER_TIMEOUT_MS);
    c.feed(b"\r\n\r\n", 4_000);
    assert_eq!(c.deadline_ms(), 4_000 + limits::IDLE_TIMEOUT_MS);
    c.touch(9_000);
    assert_eq!(c.deadline_ms(), 9_000 + limits::IDLE_TIMEOUT_MS);
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&[kind, flags]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn h2_hello(path: &str) -> Vec<u8> {
    let block = Encoder::new().encode(&[
        Field::new(":method", "GET"),
        Field::new(":scheme", "http"),
        Field::new(":path", path),
    ]);
    let mut b = PREFACE.to_vec();
    b.extend(frame(kind::SETTINGS, 0, 0, &[]));
    b.extend(frame(kind::HEADERS, flag::END_HEADERS | flag::END_STREAM, 1, &block));
    b
}

#[test]
fn without_tls_the_first_bytes_choose_the_protocol() {
    let mut c = Conn::new(None, Settings::default(), 0);
    assert_eq!(c.protocol(), None);
    let hello = h2_hello("/x");
    assert_eq!(c.feed(&hello[..5], 0), []);
    assert_eq!(c.protocol(), None);
    let found = c.feed(&hello[5..], 0);
    assert_eq!(c.protocol(), Some(Protocol::Http2));
    assert_eq!(ids(&found), [1]);
    c.take_output();
    c.respond(1, ok("x"));
    assert!(c.take_output().ends_with(&frame(kind::DATA, flag::END_STREAM, 1, b"x")));
    let mut c = Conn::new(None, Settings::default(), 0);
    let found = c.feed(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n", 0);
    assert_eq!((c.protocol(), ids(&found)), (Some(Protocol::Http1), vec![1]));
    let mut c = Conn::new(None, Settings::default(), 0);
    c.close();
    assert!(c.is_closed());
}

#[test]
fn http2_streams_resets_refusals_and_goaway() {
    let mut c = Conn::new(Some(Protocol::Http2), Settings::default(), 0);
    assert!(!c.take_output().is_empty());
    let found = c.feed(&h2_hello("/s"), 0);
    assert_eq!(ids(&found), [1]);
    c.take_output();
    c.start_stream(1, 200, &[]);
    c.stream(1, b"data: a\n\n");
    c.end_stream(1);
    let out = c.take_output();
    assert!(out.ends_with(&frame(kind::DATA, flag::END_STREAM, 1, b"")), "{out:?}");
    // A reset cancels the request.
    let block = Encoder::new().encode(&[
        Field::new(":method", "POST"),
        Field::new(":scheme", "http"),
        Field::new(":path", "/p"),
    ]);
    let mut c = Conn::new(Some(Protocol::Http2), Settings::default(), 0);
    let mut b = PREFACE.to_vec();
    b.extend(frame(kind::SETTINGS, 0, 0, &[]));
    b.extend(frame(kind::HEADERS, flag::END_HEADERS, 1, &block));
    b.extend(frame(kind::RST_STREAM, 0, 1, &8u32.to_be_bytes()));
    assert_eq!(c.feed(&b, 0), [Incoming::Cancelled { id: 1 }]);
    // A body past its limit is answered here with 413.
    let settings = Settings {
        http2: http2::Settings { max_body: 1, ..http2::Settings::default() },
        ..Settings::default()
    };
    let mut c = Conn::new(Some(Protocol::Http2), settings, 0);
    c.take_output();
    let mut b = PREFACE.to_vec();
    b.extend(frame(kind::SETTINGS, 0, 0, &[]));
    b.extend(frame(kind::HEADERS, flag::END_HEADERS, 1, &block));
    b.extend(frame(kind::DATA, flag::END_STREAM, 1, b"xy"));
    assert_eq!(c.feed(&b, 0), []);
    let out = c.take_output();
    assert!(out.windows(9).any(|w| w == b"\"status\":"), "{out:?}");
    // A broken connection closes; a GOAWAY of the server ends it.
    let mut c = Conn::new(Some(Protocol::Http2), Settings::default(), 0);
    c.feed(b"GET / HTTP/1.1\r\n\r\n", 0);
    assert!(c.is_closed());
    let mut c = Conn::new(Some(Protocol::Http2), Settings::default(), 0);
    c.take_output();
    c.close();
    assert!(c.is_closed());
    assert_eq!(c.take_output()[3], kind::GOAWAY);
}

#[test]
fn problems_name_their_status() {
    let p = Response::problem(429);
    assert_eq!(
        p.body,
        b"{\"type\":\"about:blank\",\"title\":\"Too Many Requests\",\"status\":429}"
    );
}
