//! Tests of the HTTP/1.1 chunked transfer-encoding parsers: chunk sizes with extensions and
//! trailer headers, for valid and malformed input.

#![cfg(target_os = "linux")]

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::http::chunk_parsers::{HttpChunkSizeAndExtParser, HttpChunkTrailerParser};

struct SizeTestSet {
    msg: &'static [u8],
    parsable: bool,
    size: &'static str,
    extensions: Vec<(&'static str, &'static str)>,
}

fn st(
    msg: &'static [u8],
    parsable: bool,
    size: &'static str,
    extensions: Vec<(&'static str, &'static str)>,
) -> SizeTestSet {
    SizeTestSet { msg, parsable, size, extensions }
}

teseladb::engine_test_case!(test_size_and_extensions_parsing, {
    let tests: Vec<SizeTestSet> = vec![
        st(b"14;name=value\r\n", true, "14", vec![("name", "value")]),
        st(b"abcdef;name=value;name2=\"value2\"\r\n", true, "abcdef", vec![]),
        st(b"1efg;name=value\r\n", false, "", vec![]),
        st(
            b"aa;tchars.^_`|123=t1!#$%&'*+-.~\r\n",
            true,
            "aa",
            vec![("tchars.^_`|123", "t1!#$%&'*+-.~")],
        ),
        st(
            b"1;quoted=\"hello world\";quoted-pair=\"\\a\\b\\cd\\\\ef\"\r\n",
            true,
            "1",
            vec![("quoted", "hello world"), ("quoted-pair", "abcd\\ef")],
        ),
        st(b"2;bad-quoted-pair=\"abc\\\"\r\n", false, "", vec![]),
        st(b"3;quoted-pair-outside-quoted-string=\\q\\p\r\n", false, "", vec![]),
        st(b"4;whitespace-outside-quoted-string=quoted string\r\n", false, "", vec![]),
        st(b"5;quotation-mark-inside-quoted-string=\"quoted\"mark\"\r\n", false, "", vec![]),
        st(b"6; bad=space\r\n", false, "", vec![]),
        st(b"7;sole-name\r\n", true, "7", vec![("sole-name", "")]),
        st(b"8;empty_value=\"\"\r\n", true, "8", vec![("empty_value", "")]),
        st(b"0\r\n", true, "0", vec![]),
    ];

    let mut parser = HttpChunkSizeAndExtParser::default();
    for tset in &tests {
        parser.init();
        assert!(parser.call(TemporaryBuffer::copy_from(tset.msg)).get().expect("parse").is_some());
        assert_ne!(parser.failed(), tset.parsable);
        if tset.parsable {
            assert_eq!(parser.get_size(), tset.size);
            let exts = parser.get_parsed_extensions();
            for ext in &tset.extensions {
                assert_eq!(exts[&Sstring::from(ext.0)], ext.1);
            }
        }
    }
    make_ready_future(())
});

struct TrailerTestSet {
    msg: &'static [u8],
    parsable: bool,
    header_name: &'static [u8],
    header_value: &'static [u8],
}

fn tt(
    msg: &'static [u8],
    parsable: bool,
    header_name: &'static [u8],
    header_value: &'static [u8],
) -> TrailerTestSet {
    TrailerTestSet { msg, parsable, header_name, header_value }
}

teseladb::engine_test_case!(test_trailer_headers_parsing, {
    let tests: Vec<TrailerTestSet> = vec![
        // the headers follow the same rules as in the request parser
        tt(b"Host: test\r\n\r\n", true, b"Host", b"test"),
        tt(b"Header: Field\r\n\r\n", true, b"Header", b"Field"),
        tt(b"Header: \r\n\r\n", true, b"Header", b""),
        tt(b"Header:  f  i e l d  \r\n\r\n", true, b"Header", b"f  i e l d"),
        tt(b"Header: fiel\r\n    d\r\n\r\n", true, b"Header", b"fiel d"),
        tt(
            b"tchars.^_`|123: printable!@#%^&*()obs_text\x80\x81\xff\r\n\r\n",
            true,
            b"tchars.^_`|123",
            b"printable!@#%^&*()obs_text\x80\x81\xff",
        ),
        tt(b"Header: Field\r\nHeader: Field2\r\n\r\n", true, b"Header", b"Field,Field2"),
        tt(b"Header : Field\r\n\r\n", false, b"", b""),
        tt(b"Header Field\r\n\r\n", false, b"", b""),
        tt(b"Header@: Field\r\n\r\n", false, b"", b""),
        tt(b"Header: fiel\r\nd \r\n\r\n", false, b"", b""),
        tt(b"\r\n", true, b"", b""),
    ];

    let mut parser = HttpChunkTrailerParser::default();
    for tset in &tests {
        parser.init();
        assert!(parser.call(TemporaryBuffer::copy_from(tset.msg)).get().expect("parse").is_some());
        assert_ne!(parser.failed(), tset.parsable);
        if tset.parsable {
            let mut heads = parser.get_parsed_headers();
            // A missing entry is inserted empty before it is compared.
            let v = heads.entry(Sstring::from_bytes(tset.header_name)).or_default().clone();
            assert_eq!(v, Sstring::from_bytes(tset.header_value));
        }
    }
    make_ready_future(())
});
