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
