//! Change streams of the REST API as Server-Sent Events (019 T019, FR-012): an event per change
//! with its resume token as `id`, a client that drops the connection and comes back with
//! `Last-Event-ID` gets every later change once, the heartbeat comment, and the streams of a
//! database and of the cluster.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_http::sse::{Event, HEARTBEAT, Reader};
use teseladb_raftlog::MemDisk;
use teseladb_rest::changestream::HEARTBEAT_MS;
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};

const READY: Duration = Duration::from_secs(30);
const WAIT: Duration = Duration::from_secs(30);

fn start() -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let client = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        ..ClientConfig::new(Vec::new())
    };
    let cfg = NodeConfig {
        cores: Some(2),
        http: Some(http),
        client: Some(client),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn call(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, Document) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let req = Request::new(method, target).header("Host", "h").body(body.as_bytes().to_vec());
    s.write_all(&req.encode().unwrap()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            let doc = extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default();
            return (r.status, doc);
        }
    }
}

/// An open event stream: the socket, the status line and fields, and the body's chunks read so
/// far, decoded into events and raw text.
struct Stream {
    s: TcpStream,
    head: String,
    raw: Vec<u8>,
    chunked: Vec<u8>,
    reader: Reader,
    events: Vec<Event>,
    /// Whether the last chunk came: the server ended the stream.
    ended: bool,
}

impl Stream {
    fn open(
        addr: SocketAddr,
        method: &str,
        target: &str,
        body: &str,
        last: Option<&str>,
    ) -> Stream {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut req = Request::new(method, target).header("Host", "h");
        if let Some(id) = last {
            req = req.header("Last-Event-ID", id);
        }
        let req = req.body(body.as_bytes().to_vec());
        s.write_all(&req.encode().unwrap()).unwrap();
        let mut st = Stream {
            s,
            head: String::new(),
            raw: Vec::new(),
            chunked: Vec::new(),
            reader: Reader::default(),
            events: Vec::new(),
            ended: false,
        };
        let end = Instant::now() + WAIT;
        while st.head.is_empty() {
            assert!(Instant::now() < end, "no answer");
            st.read();
        }
        st
    }

    /// Reads what came, if anything.
    fn read(&mut self) {
        let mut buf = [0u8; 65536];
        let n = match self.s.read(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return;
            }
            Err(e) => panic!("{e}"),
        };
        self.chunked.extend_from_slice(&buf[..n]);
        if self.head.is_empty() {
            let Some(at) = self.chunked.windows(4).position(|w| w == b"\r\n\r\n") else { return };
            self.head = String::from_utf8_lossy(&self.chunked[..at]).into_owned();
            self.chunked.drain(..at + 4);
        }
        // Chunks: `<hex size>\r\n<data>\r\n`.
        loop {
            let Some(line) = self.chunked.windows(2).position(|w| w == b"\r\n") else { return };
            let size = std::str::from_utf8(&self.chunked[..line]).unwrap();
            let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
            if self.chunked.len() < line + 2 + size + 2 {
                return;
            }
            let data: Vec<u8> = self.chunked[line + 2..line + 2 + size].to_vec();
            self.chunked.drain(..line + 2 + size + 2);
            self.ended |= size == 0;
            self.raw.extend_from_slice(&data);
            let evs = self.reader.feed(&data);
            self.events.extend(evs);
        }
    }

    /// Reads until `n` events came.
    fn until_events(&mut self, n: usize) {
        let end = Instant::now() + WAIT;
        while self.events.len() < n {
            assert!(Instant::now() < end, "{} of {n} events: {:?}", self.events.len(), self.events);
            self.read();
        }
    }

    /// Reads until the server ends the stream.
    fn until_ended(&mut self) {
        let end = Instant::now() + WAIT;
        while !self.ended {
            assert!(Instant::now() < end, "the stream did not end: {:?}", self.events);
            self.read();
        }
    }
}

/// The `_id` of the document each `insert` event carries.
fn inserted(events: &[Event]) -> Vec<i32> {
    events
        .iter()
        .map(|e| {
            let d = extjson::parse(&e.data).unwrap();
            let full = d.get("fullDocument").and_then(Value::as_document).unwrap();
            match full.get("_id") {
                Some(Value::Int32(n)) => *n,
                other => panic!("{other:?}"),
            }
        })
        .collect()
}

fn insert(at: SocketAddr, db: &str, coll: &str, ids: std::ops::Range<i32>) {
    let docs: Vec<String> = ids.map(|k| format!("{{\"_id\":{k}}}")).collect();
    let target = format!("/v1/databases/{db}/collections/{coll}/insertMany");
    let (st, d) = call(at, "POST", &target, &format!("{{\"documents\":[{}]}}", docs.join(",")));
    assert_eq!(st, 200, "{d:?}");
}

#[test]
fn a_dropped_stream_resumes_after_its_last_event_without_losing_or_repeating_a_change() {
    let node = start();
    let at = node.http_addrs()[0];
    let (st, d) = call(at, "POST", "/v1/databases/app/collections", r#"{"name":"c"}"#);
    assert_eq!(st, 200, "{d:?}");
    let watch = "/v1/databases/app/collections/c/changeStream";
    let mut first = Stream::open(at, "GET", watch, "", None);
    assert!(first.head.starts_with("HTTP/1.1 200"), "{}", first.head);
    assert!(first.head.to_ascii_lowercase().contains("content-type: text/event-stream"));
    insert(at, "app", "c", 0..5);
    first.until_events(5);
    assert_eq!(inserted(&first.events), [0, 1, 2, 3, 4]);
    // The client drops the connection after the third event and comes back with its id.
    let last = first.events[2].id.clone().expect("an id");
    drop(first);
    insert(at, "app", "c", 5..8);
    let mut again = Stream::open(at, "GET", watch, "", Some(&last));
    again.until_events(5);
    assert_eq!(inserted(&again.events), [3, 4, 5, 6, 7]);
    // `POST` takes the options and a pipeline in the body.
    let body = r#"{"pipeline":[{"$match":{"fullDocument._id":{"$gte":100}}}],"fullDocument":"updateLookup"}"#;
    let mut filtered = Stream::open(at, "POST", watch, body, None);
    insert(at, "app", "c", 98..102);
    filtered.until_events(2);
    assert_eq!(inserted(&filtered.events), [100, 101]);
    // A database's stream and the cluster's see other collections too.
    let mut db = Stream::open(at, "GET", "/v1/databases/app/changeStream", "", None);
    let mut all = Stream::open(at, "GET", "/v1/changeStream", "", None);
    let (st, d) = call(at, "POST", "/v1/databases/app/collections", r#"{"name":"other"}"#);
    assert_eq!(st, 200, "{d:?}");
    insert(at, "app", "other", 200..201);
    db.until_events(1);
    all.until_events(1);
    assert_eq!(inserted(&db.events[db.events.len() - 1..]), [200]);
    // Without changes, the heartbeat comment keeps the connection busy.
    let quiet = Instant::now();
    let end = quiet + Duration::from_millis(HEARTBEAT_MS) + WAIT;
    while !again.raw.windows(HEARTBEAT.len()).any(|w| w == HEARTBEAT) {
        assert!(
            Instant::now() < end,
            "no heartbeat: {:?} {}",
            again.events.len(),
            String::from_utf8_lossy(&again.raw[again.raw.len().saturating_sub(300)..])
        );
        again.read();
    }
    assert!(quiet.elapsed() >= Duration::from_millis(HEARTBEAT_MS - 2_000));
    // Bad options and a bad Last-Event-ID are problems, not streams.
    let (st, _) = call(at, "GET", &format!("{watch}?nothing=1"), "");
    assert_eq!(st, 400);
    let bad = Stream::open(at, "GET", watch, "", Some("not json"));
    assert!(bad.head.starts_with("HTTP/1.1 400"), "{}", bad.head);
    let (st, _) =
        call(at, "GET", "/v1/databases/app/collections/c/changeStream?fullDocument=bogus", "");
    assert_eq!(st, 400);
    drop(node.stop());
}

/// A stream ends when its cursor does: dropping the collection sends `invalidate` and the last
/// chunk; a change the stream's pipeline cannot process ends it with an `error` event; and a
/// client that goes away has its cursor closed.
#[test]
fn a_stream_ends_with_its_cursor_or_with_an_error_event() {
    let node = start();
    let at = node.http_addrs()[0];
    for name in ["gone", "bad"] {
        let (st, d) =
            call(at, "POST", "/v1/databases/app/collections", &format!("{{\"name\":\"{name}\"}}"));
        assert_eq!(st, 200, "{d:?}");
    }
    let mut gone =
        Stream::open(at, "GET", "/v1/databases/app/collections/gone/changeStream", "", None);
    assert!(gone.head.starts_with("HTTP/1.1 200"), "{}", gone.head);
    insert(at, "app", "gone", 0..1);
    gone.until_events(1);
    let (st, d) = call(at, "DELETE", "/v1/databases/app/collections/gone", "");
    assert_eq!(st, 200, "{d:?}");
    gone.until_ended();
    let kinds: Vec<String> = gone
        .events
        .iter()
        .map(|e| {
            let d = extjson::parse(&e.data).unwrap();
            match d.get("operationType") {
                Some(Value::String(s)) => s.clone(),
                other => format!("{other:?}"),
            }
        })
        .collect();
    assert_eq!(kinds.last().map(String::as_str), Some("invalidate"), "{kinds:?}");

    let body = r#"{"pipeline":[{"$addFields":{"q":{"$divide":[1,"$fullDocument.zero"]}}}]}"#;
    let mut bad =
        Stream::open(at, "POST", "/v1/databases/app/collections/bad/changeStream", body, None);
    assert!(bad.head.starts_with("HTTP/1.1 200"), "{}", bad.head);
    let (st, d) = call(
        at,
        "POST",
        "/v1/databases/app/collections/bad/insertOne",
        r#"{"document":{"_id":1,"zero":0}}"#,
    );
    assert_eq!(st, 200, "{d:?}");
    bad.until_ended();
    let last = bad.events.last().expect("an event");
    assert_eq!(last.event.as_deref(), Some("error"), "{:?}", bad.events);

    // A client that goes away: the stream sees it after its wait and kills its cursor, which
    // `serverStatus` counts.
    let watch = "/v1/databases/app/collections/c/changeStream";
    let (st, d) = call(at, "POST", "/v1/databases/app/collections", r#"{"name":"c"}"#);
    assert_eq!(st, 200, "{d:?}");
    let kills = |at| {
        let (st, d) = call(at, "POST", "/v1/databases/admin/command", r#"{"serverStatus":1}"#);
        assert_eq!(st, 200);
        let path = ["metrics", "commands", "killCursors"];
        let k = path.iter().try_fold(&d, |d, k| d.get(k).and_then(Value::as_document));
        match k.and_then(|o| o.get("total")) {
            Some(Value::Int32(n)) => i64::from(*n),
            Some(Value::Int64(n)) => *n,
            other => panic!("killCursors.total is {other:?}"),
        }
    };
    let before = kills(at);
    let open = Stream::open(at, "GET", watch, "", None);
    assert!(open.head.starts_with("HTTP/1.1 200"), "{}", open.head);
    drop(open);
    let end = Instant::now() + WAIT;
    while kills(at) == before {
        assert!(Instant::now() < end, "the stream's cursor was never killed");
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(node.stop());
}
