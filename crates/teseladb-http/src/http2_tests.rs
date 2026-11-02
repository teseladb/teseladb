#![cfg_attr(coverage, coverage(off))]

use super::*;

/// A frame the server wrote.
#[derive(Debug, PartialEq)]
struct Out {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

fn frames(mut bytes: &[u8]) -> Vec<Out> {
    let mut out = Vec::new();
    while bytes.len() >= FRAME_HEADER {
        let len =
            (usize::from(bytes[0]) << 16) | (usize::from(bytes[1]) << 8) | usize::from(bytes[2]);
        out.push(Out {
            kind: bytes[3],
            flags: bytes[4],
            stream: u32_at(&bytes[5..9]),
            payload: bytes[FRAME_HEADER..FRAME_HEADER + len].to_vec(),
        });
        bytes = &bytes[FRAME_HEADER + len..];
    }
    assert!(bytes.is_empty(), "a cut frame");
    out
}

fn raw(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    frame(&mut out, kind, flags, stream, payload);
    out
}

/// A client that has sent its preface and SETTINGS, and whose server answered.
struct Client {
    conn: Connection,
    enc: hpack::Encoder,
    dec: hpack::Decoder,
}

impl Client {
    fn new() -> Client {
        Client::with(Settings::default(), &[])
    }

    fn with(settings: Settings, client_settings: &[(u16, u32)]) -> Client {
        let mut conn = Connection::new(settings);
        let first = frames(&conn.take_output());
        assert_eq!(first[0].kind, kind::SETTINGS);
        let mut p = Vec::new();
        for (id, v) in client_settings {
            p.extend_from_slice(&id.to_be_bytes());
            p.extend_from_slice(&v.to_be_bytes());
        }
        let mut hello = PREFACE.to_vec();
        hello.extend(raw(kind::SETTINGS, 0, 0, &p));
        assert_eq!(conn.feed(&hello), []);
        let acked = frames(&conn.take_output());
        assert_eq!(
            acked,
            [Out { kind: kind::SETTINGS, flags: flag::ACK, stream: 0, payload: vec![] }]
        );
        Client { conn, enc: hpack::Encoder::new(), dec: hpack::Decoder::new(4096, 1 << 20) }
    }

    fn block(&mut self, fields: &[(&str, &str)]) -> Vec<u8> {
        let fields: Vec<Field> = fields.iter().map(|(n, v)| Field::new(*n, *v)).collect();
        self.enc.encode(&fields)
    }

    fn headers(&mut self, stream: u32, fields: &[(&str, &str)], end: bool) -> Vec<Event> {
        let block = self.block(fields);
        let flags = flag::END_HEADERS | if end { flag::END_STREAM } else { 0 };
        self.conn.feed(&raw(kind::HEADERS, flags, stream, &block))
    }

    fn get(&mut self, stream: u32, path: &str) -> Vec<Event> {
        self.headers(
            stream,
            &[
                (":method", "GET"),
                (":scheme", "https"),
                (":path", path),
                (":authority", "db.example"),
            ],
            true,
        )
    }

    fn out(&mut self) -> Vec<Out> {
        frames(&self.conn.take_output())
    }

    /// The fields of a HEADERS payload the server wrote.
    fn fields(&mut self, payload: &[u8]) -> Vec<(String, String)> {
        self.dec
            .decode(payload)
            .expect("block")
            .into_iter()
            .map(|f| (String::from_utf8(f.name).unwrap(), String::from_utf8(f.value).unwrap()))
            .collect()
    }
}

fn request(events: &[Event]) -> (u32, Request) {
    match events {
        [Event::Request { stream, request }] => (*stream, request.clone()),
        other => panic!("{other:?}"),
    }
}

fn failed(events: &[Event]) -> u32 {
    match events.last() {
        Some(Event::Failed { code }) => *code,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_server_announces_its_settings_first() {
    let mut conn = Connection::new(Settings::default());
    let out = frames(&conn.take_output());
    assert_eq!(out[0].kind, kind::SETTINGS);
    assert_eq!(out[0].payload.len(), 30);
    // The connection window grows to the stream window: 1 MiB less the default 65 535.
    assert_eq!(out[1].kind, kind::WINDOW_UPDATE);
    assert_eq!(u32_at(&out[1].payload), (1 << 20) - DEFAULT_WINDOW);
    let small = Settings { initial_window: DEFAULT_WINDOW, ..Settings::default() };
    assert_eq!(frames(&Connection::new(small).take_output()).len(), 1);
    assert_eq!(server_alpn(), [b"h2".to_vec(), b"http/1.1".to_vec()]);
}

#[test]
fn a_get_arrives_whole_and_its_answer_goes_back() {
    let mut c = Client::new();
    let (stream, req) = request(&c.get(1, "/v1/databases"));
    assert_eq!(stream, 1);
    assert_eq!(req.method, "GET");
    assert_eq!(req.target, "/v1/databases");
    assert_eq!(req.headers, [("host".to_owned(), "db.example".to_owned())]);
    c.conn
        .respond(
            1,
            200,
            &[
                ("Content-Type".into(), "application/json".into()),
                ("Connection".into(), "close".into()),
            ],
            b"[]",
        )
        .unwrap();
    let out = c.out();
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!((out[0].kind, out[0].flags), (kind::HEADERS, flag::END_HEADERS));
    let fields = c.fields(&out[0].payload.clone());
    assert_eq!(
        fields,
        [
            (":status".to_owned(), "200".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("content-length".to_owned(), "2".to_owned()),
        ]
    );
    assert_eq!(
        (out[1].kind, out[1].flags, out[1].payload.as_slice()),
        (kind::DATA, flag::END_STREAM, &b"[]"[..])
    );
    // The stream is gone: nothing more may be sent on it.
    assert_eq!(c.conn.send_data(1, b"x", true), Err(SendError::Closed));
    assert_eq!(c.conn.respond(1, 200, &[], b""), Err(SendError::Closed));
}

#[test]
fn a_post_body_comes_in_data_frames_with_padding_and_trailers() {
    let mut c = Client::new();
    assert_eq!(
        c.headers(
            3,
            &[(":method", "POST"), (":scheme", "https"), (":path", "/x"), ("content-length", "5")],
            false
        ),
        []
    );
    // A padded frame: one byte of pad length, the data, three bytes of padding.
    let mut padded = vec![3];
    padded.extend_from_slice(b"hel");
    padded.extend_from_slice(&[0, 0, 0]);
    assert_eq!(c.conn.feed(&raw(kind::DATA, flag::PADDED, 3, &padded)), []);
    let updates = c.out();
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(updates.iter().all(|f| f.kind == kind::WINDOW_UPDATE && u32_at(&f.payload) == 7));
    assert_eq!(c.conn.feed(&raw(kind::DATA, 0, 3, b"lo")), []);
    let block = c.block(&[("x-checksum", "1")]);
    let (_, req) =
        request(&c.conn.feed(&raw(kind::HEADERS, flag::END_HEADERS | flag::END_STREAM, 3, &block)));
    assert_eq!(req.body, b"hello");
    assert!(!req.headers.iter().any(|(n, _)| n == "host"));
}

#[test]
fn a_header_block_may_go_on_in_continuation_frames() {
    let mut c = Client::new();
    let block = c.block(&[
        (":method", "GET"),
        (":scheme", "https"),
        (":path", "/a"),
        ("x-long", &"v".repeat(300)),
    ]);
    let (a, b) = block.split_at(10);
    assert_eq!(c.conn.feed(&raw(kind::HEADERS, flag::END_STREAM, 1, a)), []);
    let (_, req) = request(&c.conn.feed(&raw(kind::CONTINUATION, flag::END_HEADERS, 1, b)));
    assert_eq!(req.headers[0].1.len(), 300);
    // Anything else in the middle of a block is a connection error.
    let mut c = Client::new();
    let block = c.block(&[(":method", "GET"), (":scheme", "https"), (":path", "/a")]);
    c.conn.feed(&raw(kind::HEADERS, flag::END_STREAM, 1, &block));
    assert_eq!(failed(&c.conn.feed(&raw(kind::PING, 0, 0, &[0; 8]))), code::PROTOCOL_ERROR);
    let goaway = c.out();
    assert_eq!(goaway[0].kind, kind::GOAWAY);
    assert_eq!(u32_at(&goaway[0].payload[4..]), code::PROTOCOL_ERROR);
    assert!(c.conn.is_closed());
    assert_eq!(c.conn.feed(b"more"), []);
    // A CONTINUATION nobody expects.
    let mut c = Client::new();
    assert_eq!(
        failed(&c.conn.feed(&raw(kind::CONTINUATION, flag::END_HEADERS, 1, &[0x82]))),
        code::PROTOCOL_ERROR
    );
}

#[test]
fn malformed_requests_reset_their_stream() {
    let cases: [&[(&str, &str)]; 9] = [
        &[(":method", "GET"), (":path", "/")],
        &[(":method", "GET"), (":scheme", "https"), (":path", "")],
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("Upper", "x")],
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("connection", "keep-alive")],
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("te", "gzip")],
        &[(":method", "GET"), ("x", "1"), (":scheme", "https"), (":path", "/")],
        &[(":method", "GET"), (":method", "GET"), (":scheme", "https"), (":path", "/")],
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), (":status", "200")],
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("content-length", "x")],
    ];
    for fields in cases {
        let mut c = Client::new();
        assert_eq!(c.headers(1, fields, true), [], "{fields:?}");
        let out = c.out();
        assert_eq!(out.len(), 1, "{fields:?}: {out:?}");
        assert_eq!(
            (out[0].kind, out[0].stream, u32_at(&out[0].payload)),
            (kind::RST_STREAM, 1, code::PROTOCOL_ERROR)
        );
    }
    // A body whose length is not the announced one.
    let mut c = Client::new();
    c.headers(
        1,
        &[(":method", "POST"), (":scheme", "https"), (":path", "/"), ("content-length", "4")],
        false,
    );
    assert_eq!(c.conn.feed(&raw(kind::DATA, flag::END_STREAM, 1, b"abc")), []);
    let out = c.out();
    assert_eq!(
        out.last().map(|f| (f.kind, u32_at(&f.payload))),
        Some((kind::RST_STREAM, code::PROTOCOL_ERROR))
    );
    // `te: trailers` is the one `te` allowed.
    let mut c = Client::new();
    let events = c.headers(
        1,
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("te", "trailers")],
        true,
    );
    assert_eq!(request(&events).0, 1);
}

#[test]
fn bodies_and_fields_past_their_limits_are_refused_with_their_status() {
    let settings = Settings { max_body: 4, max_header_list: 200, ..Settings::default() };
    let mut c = Client::with(settings, &[]);
    c.out();
    let ev = c.headers(
        1,
        &[(":method", "POST"), (":scheme", "https"), (":path", "/"), ("content-length", "5")],
        false,
    );
    assert_eq!(ev, [Event::Refused { stream: 1, status: 413 }]);
    let ev = c.headers(3, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    assert_eq!(ev, []);
    assert_eq!(
        c.conn.feed(&raw(kind::DATA, 0, 3, b"abcde")),
        [Event::Refused { stream: 3, status: 413 }]
    );
    // What follows is dropped.
    assert_eq!(c.conn.feed(&raw(kind::DATA, flag::END_STREAM, 3, b"f")), []);
    c.conn.respond(3, 413, &[], b"").unwrap();
    let ev = c.headers(
        5,
        &[(":method", "GET"), (":scheme", "https"), (":path", "/"), ("x-big", &"b".repeat(200))],
        true,
    );
    assert_eq!(ev, [Event::Refused { stream: 5, status: 431 }]);
    c.out();
    // Answered before the client ended its side: the stream is reset with NO_ERROR.
    c.conn.respond(1, 413, &[], b"").unwrap();
    let out = c.out();
    assert_eq!(
        out.last().map(|f| (f.kind, u32_at(&f.payload))),
        Some((kind::RST_STREAM, code::NO_ERROR))
    );
}

#[test]
fn answers_wait_for_the_flow_control_windows() {
    let mut c = Client::with(
        Settings::default(),
        &[(setting::INITIAL_WINDOW_SIZE, 10), (setting::MAX_FRAME_SIZE, 16_384)],
    );
    request(&c.get(1, "/"));
    c.conn.respond(1, 200, &[], &[7u8; 25]).unwrap();
    let out = c.out();
    assert_eq!(
        out.iter().filter(|f| f.kind == kind::DATA).map(|f| f.payload.len()).sum::<usize>(),
        10
    );
    assert_eq!(c.conn.pending(1), 15);
    c.conn.feed(&raw(kind::WINDOW_UPDATE, 0, 1, &12u32.to_be_bytes()));
    let out = c.out();
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].payload.len(), out[0].flags), (12, 0));
    // A larger initial window opens every stream.
    c.conn.feed(&raw(kind::SETTINGS, 0, 0, &[0, 4, 0, 0, 0, 100]));
    let out = c.out();
    assert_eq!(out[1].payload.len(), 3);
    assert_eq!(out[1].flags, flag::END_STREAM);
    assert_eq!(c.conn.pending(1), 0);
    // The connection window counts too: 65 535 for all streams.
    let mut c = Client::with(Settings::default(), &[(setting::INITIAL_WINDOW_SIZE, 1 << 20)]);
    request(&c.get(1, "/"));
    c.conn.respond(1, 200, &[], &vec![1u8; 70_000]).unwrap();
    let data: usize =
        c.out().iter().filter(|f| f.kind == kind::DATA).map(|f| f.payload.len()).sum();
    assert_eq!(data, 65_535);
    c.conn.feed(&raw(kind::WINDOW_UPDATE, 0, 0, &10_000u32.to_be_bytes()));
    let last = c.out();
    assert_eq!(last.iter().map(|f| f.payload.len()).sum::<usize>(), 70_000 - 65_535);
    assert_eq!(last.last().map(|f| f.flags), Some(flag::END_STREAM));
}

#[test]
fn streamed_answers_send_headers_then_data() {
    let mut c = Client::new();
    request(&c.get(1, "/events"));
    c.conn
        .send_headers(1, 200, &[("content-type".into(), "text/event-stream".into())], false)
        .unwrap();
    c.conn.send_data(1, b"data: 1\n\n", false).unwrap();
    c.conn.send_data(1, b"", true).unwrap();
    let out = c.out();
    let kinds: Vec<(u8, u8)> = out.iter().map(|f| (f.kind, f.flags)).collect();
    assert_eq!(
        kinds,
        [(kind::HEADERS, flag::END_HEADERS), (kind::DATA, 0), (kind::DATA, flag::END_STREAM)]
    );
    // A long block of fields goes in CONTINUATION frames of the peer's frame size.
    request(&c.get(3, "/"));
    let big: Vec<(String, String)> =
        (0..40).map(|i| (format!("x-{i}"), "v".repeat(1000))).collect();
    c.conn.send_headers(3, 200, &big, true).unwrap();
    let out = c.out();
    assert_eq!((out[0].kind, out[0].flags), (kind::HEADERS, flag::END_STREAM));
    assert!(out[1..].iter().all(|f| f.kind == kind::CONTINUATION));
    assert_eq!(out.last().map(|f| f.flags), Some(flag::END_HEADERS));
    assert_eq!(c.conn.send_headers(9, 200, &[], true), Err(SendError::Closed));
}

#[test]
fn ping_settings_goaway_and_reset() {
    let mut c = Client::new();
    c.conn.feed(&raw(kind::PING, 0, 0, b"12345678"));
    assert_eq!(
        c.out(),
        [Out { kind: kind::PING, flags: flag::ACK, stream: 0, payload: b"12345678".to_vec() }]
    );
    assert_eq!(c.conn.feed(&raw(kind::PING, flag::ACK, 0, b"12345678")), []);
    assert_eq!(c.conn.feed(&raw(kind::SETTINGS, flag::ACK, 0, &[])), []);
    assert_eq!(c.conn.feed(&raw(0x42, 0, 0, b"unknown")), []);
    assert_eq!(c.conn.feed(&raw(kind::PRIORITY, 0, 1, &[0, 0, 0, 0, 7])), []);
    c.headers(1, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    assert_eq!(
        c.conn.feed(&raw(kind::RST_STREAM, 0, 1, &code::CANCEL.to_be_bytes())),
        [Event::Reset { stream: 1, code: code::CANCEL }]
    );
    // Data on the reset stream: STREAM_CLOSED.
    c.out();
    c.conn.feed(&raw(kind::DATA, 0, 1, b"x"));
    let out = c.out();
    assert_eq!(
        out.last().map(|f| (f.kind, u32_at(&f.payload))),
        Some((kind::RST_STREAM, code::STREAM_CLOSED))
    );
    let mut away = 1u32.to_be_bytes().to_vec();
    away.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(c.conn.feed(&raw(kind::GOAWAY, 0, 0, &away)), [Event::GoAway { code: 0 }]);
    assert!(c.conn.is_closed());
    // After a GOAWAY new streams are ignored.
    assert_eq!(c.get(5, "/"), []);
    let mut s = Client::new();
    s.conn.go_away();
    assert_eq!(s.out()[0].kind, kind::GOAWAY);
}

#[test]
fn too_many_streams_are_refused() {
    let mut c = Client::with(Settings { max_concurrent_streams: 1, ..Settings::default() }, &[]);
    c.out();
    c.headers(1, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    c.headers(3, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    let out = c.out();
    assert_eq!(out.last().map(|f| (f.stream, u32_at(&f.payload))), Some((3, code::REFUSED_STREAM)));
    // The refused block was still decoded: the next one reads with the same table.
    assert_eq!(c.conn.feed(&raw(kind::RST_STREAM, 0, 1, &0u32.to_be_bytes())).len(), 1);
    assert_eq!(request(&c.get(5, "/")).0, 5);
}

#[test]
fn protocol_violations_end_the_connection() {
    let bad: Vec<(Vec<u8>, u32)> = vec![
        (raw(kind::HEADERS, flag::END_HEADERS, 2, &[0x82]), code::PROTOCOL_ERROR),
        (raw(kind::DATA, 0, 0, b""), code::PROTOCOL_ERROR),
        (raw(kind::DATA, 0, 9, b"x"), code::PROTOCOL_ERROR),
        (raw(kind::PRIORITY, 0, 0, &[0; 5]), code::PROTOCOL_ERROR),
        (raw(kind::RST_STREAM, 0, 1, &[0; 3]), code::FRAME_SIZE_ERROR),
        (raw(kind::RST_STREAM, 0, 7, &[0; 4]), code::PROTOCOL_ERROR),
        (raw(kind::SETTINGS, 0, 1, &[]), code::PROTOCOL_ERROR),
        (raw(kind::SETTINGS, flag::ACK, 0, &[0; 6]), code::FRAME_SIZE_ERROR),
        (raw(kind::SETTINGS, 0, 0, &[0; 5]), code::FRAME_SIZE_ERROR),
        (raw(kind::SETTINGS, 0, 0, &[0, 2, 0, 0, 0, 2]), code::PROTOCOL_ERROR),
        (raw(kind::SETTINGS, 0, 0, &[0, 4, 0x80, 0, 0, 0]), code::FLOW_CONTROL_ERROR),
        (raw(kind::SETTINGS, 0, 0, &[0, 5, 0, 0, 0, 1]), code::PROTOCOL_ERROR),
        (raw(kind::PUSH_PROMISE, flag::END_HEADERS, 1, &[0, 0, 0, 2]), code::PROTOCOL_ERROR),
        (raw(kind::PING, 0, 1, &[0; 8]), code::PROTOCOL_ERROR),
        (raw(kind::PING, 0, 0, &[0; 7]), code::FRAME_SIZE_ERROR),
        (raw(kind::GOAWAY, 0, 1, &[0; 8]), code::PROTOCOL_ERROR),
        (raw(kind::WINDOW_UPDATE, 0, 0, &[0; 4]), code::PROTOCOL_ERROR),
        (raw(kind::WINDOW_UPDATE, 0, 0, &[0; 3]), code::FRAME_SIZE_ERROR),
        (raw(kind::WINDOW_UPDATE, 0, 0, &0x7fff_ffffu32.to_be_bytes()), code::FLOW_CONTROL_ERROR),
        (raw(kind::WINDOW_UPDATE, 0, 9, &1u32.to_be_bytes()), code::PROTOCOL_ERROR),
        (raw(kind::HEADERS, flag::END_HEADERS, 1, &[0xbe]), code::COMPRESSION_ERROR),
        (raw(kind::HEADERS, flag::END_HEADERS | flag::PADDED, 1, &[9, 0x82]), code::PROTOCOL_ERROR),
        (
            raw(kind::HEADERS, flag::END_HEADERS | flag::PRIORITY, 1, &[0, 0]),
            code::FRAME_SIZE_ERROR,
        ),
        (raw(kind::DATA, flag::PADDED, 1, &[]), code::FRAME_SIZE_ERROR),
        (raw(kind::DATA, 0, 1, &vec![0; 20_000]), code::FRAME_SIZE_ERROR),
    ];
    for (frame, want) in bad {
        let mut c = Client::new();
        // A stream 1 open, for the frames that need one.
        c.headers(1, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
        assert_eq!(failed(&c.conn.feed(&frame)), want, "{frame:?}");
    }
    // A preface that is not HTTP/2, and a first frame that is not SETTINGS.
    let mut conn = Connection::new(Settings::default());
    assert_eq!(failed(&conn.feed(b"GET / HTTP/1.1\r\n")), code::PROTOCOL_ERROR);
    let mut conn = Connection::new(Settings::default());
    assert_eq!(conn.feed(&PREFACE[..10]), []);
    let mut rest = PREFACE[10..].to_vec();
    rest.extend(raw(kind::PING, 0, 0, &[0; 8]));
    assert_eq!(failed(&conn.feed(&rest)), code::PROTOCOL_ERROR);
    // A stream id lower than one already used, and data past the window.
    let mut c = Client::new();
    request(&c.get(5, "/"));
    assert_eq!(failed(&c.get(3, "/")), code::PROTOCOL_ERROR);
    let mut c = Client::with(
        Settings { initial_window: DEFAULT_WINDOW, max_frame_size: 1 << 20, ..Settings::default() },
        &[],
    );
    c.headers(1, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    assert_eq!(
        failed(&c.conn.feed(&raw(kind::DATA, 0, 1, &vec![0; 70_000]))),
        code::FLOW_CONTROL_ERROR
    );
}

#[test]
fn stream_level_errors_reset_only_their_stream() {
    let mut c = Client::new();
    c.headers(1, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    c.out();
    c.conn.feed(&raw(kind::WINDOW_UPDATE, 0, 1, &0u32.to_be_bytes()));
    assert_eq!(c.out()[0].kind, kind::RST_STREAM);
    c.headers(3, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    c.out();
    c.conn.feed(&raw(kind::WINDOW_UPDATE, 0, 3, &0x7fff_ffffu32.to_be_bytes()));
    assert_eq!(c.out().last().map(|f| u32_at(&f.payload)), Some(code::FLOW_CONTROL_ERROR));
    c.conn.feed(&raw(kind::PRIORITY, 0, 5, &[0; 4]));
    assert_eq!(c.out().last().map(|f| u32_at(&f.payload)), Some(code::FRAME_SIZE_ERROR));
    // A stream that depends on itself.
    let block = c.block(&[(":method", "GET"), (":scheme", "https"), (":path", "/")]);
    let mut p = 7u32.to_be_bytes().to_vec();
    p.push(16);
    p.extend_from_slice(&block);
    assert_eq!(
        c.conn.feed(&raw(
            kind::HEADERS,
            flag::END_HEADERS | flag::END_STREAM | flag::PRIORITY,
            7,
            &p
        )),
        []
    );
    assert_eq!(
        c.out().last().map(|f| (f.stream, u32_at(&f.payload))),
        Some((7, code::PROTOCOL_ERROR))
    );
    // Trailers without END_STREAM, or HEADERS on a stream the client ended, end the connection.
    c.headers(9, &[(":method", "POST"), (":scheme", "https"), (":path", "/")], false);
    let block = c.block(&[("x-t", "1")]);
    assert_eq!(
        failed(&c.conn.feed(&raw(kind::HEADERS, flag::END_HEADERS, 9, &block))),
        code::PROTOCOL_ERROR
    );
    // A WINDOW_UPDATE of a closed stream is ignored.
    let mut c = Client::new();
    request(&c.get(1, "/"));
    c.conn.respond(1, 204, &[], b"").unwrap();
    assert_eq!(c.conn.feed(&raw(kind::WINDOW_UPDATE, 0, 1, &1u32.to_be_bytes())), []);
    assert_eq!(c.conn.feed(&raw(kind::RST_STREAM, 0, 1, &0u32.to_be_bytes())), []);
}

#[test]
fn the_peer_table_size_setting_reaches_the_encoder() {
    let mut c = Client::with(Settings::default(), &[(setting::HEADER_TABLE_SIZE, 0)]);
    request(&c.get(1, "/"));
    c.conn.respond(1, 200, &[("x-a".into(), "1".into())], b"").unwrap();
    let out = c.out();
    // A size update to 0 opens the block.
    assert_eq!(out[0].payload[0], 0x20);
}

#[test]
fn the_protocol_comes_from_alpn_or_the_preface() {
    assert_eq!(negotiated(Some(b"h2")), Protocol::Http2);
    assert_eq!(negotiated(Some(b"http/1.1")), Protocol::Http1);
    assert_eq!(negotiated(None), Protocol::Http1);
    assert_eq!(sniff(b""), None);
    assert_eq!(sniff(b"PRI * HTTP/2"), None);
    assert_eq!(sniff(PREFACE), Some(Protocol::Http2));
    assert_eq!(sniff(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0"), Some(Protocol::Http2));
    assert_eq!(sniff(b"GET / HTTP/1.1"), Some(Protocol::Http1));
    assert_eq!(sniff(b"P"), None);
    assert_eq!(sniff(b"PUT"), Some(Protocol::Http1));
}
