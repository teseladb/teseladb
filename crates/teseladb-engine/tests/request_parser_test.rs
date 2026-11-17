//! Tests of the HTTP/1.1 request parser: header names and values, valid and malformed.

#![cfg(target_os = "linux")]

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::http::request_parser::HttpRequestParser;

struct TestSet {
    msg: &'static [u8],
    parsable: bool,
    header_name: &'static [u8],
    header_value: &'static [u8],
}

impl TestSet {
    const fn new(
        msg: &'static [u8],
        parsable: bool,
        header_name: &'static [u8],
        header_value: &'static [u8],
    ) -> Self {
        TestSet { msg, parsable, header_name, header_value }
    }

    fn buf(&self) -> TemporaryBuffer<u8> {
        TemporaryBuffer::copy_from(self.msg)
    }
}

teseladb::engine_test_case!(test_header_parsing, {
    let tests: Vec<TestSet> = vec![
            TestSet::new(b"GET /test HTTP/1.1\r\nHost: test\r\n\r\n", true, b"Host", b"test"),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader: Field\r\n\r\n", true, b"Header", b"Field"),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader: \r\n\r\n", true, b"Header", b""),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader:  f  i e l d  \r\n\r\n", true, b"Header", b"f  i e l d"),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader: fiel\r\n    d\r\n\r\n", true, b"Header", b"fiel d"),
            TestSet::new(
                b"GET /hello HTTP/1.0\r\ntchars.^_`|123: printable!@#%^&*()obs_text\x80\x81\xff\r\n\r\n",
                true,
                b"tchars.^_`|123",
                b"printable!@#%^&*()obs_text\x80\x81\xff",
            ),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader: Field\r\nHeader: Field2\r\n\r\n", true, b"Header", b"Field,Field2"),
            TestSet::new(b"GET /hello HTTP/1.0\r\n\r\n", true, b"", b""),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader : Field\r\n\r\n", false, b"", b""),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader Field\r\n\r\n", false, b"", b""),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader@: Field\r\n\r\n", false, b"", b""),
            TestSet::new(b"GET /hello HTTP/1.0\r\nHeader: fiel\r\nd \r\n\r\n", false, b"", b""),
        ];

    let mut parser = HttpRequestParser::default();
    for tset in &tests {
        parser.init();
        assert!(parser.call(tset.buf()).get().expect("parse").is_some());
        assert_ne!(parser.failed(), tset.parsable);
        if tset.parsable {
            let req = parser.get_parsed_request().expect("request");
            let name = Sstring::from_bytes(tset.header_name);
            assert_eq!(req.get_header(name.as_str()), Sstring::from_bytes(tset.header_value));
        }
    }
    make_ready_future(())
});

// The request line and headers together are bounded: past the limit the parse stops and
// fails, telling an overflowing URI (414) from overflowing header fields (431).
teseladb::engine_test_case!(test_request_size_limit, {
    let big_uri = format!("GET /{} HTTP/1.1\r\nHost: test\r\n\r\n", "a".repeat(1024));
    let big_header = format!("GET /test HTTP/1.1\r\nX-Big: {}\r\n\r\n", "a".repeat(1024));
    let tests: Vec<(String, usize, bool, bool)> = vec![
        ("GET /test HTTP/1.1\r\nHost: test\r\n\r\n".to_string(), 64, false, false),
        (big_uri.clone(), 64, true, true),      // the URI itself overflows: 414
        (big_header, 64, true, false),          // the header fields overflow: 431
        (big_uri, 4096, false, false),
    ];
    let mut parser = HttpRequestParser::default();
    for (msg, limit, too_large, uri_too_large) in &tests {
        parser.init();
        parser.set_size_limit(*limit);
        // The parse stops (an unconsumed remainder) whether it succeeds or overflows.
        assert!(parser.call(TemporaryBuffer::copy_from(msg.as_bytes())).get().expect("parse").is_some());
        assert_eq!(parser.size_limit_exceeded(), *too_large);
        // An oversized request also reports as failed.
        assert_eq!(parser.failed(), *too_large);
        assert_eq!(parser.uri_too_large(), *uri_too_large);
    }
    make_ready_future(())
});

// The limit holds when an oversized header is split across buffers.
teseladb::engine_test_case!(test_request_size_limit_scattered, {
    let part1 = format!("GET /test HTTP/1.1\r\nX-Big: {}", "a".repeat(200));
    let part2 = format!("{}\r\n\r\n", "a".repeat(400));
    let mut parser = HttpRequestParser::default();
    parser.init();
    parser.set_size_limit(512);
    assert!(parser.call(TemporaryBuffer::copy_from(part1.as_bytes())).get().expect("parse").is_none());
    assert!(!parser.size_limit_exceeded());
    assert!(parser.call(TemporaryBuffer::copy_from(part2.as_bytes())).get().expect("parse").is_some());
    assert!(parser.size_limit_exceeded());
    assert!(parser.failed());
    make_ready_future(())
});

// A head that never ends is cut at the limit: the parse stops there instead of waiting for
// more input.
teseladb::engine_test_case!(test_request_size_limit_unfinished, {
    let part1 = format!("GET /test HTTP/1.1\r\nX-Big: {}", "a".repeat(200));
    let part2 = "a".repeat(400);
    let mut parser = HttpRequestParser::default();
    parser.init();
    parser.set_size_limit(512);
    assert!(parser.call(TemporaryBuffer::copy_from(part1.as_bytes())).get().expect("parse").is_none());
    assert!(parser.call(TemporaryBuffer::copy_from(part2.as_bytes())).get().expect("parse").is_some());
    assert!(parser.size_limit_exceeded());
    assert!(parser.failed());
    assert!(!parser.uri_too_large());
    make_ready_future(())
});
