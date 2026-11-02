//! One connection of the HTTP port without I/O (019 FR-001, FR-003): HTTP/1.1 or HTTP/2, as ALPN
//! chose or, without TLS, as the first bytes say, behind one interface. Requests come out of
//! [`Conn::feed`] with an id; each is answered whole ([`Conn::respond`]) or as a stream of bytes
//! ([`Conn::start_stream`], for Server-Sent Events). HTTP/1.1 answers go out in the order of their
//! requests, a stream in chunks; a request the protocol layer refuses (`413`, `431`, a malformed
//! one) is answered here with its problem document (RFC 9457). The connection keeps the time of
//! its activity for the deadlines of [`crate::limits`].

use std::collections::{BTreeMap, VecDeque};

use crate::http1::{Limits, Request};
use crate::http2::{self, Event as H2Event, Protocol};
use crate::limits;
use crate::server1::{RequestReader, SERVER_LIMITS, encode_response, reason_phrase};

/// The media type of a problem document (RFC 9457).
pub const PROBLEM_JSON: &str = "application/problem+json";

/// What a connection gives its server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// A request to answer under `id`.
    Request {
        /// What the answer names.
        id: u32,
        /// The request; `host` holds the `:authority` of an HTTP/2 one.
        request: Request,
    },
    /// The client gave up the request `id` (an HTTP/2 reset): what runs for it stops.
    Cancelled {
        /// The request.
        id: u32,
    },
}

/// A whole answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Status.
    pub status: u16,
    /// Fields, without those of the framing.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl Response {
    /// The problem document of a status without a RefDb error (RFC 9457, section 4.2.1).
    #[must_use]
    pub fn problem(status: u16) -> Response {
        let body = format!(
            "{{\"type\":\"about:blank\",\"title\":\"{}\",\"status\":{status}}}",
            reason_phrase(status)
        );
        Response {
            status,
            headers: vec![("Content-Type".to_owned(), PROBLEM_JSON.to_owned())],
            body: body.into_bytes(),
        }
    }
}

/// The settings of the connections of the port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// HTTP/1.1 limits.
    pub http1: Limits,
    /// HTTP/2 settings.
    pub http2: http2::Settings,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { http1: SERVER_LIMITS, http2: http2::Settings::default() }
    }
}

/// An HTTP/1.1 answer waiting for those before it.
#[derive(Debug, Default)]
struct Slot {
    bytes: Vec<u8>,
    done: bool,
    streaming: bool,
    close: bool,
}

#[derive(Debug)]
enum Wire {
    /// Without TLS, before the first bytes tell.
    Undecided(Vec<u8>),
    Http1 {
        reader: RequestReader,
        order: VecDeque<u32>,
        slots: BTreeMap<u32, Slot>,
    },
    Http2(Box<http2::Connection>),
}

/// One connection.
#[derive(Debug)]
pub struct Conn {
    settings: Settings,
    wire: Wire,
    out: Vec<u8>,
    next_id: u32,
    last_activity_ms: u64,
    partial_since_ms: Option<u64>,
    closing: bool,
}

impl Conn {
    /// A connection that speaks `protocol`, or that finds out from its first bytes (`None`,
    /// without TLS), opened at `now_ms`.
    #[must_use]
    pub fn new(protocol: Option<Protocol>, settings: Settings, now_ms: u64) -> Conn {
        let mut c = Conn {
            settings,
            wire: Wire::Undecided(Vec::new()),
            out: Vec::new(),
            next_id: 1,
            last_activity_ms: now_ms,
            partial_since_ms: None,
            closing: false,
        };
        if let Some(p) = protocol {
            c.start(p);
        }
        c
    }

    fn start(&mut self, p: Protocol) {
        self.wire = match p {
            Protocol::Http1 => Wire::Http1 {
                reader: RequestReader::new(self.settings.http1),
                order: VecDeque::new(),
                slots: BTreeMap::new(),
            },
            Protocol::Http2 => {
                let mut h2 = http2::Connection::new(self.settings.http2);
                self.out.extend(h2.take_output());
                Wire::Http2(Box::new(h2))
            }
        };
    }

    /// The protocol, once known.
    #[must_use]
    pub fn protocol(&self) -> Option<Protocol> {
        match self.wire {
            Wire::Undecided(_) => None,
            Wire::Http1 { .. } => Some(Protocol::Http1),
            Wire::Http2(_) => Some(Protocol::Http2),
        }
    }

    /// Takes `bytes` that arrived at `now_ms`; the requests they complete.
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Incoming> {
        self.last_activity_ms = now_ms;
        if self.closing {
            return Vec::new();
        }
        let bytes = match &mut self.wire {
            Wire::Undecided(buf) => {
                buf.extend_from_slice(bytes);
                let Some(p) = http2::sniff(buf) else { return Vec::new() };
                let first = std::mem::take(buf);
                self.start(p);
                first
            }
            _ => bytes.to_vec(),
        };
        let found = match &mut self.wire {
            Wire::Http2(h2) => {
                let events = h2.feed(&bytes);
                self.out.extend(h2.take_output());
                self.take_h2(events)
            }
            Wire::Http1 { .. } => self.take_h1(&bytes),
            Wire::Undecided(_) => Vec::new(),
        };
        self.partial_since_ms = match &self.wire {
            Wire::Http1 { reader, .. } if reader.is_partial() => {
                Some(self.partial_since_ms.unwrap_or(now_ms))
            }
            _ => None,
        };
        found
    }

    fn take_h2(&mut self, events: Vec<H2Event>) -> Vec<Incoming> {
        let mut found = Vec::new();
        for e in events {
            match e {
                H2Event::Request { stream, request } => {
                    found.push(Incoming::Request { id: stream, request });
                }
                H2Event::Refused { stream, status } => {
                    self.respond(stream, Response::problem(status))
                }
                H2Event::Reset { stream, .. } => found.push(Incoming::Cancelled { id: stream }),
                H2Event::GoAway { .. } => {}
                H2Event::Failed { .. } => self.closing = true,
            }
        }
        found
    }

    fn take_h1(&mut self, bytes: &[u8]) -> Vec<Incoming> {
        let mut found = Vec::new();
        let mut first = Some(bytes);
        while let Wire::Http1 { reader, order, slots } = &mut self.wire {
            let next = match first.take() {
                Some(b) => reader.feed(b),
                None => reader.next_request(),
            };
            let id = self.next_id;
            match next {
                Ok(Some(incoming)) => {
                    self.next_id += 1;
                    order.push_back(id);
                    slots.insert(id, Slot { close: !incoming.keep_alive, ..Slot::default() });
                    found.push(Incoming::Request { id, request: incoming.request });
                    if !incoming.keep_alive {
                        // Nothing after a request that closes the connection is read.
                        self.closing = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(refused) => {
                    self.next_id += 1;
                    order.push_back(id);
                    slots.insert(id, Slot { close: true, ..Slot::default() });
                    self.respond(id, Response::problem(refused.status));
                    self.closing = true;
                    break;
                }
            }
        }
        found
    }

    /// Answers request `id` whole. An id the connection no longer has is ignored (its client
    /// reset it or went away).
    pub fn respond(&mut self, id: u32, r: Response) {
        match &mut self.wire {
            Wire::Http2(h2) => {
                // A reset stream has nobody to answer.
                let _closed = h2.respond(id, r.status, &r.headers, &r.body);
                self.out.extend(h2.take_output());
            }
            Wire::Http1 { slots, .. } => {
                if let Some(s) = slots.get_mut(&id) {
                    let mut headers = r.headers;
                    if s.close {
                        headers.push(("Connection".to_owned(), "close".to_owned()));
                    }
                    s.bytes.extend(encode_response(r.status, &headers, &r.body));
                    s.done = true;
                }
                self.flush_h1();
            }
            Wire::Undecided(_) => {}
        }
    }

    /// Starts the answer of `id` as a stream: its status and fields now, its body in pieces.
    pub fn start_stream(&mut self, id: u32, status: u16, headers: &[(String, String)]) {
        match &mut self.wire {
            Wire::Http2(h2) => {
                let _closed = h2.send_headers(id, status, headers, false);
                self.out.extend(h2.take_output());
            }
            Wire::Http1 { slots, .. } => {
                if let Some(s) = slots.get_mut(&id) {
                    let mut head = format!("HTTP/1.1 {status:03} {}\r\n", reason_phrase(status));
                    for (n, v) in headers {
                        head.push_str(&format!("{n}: {v}\r\n"));
                    }
                    head.push_str("Transfer-Encoding: chunked\r\n");
                    if s.close {
                        head.push_str("Connection: close\r\n");
                    }
                    head.push_str("\r\n");
                    s.bytes.extend(head.into_bytes());
                    s.streaming = true;
                }
                self.flush_h1();
            }
            Wire::Undecided(_) => {}
        }
    }

    /// More of the body of the stream `id`.
    pub fn stream(&mut self, id: u32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match &mut self.wire {
            Wire::Http2(h2) => {
                let _closed = h2.send_data(id, bytes, false);
                self.out.extend(h2.take_output());
            }
            Wire::Http1 { slots, .. } => {
                if let Some(s) = slots.get_mut(&id).filter(|s| s.streaming) {
                    s.bytes.extend(format!("{:x}\r\n", bytes.len()).into_bytes());
                    s.bytes.extend_from_slice(bytes);
                    s.bytes.extend_from_slice(b"\r\n");
                }
                self.flush_h1();
            }
            Wire::Undecided(_) => {}
        }
    }

    /// Ends the stream `id`.
    pub fn end_stream(&mut self, id: u32) {
        match &mut self.wire {
            Wire::Http2(h2) => {
                let _closed = h2.send_data(id, &[], true);
                self.out.extend(h2.take_output());
            }
            Wire::Http1 { slots, .. } => {
                if let Some(s) = slots.get_mut(&id).filter(|s| s.streaming) {
                    s.bytes.extend_from_slice(b"0\r\n\r\n");
                    s.done = true;
                }
                self.flush_h1();
            }
            Wire::Undecided(_) => {}
        }
    }

    /// Writes the HTTP/1.1 answers whose turn it is: whole ones, and what a stream has so far.
    fn flush_h1(&mut self) {
        let Wire::Http1 { order, slots, .. } = &mut self.wire else { return };
        while let Some(id) = order.front().copied() {
            let Some(s) = slots.get_mut(&id) else {
                order.pop_front();
                continue;
            };
            self.out.append(&mut s.bytes);
            if !s.done {
                break;
            }
            if s.close {
                self.closing = true;
            }
            order.pop_front();
            slots.remove(&id);
        }
    }

    /// The bytes to write.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// When the connection closes if nothing happens ([`crate::limits::deadline_ms`]).
    #[must_use]
    pub fn deadline_ms(&self) -> u64 {
        limits::deadline_ms(self.last_activity_ms, self.partial_since_ms)
    }

    /// Notes activity at `now_ms` that is not a read (an answer written, a stream that moved).
    pub fn touch(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }

    /// Whether the socket closes once the output is written: the client asked for it, an error
    /// ended the connection, or HTTP/2 ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        match &self.wire {
            Wire::Http2(h2) => self.closing || h2.is_closed(),
            Wire::Http1 { order, .. } => self.closing && order.is_empty(),
            Wire::Undecided(_) => self.closing,
        }
    }

    /// Ends the connection: HTTP/2 says GOAWAY, HTTP/1.1 closes after the answers it owes.
    pub fn close(&mut self) {
        if let Wire::Http2(h2) = &mut self.wire {
            h2.go_away();
            self.out.extend(h2.take_output());
        }
        self.closing = true;
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
