//! HTTP/2 of a server (RFC 9113) without I/O (019 FR-001, FR-003): the connection preface, the
//! frames, the streams of the requests and their flow control, with the fields compressed by
//! HPACK ([`crate::hpack`]). Bytes go in with [`Connection::feed`], which gives the requests
//! whole, and the answers come out of [`Connection::take_output`]. TLS chooses `h2` with ALPN
//! ([`ALPN_H2`]); without TLS a client starts with the preface (prior knowledge).

use std::collections::{BTreeMap, VecDeque};

use crate::hpack::{self, Field, HpackError};
use crate::http1::Request;
use crate::server1::{MAX_REQUEST_BODY, MAX_REQUEST_HEAD};

/// The ALPN name of HTTP/2 over TLS (RFC 9113, section 3.2).
pub const ALPN_H2: &[u8] = b"h2";
/// The ALPN name of HTTP/1.1.
pub const ALPN_HTTP11: &[u8] = b"http/1.1";
/// The client's connection preface (RFC 9113, section 3.4).
pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The ALPN protocols of the HTTP port, in order of preference.
#[must_use]
pub fn server_alpn() -> Vec<Vec<u8>> {
    vec![ALPN_H2.to_vec(), ALPN_HTTP11.to_vec()]
}

/// The protocol of a connection of the HTTP port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/1.1 ([`crate::server1`]).
    Http1,
    /// HTTP/2 ([`Connection`]).
    Http2,
}

/// The protocol TLS agreed by ALPN: `h2`, or HTTP/1.1 for anything else and without ALPN
/// (RFC 9113, section 3.2).
#[must_use]
pub fn negotiated(alpn: Option<&[u8]>) -> Protocol {
    if alpn == Some(ALPN_H2) { Protocol::Http2 } else { Protocol::Http1 }
}

/// The protocol of a connection without TLS from its first bytes: HTTP/2 when they open with the
/// preface (prior knowledge, RFC 9113 section 3.3); `None` while they could still be it.
#[must_use]
pub fn sniff(first: &[u8]) -> Option<Protocol> {
    let n = first.len().min(PREFACE.len());
    if first[..n] != PREFACE[..n] {
        return Some(Protocol::Http1);
    }
    (n == PREFACE.len()).then_some(Protocol::Http2)
}

use crate::http2_frames::{CONNECTION_FIELDS, request_of};
pub use crate::http2_frames::{code, flag, kind, setting};

/// The bytes of a frame header.
pub const FRAME_HEADER: usize = 9;
/// The smallest and default `SETTINGS_MAX_FRAME_SIZE`.
pub const DEFAULT_FRAME_SIZE: u32 = 16_384;
/// The largest `SETTINGS_MAX_FRAME_SIZE`.
const LARGEST_FRAME_SIZE: u32 = (1 << 24) - 1;
/// The window of a connection and of a stream before any setting (RFC 9113, section 6.9.2).
pub const DEFAULT_WINDOW: u32 = 65_535;
/// The largest flow-control window.
const MAX_WINDOW: i64 = (1 << 31) - 1;

/// What the server announces and enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Streams a client may have open at once.
    pub max_concurrent_streams: u32,
    /// The window of each stream the client sends on.
    pub initial_window: u32,
    /// The largest frame the server reads.
    pub max_frame_size: u32,
    /// The largest list of fields of a request (`SETTINGS_MAX_HEADER_LIST_SIZE`).
    pub max_header_list: u32,
    /// The largest body of a request.
    pub max_body: usize,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            max_concurrent_streams: 100,
            initial_window: 1 << 20,
            max_frame_size: DEFAULT_FRAME_SIZE,
            max_header_list: u32::try_from(MAX_REQUEST_HEAD).unwrap_or(u32::MAX),
            max_body: MAX_REQUEST_BODY,
        }
    }
}

/// What a connection gives its server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A whole request on `stream`, with its `:authority` as the field `host`.
    Request {
        /// The stream that answers it.
        stream: u32,
        /// The request.
        request: Request,
    },
    /// A request the server answers with `status` without running it (`413` for a body past
    /// its limit, `431` for fields past theirs).
    Refused {
        /// The stream.
        stream: u32,
        /// The status of the answer.
        status: u16,
    },
    /// The client reset `stream` with `code`: what runs for it stops.
    Reset {
        /// The stream.
        stream: u32,
        /// The error code.
        code: u32,
    },
    /// The client goes away: no new streams.
    GoAway {
        /// The error code.
        code: u32,
    },
    /// The connection failed with `code`; a GOAWAY is in the output and nothing more is read.
    Failed {
        /// The error code.
        code: u32,
    },
}

/// Why an answer cannot be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// The stream is not open for sending (unknown, reset, or already ended).
    Closed,
}

#[derive(Debug, Default)]
struct Stream {
    /// The client ended its side.
    remote_done: bool,
    /// The server sent its last frame (or queued it).
    local_done: bool,
    /// The fields arrived.
    request: Option<Request>,
    /// The announced length of the body.
    content_length: Option<usize>,
    /// The client sent more than the limit: the rest is dropped.
    refused: bool,
    /// What the server may still send on it.
    send_window: i64,
    /// Data waiting for window, and whether the stream ends after it.
    pending: VecDeque<u8>,
    end_after: bool,
}

/// A connection error: the code of its GOAWAY.
type ConnError = u32;

/// One HTTP/2 connection of a server.
#[derive(Debug)]
pub struct Connection {
    settings: Settings,
    buf: Vec<u8>,
    preface: bool,
    got_settings: bool,
    decoder: hpack::Decoder,
    encoder: hpack::Encoder,
    peer_window: u32,
    peer_frame_size: u32,
    send_window: i64,
    recv_window: i64,
    streams: BTreeMap<u32, Stream>,
    last_stream: u32,
    /// The stream whose header block goes on in CONTINUATION frames, and the block so far.
    continuing: Option<(u32, u8, bool, Vec<u8>)>,
    out: Vec<u8>,
    failed: bool,
    peer_away: bool,
}

/// A frame read from the connection.
struct Frame<'a> {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: &'a [u8],
}

/// Writes a frame.
fn frame(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
}

fn u32_at(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Connection {
    /// A connection whose server announces `settings`; its SETTINGS frame is already in the
    /// output.
    #[must_use]
    pub fn new(settings: Settings) -> Connection {
        let mut c = Connection {
            settings,
            buf: Vec::new(),
            preface: false,
            got_settings: false,
            decoder: hpack::Decoder::new(
                hpack::DEFAULT_TABLE_SIZE,
                usize::try_from(settings.max_header_list).unwrap_or(usize::MAX),
            ),
            encoder: hpack::Encoder::new(),
            peer_window: DEFAULT_WINDOW,
            peer_frame_size: DEFAULT_FRAME_SIZE,
            send_window: i64::from(DEFAULT_WINDOW),
            recv_window: i64::from(DEFAULT_WINDOW),
            streams: BTreeMap::new(),
            last_stream: 0,
            continuing: None,
            out: Vec::new(),
            failed: false,
            peer_away: false,
        };
        let mut p = Vec::new();
        for (id, v) in [
            (setting::MAX_CONCURRENT_STREAMS, settings.max_concurrent_streams),
            (setting::INITIAL_WINDOW_SIZE, settings.initial_window),
            (setting::MAX_FRAME_SIZE, settings.max_frame_size),
            (setting::MAX_HEADER_LIST_SIZE, settings.max_header_list),
            (setting::ENABLE_PUSH, 0),
        ] {
            p.extend_from_slice(&id.to_be_bytes());
            p.extend_from_slice(&v.to_be_bytes());
        }
        frame(&mut c.out, kind::SETTINGS, 0, 0, &p);
        // The connection's window grows to the stream window the server announces.
        let more = settings.initial_window.saturating_sub(DEFAULT_WINDOW);
        if more > 0 {
            frame(&mut c.out, kind::WINDOW_UPDATE, 0, 0, &more.to_be_bytes());
            c.recv_window += i64::from(more);
        }
        c
    }

    /// Whether the connection failed or both sides are done: the socket may close once the
    /// output is written.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.failed || (self.peer_away && self.streams.is_empty())
    }

    /// The bytes to write.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Takes `bytes` from the client; what they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        if self.failed {
            return events;
        }
        self.buf.extend_from_slice(bytes);
        if let Err(code) = self.read_frames(&mut events) {
            self.fail(code);
            events.push(Event::Failed { code });
        }
        events
    }

    fn fail(&mut self, code: u32) {
        let mut p = self.last_stream.to_be_bytes().to_vec();
        p.extend_from_slice(&code.to_be_bytes());
        frame(&mut self.out, kind::GOAWAY, 0, 0, &p);
        self.failed = true;
        self.buf.clear();
    }

    /// Says goodbye: the client opens no more streams, and the open ones finish.
    pub fn go_away(&mut self) {
        let mut p = self.last_stream.to_be_bytes().to_vec();
        p.extend_from_slice(&code::NO_ERROR.to_be_bytes());
        frame(&mut self.out, kind::GOAWAY, 0, 0, &p);
        self.peer_away = true;
    }

    fn read_frames(&mut self, events: &mut Vec<Event>) -> Result<(), ConnError> {
        if !self.preface {
            let n = self.buf.len().min(PREFACE.len());
            if self.buf[..n] != PREFACE[..n] {
                return Err(code::PROTOCOL_ERROR);
            }
            if n < PREFACE.len() {
                return Ok(());
            }
            self.buf.drain(..PREFACE.len());
            self.preface = true;
        }
        let mut at = 0;
        let result = loop {
            let Some(head) = self.buf.get(at..at + FRAME_HEADER) else { break Ok(()) };
            let len =
                (usize::from(head[0]) << 16) | (usize::from(head[1]) << 8) | usize::from(head[2]);
            let (k, flags, stream) = (head[3], head[4], u32_at(&head[5..9]) & 0x7fff_ffff);
            if len > self.settings.max_frame_size as usize {
                break Err(code::FRAME_SIZE_ERROR);
            }
            let end = at + FRAME_HEADER + len;
            if self.buf.len() < end {
                break Ok(());
            }
            let payload = self.buf[at + FRAME_HEADER..end].to_vec();
            at = end;
            let f = Frame { kind: k, flags, stream, payload: &payload };
            if let Err(e) = self.on_frame(&f, events) {
                break Err(e);
            }
        };
        self.buf.drain(..at.min(self.buf.len()));
        result
    }

    fn on_frame(&mut self, f: &Frame<'_>, events: &mut Vec<Event>) -> Result<(), ConnError> {
        if !self.got_settings && f.kind != kind::SETTINGS {
            return Err(code::PROTOCOL_ERROR);
        }
        if let Some((id, _, _, _)) = &self.continuing
            && (f.kind != kind::CONTINUATION || f.stream != *id)
        {
            return Err(code::PROTOCOL_ERROR);
        }
        match f.kind {
            kind::DATA => self.on_data(f, events),
            kind::HEADERS => self.on_headers(f, events),
            kind::PRIORITY => self.on_priority(f),
            kind::RST_STREAM => self.on_reset(f, events),
            kind::SETTINGS => self.on_settings(f),
            kind::PING => self.on_ping(f),
            kind::GOAWAY => {
                if f.stream != 0 || f.payload.len() < 8 {
                    return Err(code::PROTOCOL_ERROR);
                }
                self.peer_away = true;
                events.push(Event::GoAway { code: u32_at(&f.payload[4..8]) });
                Ok(())
            }
            kind::WINDOW_UPDATE => self.on_window_update(f),
            kind::CONTINUATION => self.on_continuation(f, events),
            // A client never pushes (RFC 9113, section 8.4).
            kind::PUSH_PROMISE => Err(code::PROTOCOL_ERROR),
            // Unknown types are ignored (section 4.1).
            _ => Ok(()),
        }
    }

    /// The payload of `f` without its padding.
    fn unpadded<'a>(f: &Frame<'a>) -> Result<&'a [u8], ConnError> {
        if f.flags & flag::PADDED == 0 {
            return Ok(f.payload);
        }
        let (&pad, rest) = f.payload.split_first().ok_or(code::FRAME_SIZE_ERROR)?;
        let keep = rest.len().checked_sub(usize::from(pad)).ok_or(code::PROTOCOL_ERROR)?;
        Ok(&rest[..keep])
    }

    fn reset(&mut self, stream: u32, error: u32) {
        frame(&mut self.out, kind::RST_STREAM, 0, stream, &error.to_be_bytes());
        self.streams.remove(&stream);
    }

    /// The state of a stream the client did not open: idle (a connection error) or closed.
    fn unknown(&self, stream: u32) -> Result<(), ConnError> {
        if stream == 0 || stream > self.last_stream { Err(code::PROTOCOL_ERROR) } else { Ok(()) }
    }

    fn on_data(&mut self, f: &Frame<'_>, events: &mut Vec<Event>) -> Result<(), ConnError> {
        let size = i64::try_from(f.payload.len()).unwrap_or(MAX_WINDOW);
        if size > self.recv_window {
            return Err(code::FLOW_CONTROL_ERROR);
        }
        // The window comes back at once: the server reads each body whole.
        if size > 0 {
            frame(&mut self.out, kind::WINDOW_UPDATE, 0, 0, &(size as u32).to_be_bytes());
        }
        let data = Self::unpadded(f)?;
        let max_body = self.settings.max_body;
        let Some(s) = self.streams.get_mut(&f.stream).filter(|s| !s.remote_done) else {
            self.unknown(f.stream)?;
            self.reset(f.stream, code::STREAM_CLOSED);
            return Ok(());
        };
        let end = f.flags & flag::END_STREAM != 0;
        if !end && size > 0 {
            frame(&mut self.out, kind::WINDOW_UPDATE, 0, f.stream, &(size as u32).to_be_bytes());
        }
        s.remote_done = end;
        if s.refused {
            return Ok(());
        }
        let Some(req) = s.request.as_mut() else { return Ok(()) };
        if req.body.len() + data.len() > max_body {
            s.refused = true;
            req.body = Vec::new();
            events.push(Event::Refused { stream: f.stream, status: 413 });
            return Ok(());
        }
        req.body.extend_from_slice(data);
        if end {
            self.complete(f.stream, events);
        }
        Ok(())
    }

    /// Hands over the request of `stream`, whose client side ended, if its body has the length
    /// it announced (RFC 9113, section 8.1.1).
    fn complete(&mut self, stream: u32, events: &mut Vec<Event>) {
        let Some(s) = self.streams.get_mut(&stream) else { return };
        let Some(request) = s.request.take() else { return };
        if s.content_length.is_some_and(|n| n != request.body.len()) {
            self.reset(stream, code::PROTOCOL_ERROR);
            return;
        }
        events.push(Event::Request { stream, request });
    }

    fn on_headers(&mut self, f: &Frame<'_>, events: &mut Vec<Event>) -> Result<(), ConnError> {
        if f.stream == 0 || f.stream.is_multiple_of(2) {
            return Err(code::PROTOCOL_ERROR);
        }
        let mut block = Self::unpadded(f)?;
        // A stream that depends on itself is a stream error (RFC 9113, section 5.3.1).
        let mut self_dependent = false;
        if f.flags & flag::PRIORITY != 0 {
            if block.len() < 5 {
                return Err(code::FRAME_SIZE_ERROR);
            }
            self_dependent = u32_at(block) & 0x7fff_ffff == f.stream;
            block = &block[5..];
        }
        if f.flags & flag::END_HEADERS == 0 {
            self.continuing = Some((f.stream, f.flags, self_dependent, block.to_vec()));
            return Ok(());
        }
        self.on_block(f.stream, (f.flags, self_dependent), block, events)
    }

    fn on_continuation(&mut self, f: &Frame<'_>, events: &mut Vec<Event>) -> Result<(), ConnError> {
        let Some((stream, flags, self_dependent, mut block)) = self.continuing.take() else {
            return Err(code::PROTOCOL_ERROR);
        };
        block.extend_from_slice(f.payload);
        if block.len() > self.settings.max_header_list as usize * 2 {
            // A block that never ends is a way to exhaust memory.
            return Err(code::PROTOCOL_ERROR);
        }
        if f.flags & flag::END_HEADERS == 0 {
            self.continuing = Some((stream, flags, self_dependent, block));
            return Ok(());
        }
        self.on_block(stream, (flags, self_dependent), &block, events)
    }

    /// A whole header block on `stream`: a new request, or the trailers of one.
    fn on_block(
        &mut self,
        stream: u32,
        (flags, self_dependent): (u8, bool),
        block: &[u8],
        events: &mut Vec<Event>,
    ) -> Result<(), ConnError> {
        // The block is always decoded: the table must follow it even for a refused stream.
        let decoded = self.decoder.decode(block);
        let fields = match decoded {
            Ok(fields) => Some(fields),
            Err(HpackError::ListTooLarge) => None,
            Err(_) => return Err(code::COMPRESSION_ERROR),
        };
        let end = flags & flag::END_STREAM != 0;
        if let Some(s) = self.streams.get_mut(&stream) {
            // Trailers: they end the stream, and the server does not read them.
            if s.remote_done || !end {
                return Err(code::PROTOCOL_ERROR);
            }
            s.remote_done = true;
            if !s.refused {
                self.complete(stream, events);
            }
            return Ok(());
        }
        if stream <= self.last_stream {
            return Err(code::PROTOCOL_ERROR);
        }
        self.last_stream = stream;
        if self_dependent {
            self.reset(stream, code::PROTOCOL_ERROR);
            return Ok(());
        }
        if self.peer_away {
            return Ok(());
        }
        let open = self.streams.values().filter(|s| !s.remote_done || !s.local_done).count();
        if open >= self.settings.max_concurrent_streams as usize {
            self.reset(stream, code::REFUSED_STREAM);
            return Ok(());
        }
        let mut s = Stream {
            remote_done: end,
            send_window: i64::from(self.peer_window),
            ..Stream::default()
        };
        let Some(fields) = fields else {
            s.refused = true;
            self.streams.insert(stream, s);
            events.push(Event::Refused { stream, status: 431 });
            return Ok(());
        };
        match request_of(&fields) {
            Ok((request, length)) => {
                s.content_length = length;
                if length.is_some_and(|n| n > self.settings.max_body) {
                    s.refused = true;
                    self.streams.insert(stream, s);
                    events.push(Event::Refused { stream, status: 413 });
                    return Ok(());
                }
                s.request = Some(request);
                self.streams.insert(stream, s);
                if end {
                    self.complete(stream, events);
                }
            }
            Err(()) => {
                self.streams.insert(stream, s);
                self.reset(stream, code::PROTOCOL_ERROR);
            }
        }
        Ok(())
    }

    fn on_priority(&mut self, f: &Frame<'_>) -> Result<(), ConnError> {
        if f.stream == 0 {
            return Err(code::PROTOCOL_ERROR);
        }
        if f.payload.len() != 5 {
            self.reset(f.stream, code::FRAME_SIZE_ERROR);
        }
        Ok(())
    }

    fn on_reset(&mut self, f: &Frame<'_>, events: &mut Vec<Event>) -> Result<(), ConnError> {
        if f.payload.len() != 4 {
            return Err(code::FRAME_SIZE_ERROR);
        }
        self.unknown(f.stream)?;
        if self.streams.remove(&f.stream).is_some() {
            events.push(Event::Reset { stream: f.stream, code: u32_at(f.payload) });
        }
        Ok(())
    }

    fn on_settings(&mut self, f: &Frame<'_>) -> Result<(), ConnError> {
        if f.stream != 0 {
            return Err(code::PROTOCOL_ERROR);
        }
        if f.flags & flag::ACK != 0 {
            return if f.payload.is_empty() { Ok(()) } else { Err(code::FRAME_SIZE_ERROR) };
        }
        if !f.payload.len().is_multiple_of(6) {
            return Err(code::FRAME_SIZE_ERROR);
        }
        self.got_settings = true;
        for item in f.payload.chunks(6) {
            let (id, v) = (u16::from_be_bytes([item[0], item[1]]), u32_at(&item[2..]));
            match id {
                setting::HEADER_TABLE_SIZE => {
                    self.encoder.set_max_table_size(usize::try_from(v).unwrap_or(usize::MAX));
                }
                setting::ENABLE_PUSH if v > 1 => return Err(code::PROTOCOL_ERROR),
                setting::INITIAL_WINDOW_SIZE => {
                    if i64::from(v) > MAX_WINDOW {
                        return Err(code::FLOW_CONTROL_ERROR);
                    }
                    let delta = i64::from(v) - i64::from(self.peer_window);
                    for s in self.streams.values_mut() {
                        s.send_window += delta;
                        if s.send_window > MAX_WINDOW {
                            return Err(code::FLOW_CONTROL_ERROR);
                        }
                    }
                    self.peer_window = v;
                }
                setting::MAX_FRAME_SIZE => {
                    if !(DEFAULT_FRAME_SIZE..=LARGEST_FRAME_SIZE).contains(&v) {
                        return Err(code::PROTOCOL_ERROR);
                    }
                    self.peer_frame_size = v;
                }
                _ => {}
            }
        }
        frame(&mut self.out, kind::SETTINGS, flag::ACK, 0, &[]);
        self.flush();
        Ok(())
    }

    fn on_ping(&mut self, f: &Frame<'_>) -> Result<(), ConnError> {
        if f.stream != 0 {
            return Err(code::PROTOCOL_ERROR);
        }
        if f.payload.len() != 8 {
            return Err(code::FRAME_SIZE_ERROR);
        }
        if f.flags & flag::ACK == 0 {
            frame(&mut self.out, kind::PING, flag::ACK, 0, f.payload);
        }
        Ok(())
    }

    fn on_window_update(&mut self, f: &Frame<'_>) -> Result<(), ConnError> {
        if f.payload.len() != 4 {
            return Err(code::FRAME_SIZE_ERROR);
        }
        let more = i64::from(u32_at(f.payload) & 0x7fff_ffff);
        if f.stream == 0 {
            if more == 0 {
                return Err(code::PROTOCOL_ERROR);
            }
            self.send_window += more;
            if self.send_window > MAX_WINDOW {
                return Err(code::FLOW_CONTROL_ERROR);
            }
        } else if let Some(s) = self.streams.get_mut(&f.stream) {
            if more == 0 {
                self.reset(f.stream, code::PROTOCOL_ERROR);
                return Ok(());
            }
            s.send_window += more;
            if s.send_window > MAX_WINDOW {
                self.reset(f.stream, code::FLOW_CONTROL_ERROR);
                return Ok(());
            }
        } else {
            self.unknown(f.stream)?;
        }
        self.flush();
        Ok(())
    }

    /// Sends the status and fields of the answer on `stream`; with `end`, the answer has no
    /// body. Field names go in lower case and the fields of HTTP/1.1 connections are left out.
    pub fn send_headers(
        &mut self,
        stream: u32,
        status: u16,
        headers: &[(String, String)],
        end: bool,
    ) -> Result<(), SendError> {
        if !self.streams.get(&stream).is_some_and(|s| !s.local_done) {
            return Err(SendError::Closed);
        }
        let mut fields = vec![Field::new(":status", status.to_string())];
        for (n, v) in headers {
            let name = n.to_ascii_lowercase();
            if CONNECTION_FIELDS.contains(&name.as_str()) {
                continue;
            }
            fields.push(Field::new(name, v.as_bytes()));
        }
        let block = self.encoder.encode(&fields);
        let max = self.peer_frame_size as usize;
        let mut chunks = block.chunks(max).peekable();
        let mut first = true;
        let end_flag = if end { flag::END_STREAM } else { 0 };
        loop {
            let chunk = chunks.next().unwrap_or(&[]);
            let last = chunks.peek().is_none();
            let headers_end = if last { flag::END_HEADERS } else { 0 };
            if first {
                frame(&mut self.out, kind::HEADERS, end_flag | headers_end, stream, chunk);
                first = false;
            } else {
                frame(&mut self.out, kind::CONTINUATION, headers_end, stream, chunk);
            }
            if last {
                break;
            }
        }
        if end {
            self.finish(stream);
        }
        Ok(())
    }

    /// Sends `data` of the body of the answer on `stream`, as the windows allow; with `end`, the
    /// answer ends after it.
    pub fn send_data(&mut self, stream: u32, data: &[u8], end: bool) -> Result<(), SendError> {
        let Some(s) = self.streams.get_mut(&stream).filter(|s| !s.local_done && !s.end_after)
        else {
            return Err(SendError::Closed);
        };
        s.pending.extend(data);
        s.end_after = end;
        self.flush();
        Ok(())
    }

    /// A whole answer: status, fields and body.
    pub fn respond(
        &mut self,
        stream: u32,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(), SendError> {
        if body.is_empty() {
            return self.send_headers(stream, status, headers, true);
        }
        let mut all = headers.to_vec();
        if !all.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-length")) {
            all.push(("content-length".to_owned(), body.len().to_string()));
        }
        self.send_headers(stream, status, &all, false)?;
        self.send_data(stream, body, true)
    }

    /// Bytes the answer of `stream` still holds back for want of window.
    #[must_use]
    pub fn pending(&self, stream: u32) -> usize {
        self.streams.get(&stream).map_or(0, |s| s.pending.len())
    }

    /// The server's side of `stream` ended: once the client's has too, it goes; if the client
    /// is still sending, it is told to stop (RFC 9113, section 8.1).
    fn finish(&mut self, stream: u32) {
        let Some(s) = self.streams.get_mut(&stream) else { return };
        s.local_done = true;
        if s.remote_done {
            self.streams.remove(&stream);
        } else {
            self.reset(stream, code::NO_ERROR);
        }
    }

    /// Writes the data the windows allow.
    fn flush(&mut self) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            let max = i64::from(self.peer_frame_size);
            let mut done = false;
            if let Some(s) = self.streams.get_mut(&id) {
                while !s.pending.is_empty() {
                    let n = max.min(s.send_window).min(self.send_window);
                    if n <= 0 {
                        break;
                    }
                    let n = usize::try_from(n).unwrap_or(0).min(s.pending.len());
                    let chunk: Vec<u8> = s.pending.drain(..n).collect();
                    s.send_window -= n as i64;
                    self.send_window -= n as i64;
                    let last = s.pending.is_empty() && s.end_after;
                    let f = if last { flag::END_STREAM } else { 0 };
                    frame(&mut self.out, kind::DATA, f, id, &chunk);
                    done = last;
                }
                if s.pending.is_empty() && s.end_after && !done && !s.local_done {
                    frame(&mut self.out, kind::DATA, flag::END_STREAM, id, &[]);
                    done = true;
                }
            }
            if done {
                self.finish(id);
            }
        }
    }
}

#[cfg(test)]
#[path = "http2_tests.rs"]
mod tests;
