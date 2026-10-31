//! HTTP/1.1 messages (RFC 9112) as the clients of STS and JWKS write and read them (013 T036,
//! FR-025): requests with their framing, responses whole or in any pieces, with a length, in
//! chunks or until the connection closes, and every malformed or oversized message refused.

use teseladb_http::client::{Scheme, Url, get, post};
use teseladb_http::http1::{HttpError, Limits, Request, ResponseReader};

const OK_LEN: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"keys\": []}\n";

fn read_all(bytes: &[u8]) -> Result<Option<teseladb_http::http1::Response>, HttpError> {
    ResponseReader::new(Limits::default()).feed(bytes)
}

#[test]
fn requests_carry_their_framing() {
    let r = Request::new("POST", "/")
        .header("Host", "sts.amazonaws.com")
        .body(b"Action=GetCallerIdentity".to_vec());
    assert_eq!(
        r.encode().unwrap(),
        b"POST / HTTP/1.1\r\nHost: sts.amazonaws.com\r\nContent-Length: 24\r\n\r\nAction=GetCallerIdentity".to_vec()
    );
    let r = Request::new("GET", "/.well-known/jwks.json").header("Host", "idp.example");
    assert_eq!(
        r.encode().unwrap(),
        b"GET /.well-known/jwks.json HTTP/1.1\r\nHost: idp.example\r\n\r\n".to_vec()
    );
    // No Host, a bad method, target or field: refused before anything is written.
    assert_eq!(Request::new("GET", "/").encode(), Err(HttpError::BadRequest("no Host field")));
    for (method, target, name, value) in [
        ("G ET", "/", "Host", "h"),
        ("GET", "/a b", "Host", "h"),
        ("GET", "", "Host", "h"),
        ("GET", "/", "Ho st", "h"),
        ("GET", "/", "Host", "h\r\nX: y"),
    ] {
        let r = Request::new(method, target).header(name, value);
        assert!(matches!(r.encode(), Err(HttpError::BadRequest(_))), "{method} {target} {name}");
    }
}

#[test]
fn a_response_with_a_length_reads_in_any_pieces() {
    let whole = read_all(OK_LEN).unwrap().unwrap();
    assert_eq!((whole.status, whole.reason.as_str()), (200, "OK"));
    assert_eq!(whole.header("content-type"), Some("application/json"));
    assert_eq!(whole.body, b"{\"keys\": []}\n");
    for cut in 0..OK_LEN.len() {
        let mut r = ResponseReader::new(Limits::default());
        assert_eq!(r.feed(&OK_LEN[..cut]).unwrap(), None, "{cut}");
        assert_eq!(r.feed(&OK_LEN[cut..]).unwrap(), Some(whole.clone()), "{cut}");
    }
}

#[test]
fn chunked_bodies_with_extensions_and_trailers() {
    let text = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n";
    let r = read_all(text).unwrap().unwrap();
    assert_eq!(r.body, b"hello world");
    for cut in 0..text.len() {
        let mut rd = ResponseReader::new(Limits::default());
        let first = rd.feed(&text[..cut]).unwrap();
        let got = first.or_else(|| rd.feed(&text[cut..]).unwrap()).unwrap();
        assert_eq!(got.body, b"hello world", "{cut}");
    }
    for bad in [
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"[..],
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabX\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n\r\n",
    ] {
        assert!(
            matches!(read_all(bad), Err(HttpError::Malformed(_))),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn bodies_until_close_interim_responses_and_bodiless_statuses() {
    let mut r = ResponseReader::new(Limits::default());
    assert_eq!(r.feed(b"HTTP/1.1 200 OK\r\n\r\nall of it").unwrap(), None);
    assert_eq!(r.feed(b", until the end").unwrap(), None);
    assert_eq!(r.finish().unwrap().body, b"all of it, until the end");
    let r = read_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\nX: 1\r\n\r\n")
        .unwrap()
        .unwrap();
    assert_eq!((r.status, r.body.len()), (204, 0));
    let r = read_all(b"HTTP/1.1 304 Not Modified\r\nContent-Length: 10\r\n\r\n").unwrap().unwrap();
    assert_eq!(r.status, 304);
    let mut head = ResponseReader::for_head(Limits::default());
    assert_eq!(
        head.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n").unwrap().map(|r| r.body.len()),
        Some(0)
    );
    // The connection closed before the body ended.
    let mut r = ResponseReader::new(Limits::default());
    assert_eq!(r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab").unwrap(), None);
    assert_eq!(r.finish(), Err(HttpError::Truncated));
    let mut r = ResponseReader::new(Limits::default());
    assert_eq!(r.finish(), Err(HttpError::Truncated));
}

#[test]
fn malformed_and_oversized_responses_are_refused() {
    for bad in [
        &b"HTTP/1.0x 200 OK\r\n\r\n"[..],
        b"HTTP/1.1 2000 OK\r\n\r\n",
        b"HTTP/1.1 abc OK\r\n\r\n",
        b"HTTP/2 200 OK\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nNo colon\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        b"HTTP/1.1 200 OK\nX: bare newline\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n folded: line\r\n\r\n",
    ] {
        assert!(
            matches!(read_all(bad), Err(HttpError::Malformed(_))),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    let small = Limits { max_head: 32, max_body: 4 };
    assert_eq!(
        ResponseReader::new(small)
            .feed(b"HTTP/1.1 200 OK\r\nX-Long: 0123456789012345678901234567890\r\n\r\n"),
        Err(HttpError::TooLarge("head"))
    );
    assert_eq!(
        ResponseReader::new(Limits { max_head: 64, ..small })
            .feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"),
        Err(HttpError::TooLarge("body"))
    );
    let mut r = ResponseReader::new(Limits { max_head: 64, ..small });
    assert_eq!(r.feed(b"HTTP/1.1 200 OK\r\n\r\n12345"), Err(HttpError::TooLarge("body")));
    let mut r = ResponseReader::new(Limits { max_head: 64, ..small });
    assert_eq!(
        r.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n"),
        Err(HttpError::TooLarge("body"))
    );
    // A response whose reading failed stays failed.
    let mut r = ResponseReader::new(Limits::default());
    assert!(r.feed(b"nonsense\r\n\r\n").is_err());
    assert!(r.feed(OK_LEN).is_err());
    for e in [
        HttpError::Malformed("x"),
        HttpError::TooLarge("x"),
        HttpError::Truncated,
        HttpError::BadRequest("x"),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn urls_and_the_clients_requests() {
    let u = Url::parse("https://sts.us-east-1.amazonaws.com").unwrap();
    assert_eq!(
        (u.scheme, u.host.as_str(), u.port, u.path_and_query.as_str()),
        (Scheme::Https, "sts.us-east-1.amazonaws.com", 443, "/")
    );
    assert_eq!(u.authority(), "sts.us-east-1.amazonaws.com");
    let u = Url::parse("http://127.0.0.1:8080/keys?x=1").unwrap();
    assert_eq!(
        (u.scheme, u.port, u.path_and_query.as_str(), u.authority().as_str()),
        (Scheme::Http, 8080, "/keys?x=1", "127.0.0.1:8080")
    );
    let u = Url::parse("https://[::1]:8443/jwks").unwrap();
    assert_eq!((u.host.as_str(), u.port, u.authority().as_str()), ("::1", 8443, "[::1]:8443"));
    for bad in [
        "ftp://x/",
        "https://",
        "https://h:x/",
        "https://h:99999/",
        "x",
        "https://a b/",
        "https://user@h/",
        "https://h#frag",
    ] {
        assert!(Url::parse(bad).is_err(), "{bad}");
    }
    let u = Url::parse("https://idp.example/.well-known/jwks.json").unwrap();
    let text = String::from_utf8(get(&u).encode().unwrap()).unwrap();
    assert!(
        text.starts_with("GET /.well-known/jwks.json HTTP/1.1\r\nHost: idp.example\r\n"),
        "{text}"
    );
    assert!(text.contains("Connection: close\r\n"));
    let text =
        String::from_utf8(post(&u, "application/x-www-form-urlencoded", b"a=1").encode().unwrap())
            .unwrap();
    assert!(
        text.contains(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: 3\r\n\r\na=1"
        ),
        "{text}"
    );
}

#[test]
fn edges_of_fields_lengths_and_lines() {
    // A tab in a value is allowed; a bad name next to Host is not; a given length is kept.
    let r = Request::new("GET", "/").header("Host", "h").header("X", "a\tb");
    assert!(r.encode().is_ok());
    let r = Request::new("GET", "/").header("Host", "h").header("Bad Name", "v");
    assert_eq!(r.encode(), Err(HttpError::BadRequest("field")));
    let r = Request::new("POST", "/")
        .header("Host", "h")
        .header("Content-Length", "2")
        .body(b"ab".to_vec());
    assert_eq!(
        r.encode().unwrap(),
        b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 2\r\n\r\nab".to_vec()
    );
    for bad in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999999999\r\n\r\n",
    ] {
        assert!(
            matches!(read_all(bad), Err(HttpError::Malformed(_))),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    let small = Limits { max_head: 64, max_body: 1024 };
    let mut r = ResponseReader::new(small);
    assert_eq!(r.feed(&[b'x'; 65]), Err(HttpError::TooLarge("head")));
    let mut r = ResponseReader::new(small);
    let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    chunked.extend_from_slice(&[b'1'; 65]);
    assert_eq!(r.feed(&chunked), Err(HttpError::TooLarge("chunk line")));
    for bad in ["https://:443/", "https://[]:1/"] {
        assert!(Url::parse(bad).is_err(), "{bad}");
    }
}

/// An HTTP/1.0 answer, as OpenSSL's OCSP responder sends (013 T042): read, its body until the
/// connection closes (RFC 9112, 2.6: a client takes any HTTP/1.x answer).
#[test]
fn http_1_0_answers_are_read() {
    let mut r = ResponseReader::new(Limits::default());
    assert_eq!(
        r.feed(b"HTTP/1.0 200 OK\r\nContent-Type: application/ocsp-response\r\n\r\nabc"),
        Ok(None)
    );
    let response = r.finish().unwrap();
    assert_eq!((response.status, response.body.as_slice()), (200, &b"abc"[..]));
}
